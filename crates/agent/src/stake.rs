//! Staked games: the wallet, the seat's session key, signed acts, receipts and forced moves.

use crate::reply::truncate;
use alloy_primitives::{Address, B256, Bytes, Signature, U256, keccak256};
use std::{fmt::Display, path::PathBuf, str::FromStr};
use tracing::{error, warn};
use werewolf_chain::{ChainClient, ChainConfig, ChainError, bindings::IWerewolfEscrow::Status};
use werewolf_protocol::{ActRequest, Action, PublicEvent, Receipt, StakeInfo, StakeJoin, Turn};
use werewolf_verify::{
    encode::{self, Act, Domain, LogChain},
    force::{self, ForceError},
    keys::{self, SigningKey},
};

/// Tags the digest whose wallet signature seeds a game's session key.
const SESSION_TAG: &[u8] = b"werewolf/agent-session/v1";
/// Untrusted values quoted in errors are cut to this many bytes.
const MAX_QUOTED: usize = 80;

#[derive(Debug, thiserror::Error)]
pub enum StakeError {
    #[error(transparent)]
    Chain(#[from] ChainError),
    /// The on-chain game can't take this move or seat now.
    #[error("{0}")]
    NotOpen(String),
    /// Bad configuration, bad terms, or a wallet that can't pay; retrying won't help.
    #[error("{0}")]
    Setup(String),
    #[error("sealing the forced move: {0}")]
    Force(#[from] ForceError),
    #[error("encoding an action: {0}")]
    Encode(#[from] serde_json::Error),
}

impl StakeError {
    pub fn is_retryable(&self) -> bool {
        match self {
            // A reverted join changed nothing, and the next lobby may accept it.
            Self::Chain(error) => {
                error.is_retryable() || matches!(error, ChainError::Revert { .. })
            }
            Self::NotOpen(_) => true,
            Self::Setup(_) | Self::Force(_) | Self::Encode(_) => false,
        }
    }

    pub fn class(&self) -> &'static str {
        match self {
            Self::Chain(ChainError::Revert { .. }) => "chain_revert",
            Self::Chain(ChainError::Timeout { .. }) => "chain_timeout",
            Self::Chain(_) => "chain",
            Self::NotOpen(_) => "not_open",
            Self::Setup(_) => "setup",
            Self::Force(_) | Self::Encode(_) => "encode",
        }
    }
}

/// A staked game's terms from `/v1/info`, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    pub domain: Domain,
    pub token: Address,
    pub stake: U256,
    pub operator: Address,
    pub enc_key: B256,
    pub rpc_url: Option<String>,
}

impl Terms {
    pub fn parse(info: &StakeInfo) -> Result<Self, StakeError> {
        Ok(Self {
            domain: Domain {
                chain_id: info.chain_id,
                escrow: parse("escrow", &info.escrow)?,
                game_id: parse("game_id", &info.game_id)?,
            },
            token: parse("token", &info.token)?,
            stake: parse("stake", &info.stake)?,
            operator: parse("operator", &info.operator)?,
            enc_key: parse("enc_key", &info.enc_key)?,
            rpc_url: info.rpc_url.clone(),
        })
    }

    pub const fn game_id(&self) -> U256 {
        self.domain.game_id
    }
}

fn parse<T: FromStr>(field: &str, value: &str) -> Result<T, StakeError>
where
    T::Err: Display,
{
    value.parse().map_err(|error| {
        StakeError::Setup(format!(
            "server sent an invalid stake {field} {:?}: {error}",
            truncate(value, MAX_QUOTED)
        ))
    })
}

/// The player's wallet; one connection, so play and every watchtower share a nonce queue.
pub struct Wallet {
    key_file: PathBuf,
    rpc_url: Option<String>,
    chain: Option<(String, ChainClient)>,
}

impl Wallet {
    pub const fn new(key_file: PathBuf, rpc_url: Option<String>) -> Self {
        Self {
            key_file,
            rpc_url,
            chain: None,
        }
    }

