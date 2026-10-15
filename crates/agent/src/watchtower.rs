//! After a staked game: checks the operator's result, disputes dropped receipts, and claims.

use crate::{
    backoff::{Backoff, full_jitter},
    client::GameClient,
    http::HttpError,
    stake::{Kept, Terms},
};
use anyhow::{Context, bail};
use reqwest::StatusCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{error, warn};
use werewolf_chain::{
    ChainClient, ChainError,
    bindings::IWerewolfEscrow::{GameView, Status},
};
use werewolf_verify::{
    Decrypt, PublicValues, Transcript,
    encode::{Domain, merkle_root, public_values_json},
    verify,
};

const POLL: Duration = Duration::from_secs(3);
/// How long past the game's last on-chain deadline to keep waiting for it to end.
const GRACE_SECS: u64 = 3600;
/// Gives up once the chain has been unreadable this long.
const CHAIN_GIVE_UP: Duration = Duration::from_secs(600);
const CHAIN_RETRY_CAP: Duration = Duration::from_secs(30);
const TRANSCRIPT_ATTEMPTS: u32 = 6;
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(10);

/// A finished staked game to see through to its payout.
pub struct Watch {
    pub name: String,
    pub server: GameClient,
    pub chain: ChainClient,
    pub terms: Terms,
    pub receipts: Vec<Kept>,
}

/// Logs failures rather than returning them, so a watchtower can never stop play.
pub async fn watch(watch: Watch) {
    if let Err(error) = run(&watch).await {
        error!(game_id = %watch.terms.game_id(), error = format!("{error:#}"), "watchtower stopped before the payout");
    }
}

async fn run(watch: &Watch) -> anyhow::Result<()> {
    let game = watch
        .wait(|status| !matches!(status, Status::Open | Status::Started))
        .await?;
    if matches!(game.status, Status::Proposed | Status::Challenged) {
        watch.audit(&game).await;
    }
    let game = watch
        .wait(|status| matches!(status, Status::Settled | Status::Void | Status::Cancelled))
        .await?;
    let game_id = watch.terms.game_id();
    let account = watch
        .chain
        .address()
        .context("the chain client has no wallet key")?;
    let owed = watch.chain.claimable(game_id, account).await?;
    if !owed.is_zero() {
        watch.chain.claim(game_id).await?;
    }
    println!(
        "{}: game {game_id} ended {:?}, claimed {owed}",
        watch.name, game.status
    );
    Ok(())
}

impl Watch {
    /// Polls the game until `done(status)`, finalizing it once a deadline passes.
    async fn wait(&self, done: impl Fn(Status) -> bool) -> anyhow::Result<GameView> {
        let game_id = self.terms.game_id();
        let mut failures = Backoff::new(POLL, CHAIN_RETRY_CAP, CHAIN_GIVE_UP);
        loop {
            let game = match self.chain.game_info(game_id).await {
                Ok(game) => {
                    failures.reset();
                    game
                }
                Err(error) => {
                    let Some(delay) = failures.next_delay() else {
                        return Err(error).context(format!(
                            "chain unreadable for over {}s",
                            CHAIN_GIVE_UP.as_secs()
                        ));
                    };
                    warn!(%game_id, attempt = failures.attempt(), %error, "could not read the game; retrying");
                    sleep(delay).await;
                    continue;
                }
            };
            if done(game.status) {
                return Ok(game);
            }
            let now = unix_now();
            if now > give_up_at(&game) {
                bail!(
                    "game {game_id} is still {:?} over {GRACE_SECS}s after its last deadline",
                    game.status
                );
            }
            if finalize_due(&game).is_some_and(|due| now > due) {
                self.finalize().await;
            }
            sleep(POLL).await;
        }
    }

    async fn finalize(&self) {
        let game_id = self.terms.game_id();
        match self.chain.finalize(game_id).await {
            // The server finalizes too, and an open dispute holds the game; both just revert.
            Ok(_) | Err(ChainError::Revert { .. }) => {}
            Err(error) => warn!(%game_id, %error, "finalize failed"),
        }
    }

