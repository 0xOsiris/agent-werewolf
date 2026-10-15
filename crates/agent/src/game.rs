//! Joins games and plays turns until the requested number of games is done.

use crate::{
    backoff::Backoff,
    brain::Brain,
    client::GameClient,
    http::HttpError,
    reply::{Decision, is_speak},
    stake::{Seat, StakeError, Terms, Wallet},
    watchtower::{self, Watch},
};
use alloy_primitives::U256;
use anyhow::{Context, bail};
use reqwest::StatusCode;
use std::time::Duration;
use tokio::{
    task::{JoinError, JoinSet},
    time::{Instant, sleep, timeout},
};
use tracing::{Instrument, Span, debug, error, info_span, warn};
use werewolf_protocol::{
    ActRequest, ActResponse, Action, JoinRequest, PlayerView, Poll, StakeInfo, StakeJoin, Team,
    Turn,
};

const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(10);
/// Long enough to ride out a server restart between games.
const JOIN_GIVE_UP: Duration = Duration::from_secs(180);
const OUTAGE_GIVE_UP: Duration = Duration::from_secs(120);
/// Caps a bogus deadline so `Instant` arithmetic can't overflow.
const MAX_TURN_MS: u64 = 3_600_000;
/// Pause before re-polling when the server re-offers a turn we already answered, so we don't spin.
const ANSWERED_TURN_PAUSE: Duration = Duration::from_secs(1);
/// A staked target move without a valid receipt by then is forced on-chain.
const RECEIPT_WAIT: Duration = Duration::from_secs(5);

pub struct Player {
    client: GameClient,
    brain: Brain,
    name: String,
    model: String,
    /// 0 plays forever.
    games: u32,
    /// Set for staked games.
    wallet: Option<Wallet>,
    /// Testing: send staked target moves only through `forceAction`.
    force_moves: bool,
    /// The staked seat in the current game.
    seat: Option<Seat>,
    /// The last staked game we finished, which the server keeps showing until the next lobby.
    finished: Option<U256>,
    /// Watchtowers and forced moves.
    background: JoinSet<()>,
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

/// What became of one act.
enum Delivered {
    Accepted(ActResponse),
    /// Stale, rejected, or past the deadline; already logged.
    Refused,
    /// No answer within [`RECEIPT_WAIT`].
    TimedOut,
    Ended(SessionEnd),
}

#[derive(Debug, thiserror::Error)]
enum JoinFailure {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("staked seat: {0}")]
    Stake(#[from] StakeError),
}

impl JoinFailure {
    fn is_retryable(&self) -> bool {
        match self {
            // 409 means a game is in progress; the next lobby will accept us.
            Self::Http(error) => {
                error.is_retryable() || error.status() == Some(StatusCode::CONFLICT)
            }
            Self::Stake(error) => error.is_retryable(),
        }
    }

    fn class(&self) -> &'static str {
        match self {
            Self::Http(error) => error.class(),
            Self::Stake(error) => error.class(),
        }
    }
}

impl Player {
    pub fn new(client: GameClient, brain: Brain, name: String, model: String, games: u32) -> Self {
        Self {
            client,
            brain,
            name,
            model,
            games,
            wallet: None,
            force_moves: false,
            seat: None,
            finished: None,
            background: JoinSet::new(),
        }
    }

    /// Enables staked games, which need a wallet.
    pub fn with_wallet(self, wallet: Wallet, force_moves: bool) -> Self {
        Self {
            wallet: Some(wallet),
            force_moves,
            ..self
        }
    }