    /// Connects, or reuses the connection while the RPC, chain and escrow stay the same.
    pub async fn connect(&mut self, terms: &Terms) -> Result<ChainClient, StakeError> {
        let url = self
            .rpc_url
            .clone()
            .or_else(|| terms.rpc_url.clone())
            .ok_or_else(|| {
                StakeError::Setup("the server suggests no RPC URL; pass --rpc-url".into())
            })?;
        if let Some((used, chain)) = &self.chain
            && *used == url
            && chain.chain_id() == terms.domain.chain_id
            && chain.escrow_address() == terms.domain.escrow
        {
            return Ok(chain.clone());
        }
        // The URL may embed an API key, so the error leaves it out.
        let parsed = url
            .parse()
            .map_err(|error| StakeError::Setup(format!("invalid RPC URL: {error}")))?;
        let config = ChainConfig::new(
            parsed,
            terms.domain.chain_id,
            terms.domain.escrow,
            Some(self.key_file.clone()),
        );
        let chain = ChainClient::connect(config).await?;
        self.chain = Some((url, chain.clone()));
        Ok(chain)
    }
}

/// A receipt that checked out, kept for the watchtower.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub turn_id: u64,
    pub seat: u32,
    pub act_hash: B256,
    /// The Merkle leaf.
    pub digest: B256,
    pub signature: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    #[error("receipt is for turn {turn_id} seat {seat}, not this act")]
    WrongAct { turn_id: u64, seat: u32 },
    #[error("receipt's act hash or digest differs from the act we signed")]
    WrongDigest,
    #[error("receipt signature is not 65 bytes with v 27 or 28 and a low s")]
    Malformed,
    #[error("receipt is signed by {0}, not the operator")]
    WrongSigner(Address),
    #[error("encoding the act: {0}")]
    Encode(#[from] serde_json::Error),
}

/// One staked seat in one game.
pub struct Seat {
    pub chain: ChainClient,
    pub terms: Terms,
    pub seat: u32,
    key: SigningKey,
    log: LogChain,
    receipts: Vec<Kept>,
}

impl Seat {
    /// Joins on-chain unless this wallet already holds a seat; returns the seat and its HTTP join proof.
    pub async fn take(chain: ChainClient, terms: Terms) -> Result<(Self, StakeJoin), StakeError> {
        let game_id = terms.game_id();
        let address = chain
            .address()
            .ok_or_else(|| StakeError::Setup("the chain client has no wallet key".into()))?;
        let key = session_key(&chain, &terms.domain)?;
        let public = keys::session_key(&key);
        let (players, session_keys) = chain.seats(game_id).await?;
        let seated = (0u32..).zip(&players).find(|(_, p)| **p == address);
        let seat = match seated {
            Some((seat, _)) if session_keys.get(seat as usize) == Some(&public) => seat,
            Some((seat, _)) => {
                return Err(StakeError::Setup(format!(
                    "{address} holds seat {seat} in game {game_id} under a session key it didn't derive"
                )));
            }
            None => {
                let game = chain.game_info(game_id).await?;
                if game.status != Status::Open {
                    return Err(StakeError::NotOpen(format!(
                        "game {game_id} is {:?} on-chain; waiting for a new lobby",
                        game.status
                    )));
                }
                let balance = chain.token_balance(terms.token, address).await?;
                if balance < terms.stake {
                    return Err(StakeError::Setup(format!(
                        "wallet {address} holds {balance} of token {}, short of the {} stake",
                        terms.token, terms.stake
                    )));
                }
                chain.join(game_id, public).await?.0.seat
            }
        };
        let join = StakeJoin {
            address: address.to_string(),
            session_key: public.to_string(),
            signature: keys::sign(&key, &encode::join_message(&terms.domain, &address)).to_string(),
        };
        let seat = Self {
            chain,
            log: LogChain::new(&terms.domain),
            terms,
            seat,
            key,
            receipts: Vec::new(),
        };
        Ok((seat, join))
    }

    /// Signs `request` for `turn`, over the public log the turn shows.
    pub fn sign(&mut self, turn: &Turn, request: &mut ActRequest) -> Result<(), serde_json::Error> {
        sync_log(&mut self.log, &self.terms.domain, &turn.view.public_log)?;
        let message = self
            .act(turn, &request.action)
            .message(&self.terms.domain)?;
        request.log_hash = Some(self.log.head().to_string());
        request.signature = Some(keys::sign(&self.key, &message).to_string());
        Ok(())
    }

    /// Checks `receipt` against the act last signed for `turn` and keeps it.
    pub fn keep_receipt(
        &mut self,
        turn: &Turn,
        action: &Action,
        receipt: &Receipt,
    ) -> Result<(), ReceiptError> {
        let kept = check_receipt(&self.terms, &self.act(turn, action), receipt)?;
        self.receipts.push(kept);
        Ok(())
    }