    async fn audit(&self, game: &GameView) {
        let game_id = self.terms.game_id();
        let transcript = match self.transcript().await {
            Ok(transcript) => transcript,
            // Garbage where the transcript should be is as bad as one that doesn't replay.
            Err(error @ HttpError::Decode { .. }) => {
                self.challenge(game, &format!("transcript does not parse: {error}"))
                    .await;
                return;
            }
            Err(error) => {
                error!(%game_id, error_class = error.class(), %error, "could not fetch the transcript; the result goes unchecked");
                return;
            }
        };
        let (view, domain, receipts) = (game.clone(), self.terms.domain, self.receipts.clone());
        let findings =
            tokio::task::spawn_blocking(move || audit(&transcript, &view, &domain, &receipts))
                .await;
        let findings = match findings {
            Ok(findings) => findings,
            Err(error) => {
                error!(%game_id, %error, "transcript audit panicked; the result goes unchecked");
                return;
            }
        };
        if let Some(problem) = &findings.problem {
            self.challenge(game, problem).await;
        }
        match findings.missing {
            Ok(missing) => {
                for kept in &missing {
                    self.dispute(kept).await;
                }
            }
            Err(reason) => error!(%game_id, reason, "could not check our receipts"),
        }
    }

    /// Fetches with bounded retries.
    async fn transcript(&self) -> Result<Transcript, HttpError> {
        let game_id = self.terms.game_id();
        let mut attempt = 0;
        loop {
            let error = match self.server.transcript(game_id).await {
                Ok(transcript) => return Ok(transcript),
                Err(error) => error,
            };
            attempt += 1;
            // 404 until the server saves it, just before proposing.
            let retryable = error.is_retryable() || error.status() == Some(StatusCode::NOT_FOUND);
            if !retryable || attempt >= TRANSCRIPT_ATTEMPTS {
                return Err(error);
            }
            warn!(%game_id, attempt, error_class = error.class(), %error, "transcript fetch failed; retrying");
            sleep(full_jitter(RETRY_BASE, RETRY_CAP, attempt)).await;
        }
    }

    async fn challenge(&self, game: &GameView, problem: &str) {
        let game_id = self.terms.game_id();
        if game.status != Status::Proposed || game.proven {
            error!(%game_id, problem, status = ?game.status, proven = game.proven, "operator's result does not check out; too late to challenge");
            return;
        }
        error!(%game_id, problem, "operator's result does not check out; challenging");
        if let Err(error) = self.chain.challenge(game_id).await {
            error!(%game_id, %error, "challenge failed");
        }
    }

    async fn dispute(&self, kept: &Kept) {
        let game_id = self.terms.game_id();
        error!(%game_id, turn_id = kept.turn_id, seat = kept.seat, leaf = %kept.digest, "transcript drops an act the operator gave us a receipt for; disputing");
        let disputed = self
            .chain
            .dispute_receipt(
                game_id,
                kept.turn_id,
                kept.seat,
                kept.act_hash,
                kept.signature.clone(),
            )
            .await;
        if let Err(error) = disputed {
            error!(%game_id, turn_id = kept.turn_id, %error, "dispute failed");
        }
    }
}

/// What a proposed game's transcript shows.
#[derive(Debug)]
struct Findings {
    /// Why the proposal can't be right, if it can't.
    problem: Option<String>,
    /// Kept receipts the on-chain root leaves out, or why that couldn't be checked.
    missing: Result<Vec<Kept>, String>,
}

fn audit(transcript: &Transcript, game: &GameView, domain: &Domain, receipts: &[Kept]) -> Findings {
    let proposal = proposal(game, domain);
    let problem = match verify(transcript, Decrypt::Trust) {
        Ok(values) if values == proposal => None,
        Ok(values) => Some(format!(
            "replay gives {} but the chain has {}",
            public_values_json(&values),
            public_values_json(&proposal)
        )),
        Err(error) => Some(format!("transcript does not replay: {error}")),
    };
    let missing = match transcript.leaves() {
        Ok(leaves) if merkle_root(&leaves) == game.transcriptRoot => Ok(receipts
            .iter()
            .filter(|kept| !leaves.contains(&kept.digest))
            .cloned()
            .collect()),
        Ok(_) => Err("the transcript's leaves don't hash to the on-chain root".to_owned()),
        Err(error) => Err(format!("cannot hash the transcript's leaves: {error}")),
    };
    Findings { problem, missing }
}

