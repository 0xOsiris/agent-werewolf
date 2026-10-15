//! Staked games: the escrow game behind each lobby, checkpoints, proposals and settlement. See
//! `docs/SETTLEMENT.md`.

mod ledger;
mod secrets;
mod settle;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use rand::rngs::SysError;
use tokio::time::Instant;
use tracing::{error, warn};
use url::Url;
use werewolf_chain::{
    ChainClient, ChainConfig, ChainError, bindings::IWerewolfEscrow::CreateParams,
};
use werewolf_engine::EngineError;
use werewolf_protocol::{Receipt, StakeInfo, StakeJoin};
use werewolf_verify::{
    alloy_primitives::{Address, B256, B512, Bytes, TxHash, U256, keccak256},
    encode::{self, Domain},
    force, keys,
    transcript::{BuildError, OpEntry, Rules, Seat},
};

#[cfg(test)]
pub(crate) use ledger::tests as ledger_tests;
pub use ledger::{Ledger, Signed};
pub use settle::settle;

use crate::config::StakeConfig;
use secrets::Secrets;

/// How long `start` waits for the randomness provider.
const RANDOMNESS_WAIT: Duration = Duration::from_secs(300);
const RANDOMNESS_POLL: Duration = Duration::from_secs(1);
/// Re-reads of a lagging RPC before a consistency check fails.
const CONSISTENCY_ATTEMPTS: u32 = 6;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(8);