    /// Plays, then waits for every watchtower so no payout is left unclaimed.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let result = self.play_games().await;
        while let Some(done) = self.background.join_next().await {
            log_background(done);
        }
        result
    }

    async fn play_games(&mut self) -> anyhow::Result<()> {
        let mut played = 0;
        // Shared across sessions so a server that keeps rejecting fresh tokens still hits the outage limit.
        let mut outage = Backoff::new(RETRY_BASE, RETRY_CAP, OUTAGE_GIVE_UP);
        loop {
            let token = self.join().await?;
            match self.play(&token, &mut outage).await? {
                SessionEnd::Finished { winner, view } => {
                    played += 1;
                    print_result(&self.name, winner, &view);
                    if let Some(seat) = self.seat.take() {
                        self.finished = Some(seat.terms.game_id());
                        self.watch(seat);
                    }
                    while let Some(done) = self.background.try_join_next() {
                        log_background(done);
                    }
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
    async fn join(&mut self) -> anyhow::Result<String> {
        let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP, JOIN_GIVE_UP);
        loop {
            let error = match self.try_join().await {
                Ok(token) => return Ok(token),
                Err(error) => error,
            };
            if !error.is_retryable() {
                bail!("could not join {}: {error}", self.client.base());
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

    async fn try_join(&mut self) -> Result<String, JoinFailure> {
        let info = self.client.info().await?;
        let stake = match &info.stake {
            Some(stake) => Some(self.stake(stake).await?),
            None => None,
        };
        let request = JoinRequest {
            name: self.name.clone(),
            model: Some(self.model.clone()),
            human: false,
            stake,
        };
        let joined = self.client.join(&request).await?;
        if let Some(seat) = &self.seat
            && seat.seat != joined.player_id
        {
            error!(
                seat = seat.seat,
                player_id = joined.player_id,
                "server gave us a different seat than the escrow; our signatures will not verify"
            );
        }
        Ok(joined.token)
    }

    /// Takes or resumes a staked seat and returns the join proof.
    async fn stake(&mut self, info: &StakeInfo) -> Result<StakeJoin, StakeError> {
        let terms = Terms::parse(info)?;
        let game_id = terms.game_id();
        if self.finished == Some(game_id) {
            return Err(StakeError::NotOpen(format!(
                "game {game_id} is over; waiting for the next lobby"
            )));
        }
        let Some(wallet) = &mut self.wallet else {
            return Err(StakeError::Setup(
                "this server runs staked games; pass --wallet-key-file".into(),
            ));
        };
        let chain = wallet.connect(&terms).await?;
        // A seat left behind by a server reset mid-game still has a payout to collect.
        if let Some(old) = self.seat.take_if(|seat| seat.terms.domain != terms.domain) {
            self.watch(old);
        }
        let (seat, join) = Seat::take(chain, terms).await?;
        // A re-join after a reset keeps the receipts collected so far.
        self.seat.get_or_insert(seat);
        Ok(join)
    }

    fn watch(&mut self, seat: Seat) {
        let (chain, terms, receipts) = seat.into_receipts();
        let watch = Watch {
            name: self.name.clone(),
            server: self.client.clone(),
            chain,
            terms,
            receipts,
        };
        self.background
            .spawn(watchtower::watch(watch).instrument(Span::current()));
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
            log_hash: None,
            signature: None,
        };
        // Only staked target turns have a round to force.
        let round = turn.round.filter(|_| self.seat.is_some());
        if let Some(round) = round
            && self.force_moves
        {
            self.force(round, &request.action, "testing --force-moves");
            return Ok(None);
        }
        self.sign(&turn, &mut request)?;
        let delivery = self.deliver(token, &turn, &mut request, deadline, outage);
        let delivered = match round {
            Some(_) => timeout(RECEIPT_WAIT, delivery)
                .await
                .unwrap_or(Ok(Delivered::TimedOut))?,
            None => delivery.await?,
        };
        let Some(seat) = &mut self.seat else {
            return Ok(delivered.into_end());
        };
        let checked = match &delivered {
            Delivered::Accepted(ActResponse {
                receipt: Some(receipt),
            }) => Some(seat.keep_receipt(&turn, &request.action, receipt)),
            _ => None,
        };
        let failure = match (checked, &delivered) {
            (Some(Ok(())), _) => None,
            (Some(Err(error)), _) => Some(error.to_string()),
            // Passes and rate-limited chat get no receipt, and chat can't be forced.
            (None, _) if round.is_none() => None,
            (None, Delivered::Accepted(_)) => Some("accepted without a receipt".to_owned()),
            (None, Delivered::TimedOut) => {
                Some(format!("no answer within {}s", RECEIPT_WAIT.as_secs()))
            }
            (None, Delivered::Refused | Delivered::Ended(_)) => {
                Some("the server did not take it".to_owned())
            }
        };
        match (failure, round) {
            (Some(failure), Some(round)) => self.force(round, &request.action, &failure),
            (Some(failure), None) => {
                warn!(error_class = "bad_receipt", %failure, "operator's chat receipt does not check out")
            }
            (None, _) => {}
        }
        Ok(delivered.into_end())
    }

    /// Sends `request` until the server takes it, refuses it, or the session ends.
    async fn deliver(
        &mut self,
        token: &str,
        turn: &Turn,
        request: &mut ActRequest,
        deadline: Instant,
        outage: &mut Backoff,
    ) -> anyhow::Result<Delivered> {
        let fallback = Decision::fallback(turn.kind).action;
        loop {
            let error = match self.client.act(token, request).await {
                Ok(response) => {
                    outage.reset();
                    return Ok(Delivered::Accepted(response));
                }
                Err(error) => error,
            };
            match error.status() {
                Some(StatusCode::UNAUTHORIZED) => {
                    return Ok(Delivered::Ended(SessionEnd::TokenRejected));
                }
                // Chat windows routinely close while the model is still thinking.
                Some(StatusCode::CONFLICT) if is_speak(turn.kind) => {
                    debug!(error_class = "stale_turn", %error, "chat window closed before our message landed");
                    return Ok(Delivered::Refused);
                }
                Some(StatusCode::CONFLICT) => {
                    warn!(error_class = "stale_turn", %error, "turn already closed; polling again");
                    return Ok(Delivered::Refused);
                }
                // The turn stays open after a 422; answer with the always-valid default so the table isn't held up.
                Some(StatusCode::UNPROCESSABLE_ENTITY) if request.action != fallback => {
                    warn!(error_class = error.class(), %error, "server rejected our action; sending fallback");
                    request.action = fallback.clone();
                    self.sign(turn, request)?;
                }
                _ if !error.is_retryable() => {
                    warn!(error_class = error.class(), %error, "server rejected our action; polling again");
                    return Ok(Delivered::Refused);
                }
                _ if Instant::now() >= deadline => {
                    warn!(error_class = error.class(), %error, "could not deliver action before the deadline");
                    return Ok(Delivered::Refused);
                }
                _ => pause(outage, "act", &error).await?,
            }
        }
    }

    /// Signs `request` in staked games.
    fn sign(&mut self, turn: &Turn, request: &mut ActRequest) -> anyhow::Result<()> {
        if let Some(seat) = &mut self.seat {
            seat.sign(turn, request).context("signing our action")?;
        }
        Ok(())
    }

    /// Forces `action` on-chain in the background, so a slow chain doesn't stall play.
    fn force(&mut self, round: u32, action: &Action, reason: &str) {
        let Some(seat) = &self.seat else { return };
        warn!(game_id = %seat.terms.game_id(), round, seat = seat.seat, reason, "no valid receipt; forcing the move on-chain");
        let forced = seat.force(round, action.clone());
        self.background.spawn(forced.instrument(Span::current()));
    }
}

impl Delivered {
    fn into_end(self) -> Option<SessionEnd> {
        match self {
            Self::Ended(end) => Some(end),
            Self::Accepted(_) | Self::Refused | Self::TimedOut => None,
        }
    }
}

fn log_background(done: Result<(), JoinError>) {
    if let Err(error) = done {
        error!(%error, "background task panicked");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockBrain;
    use axum::{
        Json, Router,
        routing::{get, post},
    };
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
            step: 0,
            round: None,
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
                "/v1/info",
                get(|| async {
                    Json(json!({
                        "version": 1,
                        "play_url": "http://fake/play",
                        "settings": {
                            "wolves": null, "sheriff": true, "cupid": true,
                            "discussion_secs": 120, "ai_chat_cooldown_secs": 15,
                            "ai_max_chat_messages": 3, "human_turn_timeout_secs": 60
                        }
                    }))
                }),
            )
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
        Player::new(client, brain, "Me".into(), "mock".into(), 1)
            .run()
            .await
            .unwrap();
        String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
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
