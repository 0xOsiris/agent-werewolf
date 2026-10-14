//! Joins games and plays turns until the requested number of games is done.

use crate::{backoff::Backoff, brain::Brain, client::GameClient, http::HttpError, reply::Decision};
use anyhow::bail;
use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{Instrument, info_span, warn};
use werewolf_protocol::{ActRequest, JoinRequest, PlayerView, Poll, Team, Turn};

const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(10);
/// Long enough to ride out a server restart between games.
const JOIN_GIVE_UP: Duration = Duration::from_secs(180);
const OUTAGE_GIVE_UP: Duration = Duration::from_secs(120);
/// Caps a bogus deadline so `Instant` arithmetic can't overflow.
const MAX_TURN_MS: u64 = 3_600_000;
/// Pause before re-polling when the server re-offers a turn we already answered, so we don't spin.
const ANSWERED_TURN_PAUSE: Duration = Duration::from_secs(1);

pub struct Player {
    client: GameClient,
    brain: Brain,
    name: String,
    model: String,
    /// 0 plays forever.
    games: u32,
}

/// Why a session (one join token) stopped.
enum SessionEnd {
    Finished {
        winner: Team,
        view: PlayerView,
    },
    /// The server no longer knows our token, usually because it was reset.
    TokenRejected,
}

impl Player {
    pub const fn new(
        client: GameClient,
        brain: Brain,
        name: String,
        model: String,
        games: u32,
    ) -> Self {
        Self {
            client,
            brain,
            name,
            model,
            games,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut played = 0;
        // Shared across sessions so a server that keeps rejecting fresh tokens still hits the outage limit.
        let mut outage = Backoff::new(RETRY_BASE, RETRY_CAP, OUTAGE_GIVE_UP);
        loop {
            let token = self.join().await?;
            match self.play(&token, &mut outage).await? {
                SessionEnd::Finished { winner, view } => {
                    played += 1;
                    print_result(&self.name, winner, &view);
                    if self.games != 0 && played >= self.games {
                        return Ok(());
                    }
                }
                SessionEnd::TokenRejected => {
                    let Some(delay) = outage.next_delay() else {
                        bail!(
                            "server kept rejecting fresh tokens for over {}s",
                            OUTAGE_GIVE_UP.as_secs()
                        );
                    };
                    warn!(
                        attempt = outage.attempt(),
                        error_class = "unauthorized",
                        "server rejected our token, probably reset; re-joining"
                    );
                    sleep(delay).await;
                }
            }
        }
    }

    /// Returns a bearer token, waiting out a server that is down or mid-game.
    async fn join(&self) -> anyhow::Result<String> {
        let request = JoinRequest {
            name: self.name.clone(),
            model: Some(self.model.clone()),
            human: false,
        };
        let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP, JOIN_GIVE_UP);
        loop {
            let error = match self.client.join(&request).await {
                Ok(joined) => return Ok(joined.token),
                Err(error) => error,
            };
            // 409 means a game is in progress; the next lobby will accept us.
            if !error.is_retryable() && error.status() != Some(StatusCode::CONFLICT) {
                bail!("server at {} refused the join: {error}", self.client.base());
            }
            let Some(delay) = backoff.next_delay() else {
                bail!(
                    "could not join {} within {}s; last error: {error}",
                    self.client.base(),
                    JOIN_GIVE_UP.as_secs()
                );
            };
            warn!(attempt = backoff.attempt(), error_class = error.class(), %error, "join failed; retrying");
            sleep(delay).await;
        }
    }

    async fn play(&mut self, token: &str, outage: &mut Backoff) -> anyhow::Result<SessionEnd> {
        let mut answered = None;
        loop {
            let poll = match self.client.poll(token).await {
                Ok(poll) => {
                    outage.reset();
                    poll
                }
                Err(error) if error.status() == Some(StatusCode::UNAUTHORIZED) => {
                    return Ok(SessionEnd::TokenRejected);
                }
                Err(error) => {
                    pause(outage, "poll", &error).await?;
                    continue;
                }
            };
            match poll {
                Poll::Wait => {}
                Poll::Turn(turn) if answered == Some(turn.turn_id) => {
                    sleep(ANSWERED_TURN_PAUSE).await
                }
                Poll::Turn(turn) => {
                    answered = Some(turn.turn_id);
                    let span = info_span!("turn", turn_id = turn.turn_id, kind = ?turn.kind);
                    if let Some(end) = self.take_turn(token, turn, outage).instrument(span).await? {
                        return Ok(end);
                    }
                }
                Poll::GameOver { winner, view } => {
                    return Ok(SessionEnd::Finished { winner, view });
                }
            }
        }
    }

    /// Decides and delivers one action; `Some` only if the session ended meanwhile.
    async fn take_turn(
        &mut self,
        token: &str,
        turn: Turn,
        outage: &mut Backoff,
    ) -> anyhow::Result<Option<SessionEnd>> {
        let deadline = deadline_instant(turn.deadline_ms);
        let decision = self.brain.decide(&turn, deadline).await;
        let mut request = ActRequest {
            turn_id: turn.turn_id,
            action: decision.action,
            reasoning: decision.reasoning,
        };
        let fallback = Decision::fallback(turn.kind).action;
        loop {
            let error = match self.client.act(token, &request).await {
                Ok(()) => {
                    outage.reset();
                    return Ok(None);
                }
                Err(error) => error,
            };
            match error.status() {
                Some(StatusCode::UNAUTHORIZED) => return Ok(Some(SessionEnd::TokenRejected)),
                Some(StatusCode::CONFLICT) => {
                    warn!(error_class = "stale_turn", %error, "turn already closed; polling again");
                    return Ok(None);
                }
                // The turn stays open after a 422; answer with the always-valid default so the table isn't held up.
                Some(StatusCode::UNPROCESSABLE_ENTITY) if request.action != fallback => {
                    warn!(error_class = error.class(), %error, "server rejected our action; sending fallback");
                    request.action = fallback.clone();
                }
                _ if !error.is_retryable() => {
                    warn!(error_class = error.class(), %error, "server rejected our action; polling again");
                    return Ok(None);
                }
                _ if Instant::now() >= deadline => {
                    warn!(error_class = error.class(), %error, "could not deliver action before the deadline");
                    return Ok(None);
                }
                _ => pause(outage, "act", &error).await?,
            }
        }
    }
}

/// Sleeps before retrying a failed call, or fails once the server has been failing for too long.
async fn pause(outage: &mut Backoff, call: &'static str, error: &HttpError) -> anyhow::Result<()> {
    let Some(delay) = outage.next_delay() else {
        bail!(
            "game server failing for over {}s; last {call} error: {error}",
            OUTAGE_GIVE_UP.as_secs()
        );
    };
    warn!(call, attempt = outage.attempt(), error_class = error.class(), %error, "game server call failed; retrying");
    sleep(delay).await;
    Ok(())
}

/// The server sends milliseconds remaining, so this is immune to clock skew between machines.
fn deadline_instant(deadline_ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(deadline_ms.min(MAX_TURN_MS))
}

fn print_result(name: &str, winner: Team, view: &PlayerView) {
    let survived = view.players.iter().any(|p| p.id == view.you && p.alive);
    let winner = format!("{winner:?}").to_lowercase();
    let role = format!("{:?}", view.role).to_lowercase();
    println!("{name}: game over, winner={winner} role={role} survived={survived}");
}
