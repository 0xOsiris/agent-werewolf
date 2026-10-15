//! One background task per escrow game, from `create` until the operator's credit is claimed:
//! answers receipt disputes, proves challenged results, finalizes and claims.

use std::{process::Stdio, sync::Arc, time::Duration};

use alloy_sol_types::SolEvent;
use serde::Deserialize;
use tokio::{fs, process::Command, task::JoinHandle, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use werewolf_chain::bindings::IWerewolfEscrow::{
    Challenged, GameView, IWerewolfEscrowEvents, ReceiptDisputed, Status,
};
use werewolf_verify::{
    Transcript,
    alloy_primitives::{B256, Bytes, U256},
    encode::merkle_proof,
};

use super::{OnChain, Stake, StakeError, secrets};

/// Time kept back from the proof deadline for the `prove` transaction itself.
const PROOF_MARGIN: Duration = Duration::from_secs(60);
/// Prover stderr kept for the error log.
const STDERR_TAIL: usize = 2_000;

/// `werewolf-prove`'s `--out` file.
#[derive(Deserialize)]
struct ProverOutput {
    public_values: Bytes,
    proof: Bytes,
}

/// Runs until the game is finalized and claimed, or `shutdown`. Calls `on_cancelled` if it ends
/// `Cancelled`, i.e. the join window closed before `start`.
pub async fn settle(
    stake: Arc<Stake>,
    game: Arc<OnChain>,
    shutdown: CancellationToken,
    on_cancelled: impl FnOnce() + Send + 'static,
) {
    let poll = stake.config.settle_poll;
    let events = [Challenged::SIGNATURE_HASH, ReceiptDisputed::SIGNATURE_HASH];
    let mut watcher = match stake.client.watch(
        game.id,
        &events,
        game.created_block,
        poll,
        shutdown.child_token(),
    ) {
        Ok(watcher) => watcher,
        Err(err) => {
            error!(game = %game.id, %err, "cannot watch the escrow; settle this game by hand");
            return;
        }
    };
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut settlement = Settlement {
        stake,
        game,
        disputes: Vec::new(),
        leaves: None,
        challenged: false,
        prover: None,
        proof: None,
    };
    loop {
        let mut check_end = false;
        tokio::select! {
            () = shutdown.cancelled() => return,
            batch = watcher.next() => match batch {
                None => return,
                Some(Ok(batch)) => {
                    for (event, _) in batch {
                        settlement.observe(event);
                    }
                }
                Some(Err(err)) => {
                    let id = settlement.game.id;
                    warn!(game = %id, op = "watch", %err, "escrow log poll failed; retrying");
                }
            },
            proved = settlement.prover_done() => settlement.proved(proved),
            _ = tick.tick() => check_end = true,
        }
        settlement.answer_disputes().await;
        settlement.start_prover().await;
        settlement.submit_proof().await;
        if check_end && let Some(status) = settlement.advance().await {
            if status == Status::Cancelled {
                on_cancelled();
            }
            return;
        }
    }
}

struct Settlement {
    stake: Arc<Stake>,
    game: Arc<OnChain>,
    /// Disputes still to answer: id and leaf.
    disputes: Vec<(U256, B256)>,
    /// The saved transcript's leaves, loaded at the first dispute.
    leaves: Option<Vec<B256>>,
    challenged: bool,
    prover: Option<JoinHandle<Result<ProverOutput, StakeError>>>,
    /// A proof waiting for its `prove` transaction.
    proof: Option<ProverOutput>,
}

impl Settlement {
    fn observe(&mut self, event: IWerewolfEscrowEvents) {
        match event {
            IWerewolfEscrowEvents::Challenged(_) => self.challenged = true,
            IWerewolfEscrowEvents::ReceiptDisputed(dispute) => {
                self.disputes.push((dispute.disputeId, dispute.leaf));
            }
            _ => {}
        }
    }

    async fn answer_disputes(&mut self) {
        let id = self.game.id;
        let mut pending = std::mem::take(&mut self.disputes);
        if pending.is_empty() {
            return;
        }
        let leaves = match self.leaves().await {
            Ok(leaves) => leaves.to_vec(),
            Err(err) => {
                error!(game = %id, disputes = pending.len(), %err, "cannot answer receipt disputes without the transcript");
                return;
            }
        };
        for (dispute, leaf) in pending.drain(..) {
            let Some(proof) = leaves
                .iter()
                .position(|l| *l == leaf)
                .and_then(|index| merkle_proof(&leaves, index))
            else {
                error!(game = %id, %dispute, %leaf, "disputed receipt is not a transcript leaf; cannot answer it");
                continue;
            };
            match self.stake.client.answer_dispute(id, dispute, proof).await {
                Ok(_) => {}
                Err(err) => {
                    let err = StakeError::from(err);
                    let (class, attempts, tx) = err.fields();
                    if err.is_retryable() {
                        warn!(game = %id, op = "answerDispute", %dispute, class, ?attempts, ?tx, %err, "dispute answer failed; retrying");
                        self.disputes.push((dispute, leaf));
                    } else {
                        error!(game = %id, op = "answerDispute", %dispute, class, ?tx, %err, "dispute answer failed for good");
                    }
                }
            }
        }
    }

    async fn leaves(&mut self) -> Result<&[B256], StakeError> {
        if self.leaves.is_none() {
            let path = &self.game.transcript_path;
            let json = fs::read(path).await.map_err(|source| StakeError::Io {
                what: "reading",
                path: path.clone(),
                source,
            })?;
            let transcript: Transcript = serde_json::from_slice(&json)?;
            self.leaves = Some(transcript.leaves()?);
        }
        Ok(self.leaves.as_deref().unwrap_or_default())
    }

    /// Starts the prover once a challenge is seen, bounded by the proof deadline.
    async fn start_prover(&mut self) {
        if !self.challenged || self.prover.is_some() || self.proof.is_some() {
            return;
        }
        // One attempt per challenge; a failed prover is reported, and proving by hand remains.
        self.challenged = false;
        let id = self.game.id;
        let client = &self.stake.client;
        let (info, now) = match tokio::try_join!(client.game_info(id), client.latest_timestamp()) {
            Ok(found) => found,
            Err(err) => {
                warn!(game = %id, op = "gameInfo", %err, "cannot read the proof deadline; retrying");
                self.challenged = true;
                return;
            }
        };
        if info.proven {
            return;
        }
        let left = Duration::from_secs(info.proofDeadline.saturating_sub(now));
        let Some(budget) = left.checked_sub(PROOF_MARGIN) else {
            error!(game = %id, proof_deadline = info.proofDeadline, now, "too little time left to prove the challenged result");
            return;
        };
        let (stake, game) = (Arc::clone(&self.stake), Arc::clone(&self.game));
        self.prover = Some(tokio::spawn(async move {
            run_prover(&stake, &game, budget).await
        }));
    }

    /// Resolves when the running prover exits; never, if none is running.
    async fn prover_done(
        &mut self,
    ) -> Result<Result<ProverOutput, StakeError>, tokio::task::JoinError> {
        match &mut self.prover {
            Some(handle) => handle.await,
            None => std::future::pending().await,
        }
    }

    fn proved(&mut self, result: Result<Result<ProverOutput, StakeError>, tokio::task::JoinError>) {
        self.prover = None;
        match result {
            Ok(Ok(output)) => self.proof = Some(output),
            Ok(Err(err)) => {
                let (class, ..) = err.fields();
                error!(game = %self.game.id, op = "prove", class, %err, "prover failed; prove by hand with werewolf-prove before the deadline");
            }
            Err(err) => {
                error!(game = %self.game.id, op = "prove", %err, "prover task died; prove by hand with werewolf-prove before the deadline");
            }
        }
    }

    async fn submit_proof(&mut self) {
        let Some(output) = &self.proof else { return };
        let id = self.game.id;
        let sent = self
            .stake
            .client
            .prove(id, output.public_values.clone(), output.proof.clone())
            .await;
        match sent.map_err(StakeError::from) {
            Ok(_) => self.proof = None,
            Err(err) => {
                let (class, attempts, tx) = err.fields();
                if err.is_retryable() {
                    warn!(game = %id, op = "prove", class, ?attempts, ?tx, %err, "prove failed; retrying");
                } else {
                    error!(game = %id, op = "prove", class, ?tx, %err, "prove failed for good");
                    self.proof = None;
                }
            }
        }
    }

    /// Finalizes once a deadline has passed, then claims; the end status once nothing is left.
    async fn advance(&mut self) -> Option<Status> {
        let id = self.game.id;
        let client = &self.stake.client;
        let info = match client.game_info(id).await {
            Ok(info) => info,
            Err(err) => {
                warn!(game = %id, op = "gameInfo", %err, "settlement read failed; retrying");
                return None;
            }
        };
        let mut status = info.status;
        if !matches!(status, Status::Settled | Status::Void | Status::Cancelled) {
            let now = match client.latest_timestamp().await {
                Ok(now) => now,
                Err(err) => {
                    warn!(game = %id, op = "latestTimestamp", %err, "settlement read failed; retrying");
                    return None;
                }
            };
            if !due(&info, now) {
                return None;
            }
            match client.finalize(id).await.map_err(StakeError::from) {
                Ok((finalized, _)) => status = finalized.status,
                // Open disputes or a deadline not yet mined; the next tick tries again.
                Err(err) if err.is_revert("NotFinalizable") => return None,
                Err(err) => {
                    let (class, attempts, tx) = err.fields();
                    warn!(game = %id, op = "finalize", class, ?attempts, ?tx, %err, "finalize failed; retrying");
                    return None;
                }
            }
        }
        let owed = match client.claimable(id, self.stake.operator).await {
            Ok(owed) => owed,
            Err(err) => {
                warn!(game = %id, op = "claimable", %err, "settlement read failed; retrying");
                return None;
            }
        };
        if !owed.is_zero()
            && let Err(err) = client.claim(id).await.map_err(StakeError::from)
        {
            let (class, attempts, tx) = err.fields();
            warn!(game = %id, op = "claim", class, ?attempts, ?tx, %err, "claim failed; retrying");
            return None;
        }
        Some(status)
    }
}

/// Whether `finalize` should succeed at chain time `now`, disputes aside.
fn due(info: &GameView, now: u64) -> bool {
    match info.status {
        Status::Open => now > info.joinBy,
        Status::Started => now > info.playBy,
        Status::Proposed => now >= info.challengeEnds,
        Status::Challenged => now >= info.proofDeadline,
        _ => false,
    }
}

/// Runs `werewolf-prove` on the saved transcript and reads its output.
async fn run_prover(
    stake: &Stake,
    game: &OnChain,
    budget: Duration,
) -> Result<ProverOutput, StakeError> {
    let config = &stake.config;
    let secret = B256::from(*game.enc_secret()).to_string();
    secrets::write_private(&game.enc_secret_path, secret.as_bytes()).await?;
    let out = game.transcript_path.with_extension("proof.json");
    let child = Command::new(&config.prover_bin)
        .arg("prove")
        .arg("--transcript")
        .arg(&game.transcript_path)
        .arg("--enc-secret-file")
        .arg(&game.enc_secret_path)
        .arg("--mode")
        .arg(config.prover_mode.as_str())
        .arg("--out")
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| StakeError::Io {
            what: "running",
            path: config.prover_bin.clone(),
            source,
        })?;
    let output = tokio::time::timeout(budget, child.wait_with_output())
        .await
        .map_err(|_| StakeError::Prover(format!("no proof within {budget:?}")))?
        .map_err(|source| StakeError::Io {
            what: "waiting for",
            path: config.prover_bin.clone(),
            source,
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let skip = stderr.chars().count().saturating_sub(STDERR_TAIL);
        let tail: String = stderr.chars().skip(skip).collect();
        return Err(StakeError::Prover(format!(
            "exited with {}: {}",
            output.status,
            tail.trim()
        )));
    }
    let json = fs::read(&out).await.map_err(|source| StakeError::Io {
        what: "reading",
        path: out.clone(),
        source,
    })?;
    Ok(serde_json::from_slice(&json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_is_due_after_each_deadline() {
        let info = |status| GameView {
            operator: Default::default(),
            token: Default::default(),
            stake: Default::default(),
            minSeats: 5,
            maxSeats: 5,
            joinBy: 100,
            playBy: 200,
            rulesHash: B256::ZERO,
            seedCommit: B256::ZERO,
            encKey: B256::ZERO,
            status,
            seatCount: 5,
            rosterHash: B256::ZERO,
            randomness: B256::ZERO,
            opLogHash: B256::ZERO,
            nextRound: 0,
            winnerTeam: 0,
            winners: U256::ZERO,
            transcriptRoot: B256::ZERO,
            challengeEnds: 300,
            proofDeadline: 400,
            challenger: Default::default(),
            proven: false,
            openDisputes: 0,
        };
        assert!(!due(&info(Status::Open), 100));
        assert!(due(&info(Status::Open), 101));
        assert!(!due(&info(Status::Started), 200));
        assert!(due(&info(Status::Started), 201));
        assert!(!due(&info(Status::Proposed), 299));
        assert!(due(&info(Status::Proposed), 300));
        assert!(due(&info(Status::Challenged), 400));
        assert!(!due(&info(Status::Settled), 1_000));
    }
}