#[derive(Debug, thiserror::Error)]
pub enum StakeError {
    #[error(transparent)]
    Chain(#[from] ChainError),
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("OS RNG failed: {0}")]
    Rng(#[from] SysError),
    #[error("{op}: chain reads still disagree with the committed hash after {attempts} attempts")]
    Inconsistent { op: &'static str, attempts: u32 },
    #[error("{0}")]
    Invalid(String),
    #[error("prover: {0}")]
    Prover(String),
    #[error("cannot deal: {0}")]
    Engine(#[from] EngineError),
    #[error("transcript: {0}")]
    Build(#[from] BuildError),
    #[error("encoding: {0}")]
    Json(#[from] serde_json::Error),
}

impl StakeError {
    /// Failure class, attempts and transaction, for structured logs.
    pub fn fields(&self) -> (&'static str, Option<u32>, Option<TxHash>) {
        match self {
            Self::Chain(ChainError::Rpc { attempts, .. }) => ("rpc", Some(*attempts), None),
            Self::Chain(ChainError::Timeout { attempts, tx, .. }) => {
                ("timeout", Some(*attempts), *tx)
            }
            Self::Chain(ChainError::Revert { tx, .. }) => ("revert", None, *tx),
            Self::Chain(ChainError::InvalidInput(_)) => ("invalid_input", None, None),
            Self::Chain(ChainError::Unexpected { .. }) => ("unexpected", None, None),
            Self::Io { .. } => ("io", None, None),
            Self::Rng(_) => ("rng", None, None),
            Self::Inconsistent { attempts, .. } => ("inconsistent", Some(*attempts), None),
            Self::Invalid(_) => ("invalid", None, None),
            Self::Prover(_) => ("prover", None, None),
            Self::Engine(_) | Self::Build(_) | Self::Json(_) => ("internal", None, None),
        }
    }

    /// Whether another attempt might succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Chain(err) if err.is_retryable())
    }

    /// A revert whose decoded reason names `error`.
    pub fn is_revert(&self, error: &str) -> bool {
        matches!(self, Self::Chain(ChainError::Revert { reason, .. }) if reason.contains(error))
    }
}

/// A failed step of a staked game, named for logs and the spectator feed.
#[derive(Debug, thiserror::Error)]
#[error("{op} failed: {source}")]
pub struct Failure {
    pub op: &'static str,
    #[source]
    pub source: StakeError,
}

impl Failure {
    /// `game` is `None` when `create` failed.
    pub fn log(&self, game: Option<U256>) {
        let (class, attempts, tx) = self.source.fields();
        error!(?game, op = self.op, class, ?attempts, ?tx, err = %self.source, "staked game step failed");
    }

    /// What spectators see; chain errors stay in the operator's logs.
    pub fn public_message(&self, game: U256) -> String {
        let (class, ..) = self.source.fields();
        format!(
            "staked game {game} stopped: {} failed ({class}); deadlines settle it on-chain as cancelled or void",
            self.op
        )
    }
}

/// Names the step a `StakeError` came from.
pub trait During<T> {
    fn during(self, op: &'static str) -> Result<T, Failure>;
}

impl<T, E: Into<StakeError>> During<T> for Result<T, E> {
    fn during(self, op: &'static str) -> Result<T, Failure> {
        self.map_err(|err| Failure {
            op,
            source: err.into(),
        })
    }
}

/// The operator's chain connection and staked-mode settings.
pub struct Stake {
    pub config: StakeConfig,
    pub client: ChainClient,
    pub operator: Address,
}

/// One escrow game this server created, with the secrets it committed to.
pub struct OnChain {
    pub id: U256,
    pub domain: Domain,
    pub rules: Rules,
    /// Escrow terms as `/v1/info` shows them.
    pub info: StakeInfo,
    pub join_by: u64,
    /// Block of `GameCreated`; log queries start here.
    pub created_block: u64,
    pub transcript_path: PathBuf,
    /// The prover's `--enc-secret-file`, written when a challenge comes in.
    pub enc_secret_path: PathBuf,
    secrets: Secrets,
}

impl OnChain {
    pub fn seed(&self) -> B256 {
        self.secrets.seed
    }

    pub fn enc_secret(&self) -> &[u8; 32] {
        &self.secrets.enc_secret
    }

    pub fn enc_key(&self) -> B256 {
        force::public_key(&self.secrets.enc_secret)
    }
}

/// What `start` fixed on-chain.
pub struct Started {
    /// The roster, in seat order.
    pub seats: Vec<Seat>,
    pub randomness: B256,
    pub roster_hash: B256,
    pub cursor: OpCursor,
}

/// The op log hash and block of the last checkpoint, or of `start` before any.
#[derive(Clone, Copy, Debug)]
pub struct OpCursor {
    pub hash: B256,
    pub block: u64,
}

/// A forced move as the contract logged it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Force {
    pub seat: u32,
    pub payload: Bytes,
}

impl Stake {
    /// Connects to the RPC, checks the chain id and escrow, and prepares the secrets directory.
    pub async fn connect(config: StakeConfig) -> Result<Self, StakeError> {
        let chain = ChainConfig::new(
            config.rpc_url.clone(),
            config.chain_id,
            config.escrow,
            Some(config.operator_key_file.clone()),
        );
        let client = ChainClient::connect(chain).await?;
        let operator = client.address().ok_or_else(|| {
            StakeError::Invalid("the operator key file gave no signer".to_owned())
        })?;
        secrets::ensure_dir(&config.dir).await?;
        Ok(Self {
            config,
            client,
            operator,
        })
    }

    /// Persists fresh secrets, then creates the escrow game for a new lobby.
    pub async fn create(
        &self,
        rules: Rules,
        min_seats: u8,
        max_seats: u8,
    ) -> Result<OnChain, StakeError> {
        let config = &self.config;
        let secrets = Secrets::generate()?;
        let pending =
            secrets::write_pending(&config.dir, config.chain_id, config.escrow, &secrets).await?;
        let now = self.client.latest_timestamp().await?;
        let join_by = now.saturating_add(config.join_window_secs);
        let play_by = join_by.saturating_add(config.play_window_secs);
        let params = CreateParams {
            token: config.token,
            stake: config.stake,
            minSeats: min_seats,
            maxSeats: max_seats,
            joinBy: join_by,
            playBy: play_by,
            rulesHash: encode::rules_hash(&rules),
            seedCommit: keccak256(secrets.seed),
            encKey: force::public_key(&secrets.enc_secret),
        };
        let (created, at) = self.client.create(params).await?;
        let id = created.gameId;
        let prefix = self.prefix(id);
        let named = config.dir.join(format!("{prefix}.secret.json"));
        if let Err(err) = secrets::rename(&pending, &named).await {
            // The game is live and the secrets are safe on disk, just under the temporary name.
            error!(game = %id, path = %pending.display(), %err, "could not rename the secrets file");
        }
        let params = created.params;
        let info = StakeInfo {
            chain_id: config.chain_id,
            escrow: format!("{:#x}", config.escrow),
            game_id: id.to_string(),
            token: format!("{:#x}", params.token),
            stake: params.stake.to_string(),
            operator: format!("{:#x}", created.operator),
            enc_key: params.encKey.to_string(),
            rpc_url: config.public_rpc_url.as_ref().map(Url::to_string),
            min_seats: params.minSeats,
            max_seats: params.maxSeats,
            join_by: params.joinBy,
        };
        Ok(OnChain {
            id,
            domain: Domain {
                chain_id: config.chain_id,
                escrow: config.escrow,
                game_id: id,
            },
            rules,
            info,
            join_by,
            created_block: at.block,
            transcript_path: config.dir.join(format!("{prefix}.transcript.json")),
            enc_secret_path: config.dir.join(format!("{prefix}.enc-secret")),
            secrets,
        })
    }

    /// Where game `id`'s transcript is saved.
    pub fn transcript_path(&self, id: U256) -> PathBuf {
        self.config
            .dir
            .join(format!("{}.transcript.json", self.prefix(id)))
    }

    /// Fixes the roster on-chain and waits for the randomness.
    pub async fn start(&self, game: &OnChain) -> Result<Started, StakeError> {
        let (started, at) = self.client.start(game.id).await?;
        let randomness = self.randomness(game.id, at.block).await?;
        let seats = self
            .consistent("seats", game.id, || async {
                let (players, keys) = self.client.seats(game.id).await?;
                let seats: Vec<Seat> = players
                    .into_iter()
                    .zip(keys)
                    .map(|(address, session_key)| Seat {
                        address,
                        session_key,
                    })
                    .collect();
                Ok((encode::roster_hash(&seats) == started.rosterHash).then_some(seats))
            })
            .await?;
        Ok(Started {
            seats,
            randomness,
            roster_hash: started.rosterHash,
            cursor: OpCursor {
                hash: B256::ZERO,
                block: at.block,
            },
        })
    }

    async fn randomness(&self, id: U256, from_block: u64) -> Result<B256, StakeError> {
        let deadline = Instant::now() + RANDOMNESS_WAIT;
        loop {
            let fulfilled = self.client.randomness_fulfilled(id, from_block).await?;
            if let Some((event, _)) = fulfilled.into_iter().next() {
                return Ok(event.randomness);
            }
            if Instant::now() >= deadline {
                return Err(StakeError::Invalid(format!(
                    "no RandomnessFulfilled within {RANDOMNESS_WAIT:?} of start"
                )));
            }
            tokio::time::sleep(RANDOMNESS_POLL).await;
        }
    }

    /// Checkpoints `round` and returns its forced moves in on-chain order, checked against the op
    /// log hash chain from `cursor`, which moves to the new checkpoint.
    pub async fn checkpoint(
        &self,
        game: &OnChain,
        round: u32,
        cursor: &mut OpCursor,
    ) -> Result<Vec<Force>, StakeError> {
        let (checkpoint, at) = self.client.checkpoint(game.id, round).await?;
        let prev = *cursor;
        let forces = self
            .consistent("forced_actions", game.id, || async {
                let mut logged = self
                    .client
                    .forced_actions(game.id, round, prev.block, at)
                    .await?;
                logged.sort_by_key(|(_, at)| *at);
                let forces: Vec<Force> = logged
                    .into_iter()
                    .map(|(event, _)| Force {
                        seat: event.seat,
                        payload: event.payload,
                    })
                    .collect();
                Ok(chains_to(prev.hash, round, &forces, checkpoint.opLogHash).then_some(forces))
            })
            .await?;
        *cursor = OpCursor {
            hash: checkpoint.opLogHash,
            block: at.block,
        };
        Ok(forces)
    }

    /// Posts the result and reveals the seed.
    pub async fn propose(
        &self,
        game: &OnChain,
        winner_team: u8,
        winners: U256,
        root: B256,
        uri: String,
    ) -> Result<(), StakeError> {
        self.client
            .propose(game.id, game.seed(), winner_team, winners, root, uri)
            .await?;
        Ok(())
    }

    /// The operator's signed receipt for an act that entered the transcript.
    pub fn receipt(&self, seat: u32, signed: &Signed) -> Result<Receipt, ChainError> {
        let signature = self.client.sign_digest(signed.digest)?;
        Ok(Receipt {
            turn_id: signed.turn_id,
            seat,
            act_hash: signed.act_hash.to_string(),
            digest: signed.digest.to_string(),
            signature: format!("0x{}", hex::encode(signature)),
        })
    }

    /// Re-reads until `read` agrees with what the chain committed to, since a lagging RPC can
    /// serve stale state or logs.
    async fn consistent<T, F, Fut>(
        &self,
        op: &'static str,
        game: U256,
        read: F,
    ) -> Result<T, StakeError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Option<T>, StakeError>>,
    {
        for attempt in 0..CONSISTENCY_ATTEMPTS {
            if let Some(value) = read().await? {
                return Ok(value);
            }
            let delay = backoff(attempt);
            warn!(%game, op, attempt = attempt + 1, ?delay, "chain reads disagree with the committed hash; re-reading");
            tokio::time::sleep(delay).await;
        }
        Err(StakeError::Inconsistent {
            op,
            attempts: CONSISTENCY_ATTEMPTS,
        })
    }

    /// `<chain>-<escrow>-<game>`, unique across escrows and chains.
    fn prefix(&self, id: U256) -> String {
        format!("{}-{:#x}-{id}", self.config.chain_id, self.config.escrow)
    }
}

/// Writes `bytes` to `path` through a temporary file, so readers never see a partial file.
pub async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StakeError> {
    let io = |source| StakeError::Io {
        what: "writing",
        path: path.to_owned(),
        source,
    };
    let partial = path.with_extension("partial");
    tokio::fs::write(&partial, bytes).await.map_err(io)?;
    tokio::fs::rename(&partial, path).await.map_err(io)
}

/// Parses a join proof and checks its session-key signature; returns the address and key.
pub fn check_join(domain: &Domain, join: &StakeJoin) -> Result<(Address, B256), String> {
    let address: Address = join
        .address
        .parse()
        .map_err(|_| "stake.address must be a 0x-hex address".to_owned())?;
    let session_key: B256 = join
        .session_key
        .parse()
        .map_err(|_| "stake.session_key must be 32 bytes of 0x-hex".to_owned())?;
    let signature: B512 = join
        .signature
        .parse()
        .map_err(|_| "stake.signature must be 64 bytes of 0x-hex".to_owned())?;
    keys::verify(
        &session_key,
        &encode::join_message(domain, &address),
        &signature,
    )
    .map_err(|err| format!("join signature: {err}"))?;
    Ok((address, session_key))
}

/// Whether `forces`, then checkpoint `round`, extend the op log from `prev` to `checkpoint`.
fn chains_to(prev: B256, round: u32, forces: &[Force], checkpoint: B256) -> bool {
    let forced = forces.iter().fold(prev, |hash, force| {
        encode::op_log_next(
            hash,
            &OpEntry::Force {
                seat: force.seat,
                round,
                payload: force.payload.clone(),
            },
        )
    });
    encode::op_log_next(forced, &OpEntry::Checkpoint { round }) == checkpoint
}

/// Full jitter: uniform in `[0, min(cap, base × 2^attempt))`.
pub(crate) fn backoff(attempt: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(1 << attempt.min(16))
        .min(BACKOFF_CAP)
        .mul_f64(rand::random::<f64>())
}

#[cfg(test)]
mod tests {
    use werewolf_verify::encode::op_log_hash;

    use super::*;

    #[test]
    fn op_chain_must_cover_every_force() {
        let a = Force {
            seat: 1,
            payload: Bytes::from_static(b"a"),
        };
        let b = Force {
            seat: 2,
            payload: Bytes::from_static(b"b"),
        };
        let entries = |forces: &[Force]| -> Vec<OpEntry> {
            forces
                .iter()
                .map(|f| OpEntry::Force {
                    seat: f.seat,
                    round: 0,
                    payload: f.payload.clone(),
                })
                .chain([OpEntry::Checkpoint { round: 0 }])
                .collect()
        };
        let both = [a.clone(), b.clone()];
        let committed = op_log_hash(&entries(&both));
        assert!(chains_to(B256::ZERO, 0, &both, committed));
        assert!(
            !chains_to(B256::ZERO, 0, &both[..1], committed),
            "a lagging RPC that misses a force must not match"
        );
        assert!(
            !chains_to(B256::ZERO, 0, &[b, a], committed),
            "order matters"
        );
        assert!(!chains_to(B256::ZERO, 1, &both, committed), "round matters");
    }

    #[test]
    fn join_proof_needs_the_session_key_signature() {
        let domain = Domain {
            chain_id: 31337,
            escrow: Address::with_last_byte(1),
            game_id: U256::from(3),
        };
        let key = keys::SigningKey::from_bytes(&[7; 32]);
        let address = Address::with_last_byte(0xaa);
        let signature = keys::sign(&key, &encode::join_message(&domain, &address));
        let join = StakeJoin {
            address: format!("{address:#x}"),
            session_key: keys::session_key(&key).to_string(),
            signature: signature.to_string(),
        };
        assert_eq!(
            check_join(&domain, &join),
            Ok((address, keys::session_key(&key)))
        );
        let other = Domain {
            game_id: U256::from(4),
            ..domain
        };
        assert!(check_join(&other, &join).is_err(), "bound to the game");
        let bad = StakeJoin {
            signature: "0x12".to_owned(),
            ..join
        };
        assert!(check_join(&domain, &bad).is_err());
    }
}