/// What `prove` would have to match: the proposal plus everything fixed on-chain before it.
fn proposal(game: &GameView, domain: &Domain) -> PublicValues {
    PublicValues {
        chainId: domain.chain_id,
        escrow: domain.escrow,
        gameId: domain.game_id,
        rulesHash: game.rulesHash,
        rosterHash: game.rosterHash,
        seedCommit: game.seedCommit,
        randomness: game.randomness,
        encKey: game.encKey,
        opLogHash: game.opLogHash,
        winnerTeam: game.winnerTeam,
        winners: game.winners,
        transcriptRoot: game.transcriptRoot,
    }
}

/// Wall-clock time when `finalize` may first succeed; chain time tracks it on a live chain.
fn finalize_due(game: &GameView) -> Option<u64> {
    match game.status {
        Status::Open => Some(game.joinBy),
        Status::Started => Some(game.playBy),
        Status::Proposed => Some(game.challengeEnds),
        Status::Challenged => Some(game.proofDeadline),
        _ => None,
    }
}

fn give_up_at(game: &GameView) -> u64 {
    game.playBy
        .max(game.challengeEnds)
        .max(game.proofDeadline)
        .saturating_add(GRACE_SECS)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bytes, U256, aliases::U96};

    const TRANSCRIPT: &str = include_str!("../../verify/tests/transcript.json");

    /// The verify crate's sample game, and an on-chain view that proposes its true result.
    fn proposed() -> (Transcript, Domain, GameView, Vec<B256>) {
        let transcript: Transcript = serde_json::from_str(TRANSCRIPT).unwrap();
        let domain = transcript.header.domain();
        let values = verify(&transcript, Decrypt::Trust).unwrap();
        let game = GameView {
            operator: Address::ZERO,
            token: Address::ZERO,
            stake: U96::from(100),
            minSeats: 5,
            maxSeats: 12,
            joinBy: 0,
            playBy: 0,
            rulesHash: values.rulesHash,
            seedCommit: values.seedCommit,
            encKey: values.encKey,
            status: Status::Proposed,
            seatCount: 8,
            rosterHash: values.rosterHash,
            randomness: values.randomness,
            opLogHash: values.opLogHash,
            nextRound: 0,
            winnerTeam: values.winnerTeam,
            winners: values.winners,
            transcriptRoot: values.transcriptRoot,
            challengeEnds: 0,
            proofDeadline: 0,
            challenger: Address::ZERO,
            proven: false,
            openDisputes: 0,
        };
        let leaves = transcript.leaves().unwrap();
        (transcript, domain, game, leaves)
    }

    fn kept(digest: B256) -> Kept {
        Kept {
            turn_id: 1,
            seat: 0,
            act_hash: B256::ZERO,
            digest,
            signature: Bytes::new(),
        }
    }

    #[test]
    fn honest_proposal_passes() {
        let (transcript, domain, game, leaves) = proposed();
        let findings = audit(&transcript, &game, &domain, &[kept(leaves[0])]);
        assert!(findings.problem.is_none(), "{findings:?}");
        assert_eq!(findings.missing.unwrap(), []);
    }

    #[test]
    fn wrong_result_bad_replay_and_dropped_receipts_are_caught() {
        let (mut transcript, domain, mut game, leaves) = proposed();
        let dropped = kept(B256::repeat_byte(7));
        let findings = audit(
            &transcript,
            &game,
            &domain,
            &[kept(leaves[0]), dropped.clone()],
        );
        assert_eq!(findings.missing.unwrap(), [dropped]);

        game.winners ^= U256::from(1);
        let problem = audit(&transcript, &game, &domain, &[]).problem.unwrap();
        assert!(problem.contains("winners"), "{problem}");

        game.transcriptRoot = B256::repeat_byte(1);
        assert!(audit(&transcript, &game, &domain, &[]).missing.is_err());

        transcript.header.seed = B256::ZERO;
        let problem = audit(&transcript, &game, &domain, &[]).problem.unwrap();
        assert!(problem.contains("does not replay"), "{problem}");
    }
}