    /// Forces `action` for `round` from this seat's wallet; meant to be spawned.
    pub fn force(&self, round: u32, action: Action) -> impl Future<Output = ()> + Send + 'static {
        let (chain, terms, seat) = (self.chain.clone(), self.terms.clone(), self.seat);
        async move {
            let game_id = terms.game_id();
            match try_force(&chain, &terms, seat, round, &action).await {
                Ok(()) => {}
                Err(error @ StakeError::NotOpen(_)) => {
                    warn!(%game_id, round, seat, error_class = error.class(), %error, "could not force the move");
                }
                Err(error) => {
                    error!(%game_id, round, seat, error_class = error.class(), %error, "could not force the move");
                }
            }
        }
    }

    pub fn into_receipts(self) -> (ChainClient, Terms, Vec<Kept>) {
        (self.chain, self.terms, self.receipts)
    }

    fn act<'a>(&self, turn: &Turn, action: &'a Action) -> Act<'a> {
        Act {
            turn_id: turn.turn_id,
            step: turn.step,
            seat: self.seat,
            log_hash: self.log.head(),
            action,
        }
    }
}

/// Extends `chain` with the events it hasn't seen; restarts it if the log got shorter.
fn sync_log(
    chain: &mut LogChain,
    domain: &Domain,
    log: &[PublicEvent],
) -> Result<(), serde_json::Error> {
    let mut seen = chain.hashes().len().saturating_sub(1);
    if log.len() < seen {
        *chain = LogChain::new(domain);
        seen = 0;
    }
    log[seen..].iter().try_for_each(|event| chain.push(event))
}

/// Derived from a deterministic wallet signature, so a restarted agent gets the key it registered.
fn session_key(chain: &ChainClient, domain: &Domain) -> Result<SigningKey, ChainError> {
    let digest = keccak256([SESSION_TAG, &domain.to_bytes()].concat());
    let signature = chain.sign_digest(digest)?;
    Ok(SigningKey::from_bytes(&keccak256(signature).0))
}

async fn try_force(
    chain: &ChainClient,
    terms: &Terms,
    seat: u32,
    round: u32,
    action: &Action,
) -> Result<(), StakeError> {
    let game = chain.game_info(terms.game_id()).await?;
    if game.nextRound != round {
        return Err(StakeError::NotOpen(format!(
            "round {round} is already checkpointed (next round {})",
            game.nextRound
        )));
    }
    let payload = force::encrypt_random(&terms.enc_key, &terms.domain, round, seat, action)?;
    chain
        .force_action(terms.game_id(), round, payload.into())
        .await?;
    Ok(())
}

/// Recomputes the receipt's digest from `act` and checks the operator signed it.
fn check_receipt(terms: &Terms, act: &Act<'_>, receipt: &Receipt) -> Result<Kept, ReceiptError> {
    if receipt.turn_id != act.turn_id || receipt.seat != act.seat {
        return Err(ReceiptError::WrongAct {
            turn_id: receipt.turn_id,
            seat: receipt.seat,
        });
    }
    let act_hash = encode::act_hash(&act.message(&terms.domain)?);
    let digest = encode::receipt_digest(&terms.domain, act.turn_id, act.seat, act_hash);
    if receipt.act_hash.parse() != Ok(act_hash) || receipt.digest.parse() != Ok(digest) {
        return Err(ReceiptError::WrongDigest);
    }
    let signature: Bytes = receipt
        .signature
        .parse()
        .map_err(|_| ReceiptError::Malformed)?;
    let signer = recover(&signature, &digest).ok_or(ReceiptError::Malformed)?;
    if signer != terms.operator {
        return Err(ReceiptError::WrongSigner(signer));
    }
    Ok(Kept {
        turn_id: act.turn_id,
        seat: act.seat,
        act_hash,
        digest,
        signature,
    })
}

