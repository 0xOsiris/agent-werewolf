//! Joins games and plays turns until the requested number of games is done.

use crate::{
    backoff::Backoff,
    brain::Brain,
    cast::Character,
    client::GameClient,
    http::HttpError,
    reply::{Decision, is_speak},
};
use anyhow::bail;
use rand::{rngs::StdRng, seq::IndexedRandom};
use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{Instrument, debug, info_span, warn};
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
/// The server's 409 body when another player already has the name.
const NAME_TAKEN: &str = "name already taken";

pub struct Player {
    client: GameClient,
    brain: Brain,
    /// Each game plays one of these at random.
    cast: Vec<Character>,
    model: String,
    /// 0 plays forever.
    games: u32,
    rng: StdRng,
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
    pub fn new(
        client: GameClient,
        brain: Brain,
        cast: Vec<Character>,
        model: String,
        games: u32,
        rng: StdRng,
    ) -> Self {
        Self {
            client,
            brain,
            cast,
            model,
            games,
            rng,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut played = 0;
        // Shared across sessions so a server that keeps rejecting fresh tokens still hits the outage limit.
        let mut outage = Backoff::new(RETRY_BASE, RETRY_CAP, OUTAGE_GIVE_UP);
        loop {
            let (token, character) = self.join().await?;
            self.brain.set_persona(character.persona.clone());
            let span = info_span!("game", player = %character.name);
            match self.play(&token, &mut outage).instrument(span).await? {
                SessionEnd::Finished { winner, view } => {
                    played += 1;
                    print_result(&character.name, winner, &view);
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

    /// Joins as a random character from the cast, waiting out a server that is down or mid-game.
    async fn join(&mut self) -> anyhow::Result<(String, Character)> {
        let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP, JOIN_GIVE_UP);
        // Characters whose names are already in this lobby; tried again only once all are taken.
        let mut taken: Vec<usize> = Vec::new();
        loop {
            let free: Vec<usize> = (0..self.cast.len())
                .filter(|i| !taken.contains(i))
                .collect();
            // The cast is never empty and `taken` never holds all of it, so `free` has a pick.
            let pick = free.choose(&mut self.rng).copied().unwrap_or(0);
            let character = self.cast[pick].clone();
            let request = JoinRequest {
                name: character.name.clone(),
                model: Some(self.model.clone()),
                human: false,
            };
            let error = match self.client.join(&request).await {
                Ok(joined) => return Ok((joined.token, character)),
                Err(error) => error,
            };
            if is_name_taken(&error) {
                taken.push(pick);
                if taken.len() < self.cast.len() {
                    debug!(name = %character.name, "name taken; trying another character");
                    continue;
                }
                // Every name is in use; back off, then draw from the whole cast again.
                taken.clear();
            }
            // 409 means a game is in progress or every name is taken; the next lobby will accept us.
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
                // Chat windows routinely close while the model is still thinking.
                Some(StatusCode::CONFLICT) if is_speak(turn.kind) => {
                    debug!(error_class = "stale_turn", %error, "chat window closed before our message landed");
                    return Ok(None);
                }
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

fn is_name_taken(error: &HttpError) -> bool {
    matches!(error, HttpError::Status { status: StatusCode::CONFLICT, body, .. } if body.contains(NAME_TAKEN))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockBrain;
    use axum::{
        Json, Router,
        response::IntoResponse,
        routing::{get, post},
    };
    use rand::SeedableRng;
    use serde_json::json;
    use std::{
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use werewolf_protocol::{JoinResponse, Phase, PublicPlayer, Role, TurnKind};

    /// Collects formatted log lines so tests can check levels.
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Fake game server: one turn of `kind`, a 409 on every act, then game over.
    async fn stale_act_server(kind: TurnKind) -> String {
        let player = |id, name: &str| PublicPlayer {
            id,
            name: name.into(),
            model: None,
            alive: true,
            revealed_role: None,
            human: false,
            look: 0,
        };
        let view = PlayerView {
            you: 0,
            role: Role::Villager,
            phase: Phase::Day { n: 1 },
            players: vec![player(0, "Me"), player(1, "Bob")],
            wolf_team: vec![],
            public_log: vec![],
            private_log: vec![],
        };
        let turn = Turn {
            turn_id: 1,
            kind,
            candidates: if kind == TurnKind::DayVote {
                vec![1]
            } else {
                vec![]
            },
            deadline_ms: 30_000,
            view: view.clone(),
        };
        let polls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1/join",
                post(|| async {
                    Json(JoinResponse {
                        player_id: 0,
                        token: "t".into(),
                    })
                }),
            )
            .route(
                "/v1/poll",
                get(move || {
                    let poll = if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                        Poll::Turn(turn.clone())
                    } else {
                        Poll::GameOver {
                            winner: Team::Village,
                            view: view.clone(),
                        }
                    };
                    async move { Json(poll) }
                }),
            )
            .route(
                "/v1/act",
                post(|| async {
                    (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "stale turn; poll again" })),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// Plays one game whose only turn is stale by the time we act, returning our logs.
    async fn logs_for_stale_act(kind: TurnKind) -> String {
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("werewolf_agent=debug")
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        // The current-thread test runtime keeps every task on this thread, so a thread-local subscriber sees them all.
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = GameClient::new(&stale_act_server(kind).await).unwrap();
        let brain = Brain::Mock(MockBrain::new(Some(1)));
        let cast = vec![Character::new("Me", None).unwrap()];
        Player::new(
            client,
            brain,
            cast,
            "mock".into(),
            1,
            StdRng::seed_from_u64(1),
        )
        .run()
        .await
        .unwrap();
        String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
    }

    #[tokio::test]
    async fn a_taken_name_draws_another_character() {
        let tried = Arc::new(Mutex::new(Vec::new()));
        let seen = tried.clone();
        let app = Router::new().route(
            "/v1/join",
            post(move |Json(req): Json<JoinRequest>| {
                seen.lock().unwrap().push(req.name.clone());
                async move {
                    if req.name == "Cleo" {
                        Json(JoinResponse {
                            player_id: 0,
                            token: "t".into(),
                        })
                        .into_response()
                    } else {
                        (
                            StatusCode::CONFLICT,
                            Json(json!({ "error": "name already taken" })),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cast = ["Ada", "Bob", "Cleo"].map(|n| Character::new(n, Some("p")).unwrap());
        for seed in 0..5 {
            tried.lock().unwrap().clear();
            let client = GameClient::new(&url).unwrap();
            let mut player = Player::new(
                client,
                Brain::Mock(MockBrain::new(Some(1))),
                cast.to_vec(),
                "mock".into(),
                1,
                StdRng::seed_from_u64(seed),
            );
            let (token, character) = player.join().await.unwrap();
            assert_eq!((token.as_str(), character.name.as_str()), ("t", "Cleo"));
            let tried = tried.lock().unwrap();
            let mut unique = tried.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                tried.len(),
                "a taken name was retried: {tried:?}"
            );
        }
    }

    #[tokio::test]
    async fn stale_chat_act_is_routine_but_stale_vote_warns() {
        let chat = logs_for_stale_act(TurnKind::DaySpeak).await;
        assert!(
            chat.contains("DEBUG") && chat.contains("chat window closed"),
            "{chat}"
        );
        assert!(!chat.contains("WARN"), "{chat}");

        let vote = logs_for_stale_act(TurnKind::DayVote).await;
        assert!(
            vote.contains("WARN") && vote.contains("turn already closed"),
            "{vote}"
        );
    }
}