/// Accepts only what OpenZeppelin's `ECDSA.tryRecover` in `disputeReceipt` accepts: v 27 or 28, low s.
fn recover(signature: &[u8], digest: &B256) -> Option<Address> {
    let parsed = Signature::from_raw(signature).ok()?;
    if !matches!(signature.last(), Some(27 | 28)) || parsed.normalize_s().is_some() {
        return None;
    }
    parsed.recover_address_from_prehash(digest).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};
    use alloy_primitives::uint;
    use werewolf_protocol::Phase;

    const SECP256K1_ORDER: U256 =
        uint!(0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141_U256);

    fn terms(operator: Address) -> Terms {
        Terms {
            domain: Domain {
                chain_id: 31337,
                escrow: Address::repeat_byte(0xe5),
                game_id: U256::from(7),
            },
            token: Address::repeat_byte(0x70),
            stake: U256::from(100),
            operator,
            enc_key: B256::repeat_byte(9),
            rpc_url: None,
        }
    }

    fn receipt(operator: &PrivateKeySigner, act: &Act<'_>, terms: &Terms) -> Receipt {
        let act_hash = encode::act_hash(&act.message(&terms.domain).unwrap());
        let digest = encode::receipt_digest(&terms.domain, act.turn_id, act.seat, act_hash);
        let signature = operator.sign_hash_sync(&digest).unwrap();
        Receipt {
            turn_id: act.turn_id,
            seat: act.seat,
            act_hash: act_hash.to_string(),
            digest: digest.to_string(),
            signature: Bytes::from(signature.as_bytes()).to_string(),
        }
    }

    #[test]
    fn receipt_must_match_the_act_and_recover_to_the_operator() {
        let operator = PrivateKeySigner::random();
        let terms = terms(operator.address());
        let action = Action::Target { target: Some(2) };
        let act = Act {
            turn_id: 9,
            step: 4,
            seat: 1,
            log_hash: B256::repeat_byte(1),
            action: &action,
        };
        let good = receipt(&operator, &act, &terms);
        let kept = check_receipt(&terms, &act, &good).unwrap();
        assert_eq!(kept.digest.to_string(), good.digest);

        let other = Action::Target { target: None };
        let changed = Act {
            action: &other,
            ..act
        };
        assert!(matches!(
            check_receipt(&terms, &changed, &good),
            Err(ReceiptError::WrongDigest)
        ));
        assert!(matches!(
            check_receipt(&terms, &Act { seat: 2, ..act }, &good),
            Err(ReceiptError::WrongAct { .. })
        ));

        let impostor = PrivateKeySigner::random();
        assert!(matches!(
            check_receipt(&terms, &act, &receipt(&impostor, &act, &terms)),
            Err(ReceiptError::WrongSigner(signer)) if signer == impostor.address()
        ));

        // The contract rejects v 0/1 and high s, so neither could back a dispute.
        let raw: Bytes = good.signature.parse().unwrap();
        let mut low_v = raw.to_vec();
        low_v[64] -= 27;
        let parsed = Signature::from_raw(&raw).unwrap();
        let high_s = Signature::new(parsed.r(), SECP256K1_ORDER - parsed.s(), !parsed.v());
        for bad in [low_v, high_s.as_bytes().to_vec()] {
            let receipt = Receipt {
                signature: Bytes::from(bad).to_string(),
                ..good.clone()
            };
            assert!(matches!(
                check_receipt(&terms, &act, &receipt),
                Err(ReceiptError::Malformed)
            ));
        }
    }

    #[test]
    fn terms_parse_and_reject_bad_hex() {
        let info = StakeInfo {
            chain_id: 31337,
            escrow: "0x5fbdb2315678afecb367f032d93f642f64180aa3".into(),
            game_id: "12".into(),
            token: "0xe7f1725e7734ce288f8367e1bb143e90bb3f0512".into(),
            stake: "1000".into(),
            operator: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".into(),
            enc_key: format!("{}", B256::repeat_byte(3)),
            rpc_url: Some("http://127.0.0.1:8545".into()),
            min_seats: 5,
            max_seats: 8,
            join_by: 0,
        };
        let terms = Terms::parse(&info).unwrap();
        assert_eq!(terms.game_id(), U256::from(12));
        assert_eq!(terms.stake, U256::from(1000));

        let bad = StakeInfo {
            operator: "0xnope".into(),
            ..info
        };
        let error = Terms::parse(&bad).unwrap_err();
        assert!(!error.is_retryable());
        assert!(error.to_string().contains("operator"), "{error}");
    }

    #[test]
    fn log_chain_follows_the_view_and_restarts_when_it_shrinks() {
        let domain = terms(Address::ZERO).domain;
        let log: Vec<PublicEvent> = (1..=3)
            .map(|n| PublicEvent::PhaseStarted {
                phase: Phase::Day { n },
            })
            .collect();
        let mut fresh = LogChain::new(&domain);
        log.iter().for_each(|event| fresh.push(event).unwrap());

        let mut chain = LogChain::new(&domain);
        sync_log(&mut chain, &domain, &log[..1]).unwrap();
        sync_log(&mut chain, &domain, &log).unwrap();
        assert_eq!(chain.head(), fresh.head());

        sync_log(&mut chain, &domain, &log[..2]).unwrap();
        assert_eq!(chain.hashes(), &fresh.hashes()[..3]);
    }
}
