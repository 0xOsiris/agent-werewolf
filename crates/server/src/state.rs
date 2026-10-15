//! Shared server state, where HTTP handlers and the game driver meet.

use std::{borrow::Cow, num::NonZeroUsize, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use rand::{
    TryRng,
    rngs::{SysError, SysRng},
};
use tokio::{
    sync::{Notify, broadcast, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use werewolf_engine::{Answer, Game, GameEvent, Setup, TurnSpec, players_needed};
use werewolf_protocol::{
    ActRequest, ActResponse, Action, JoinRequest, JoinResponse, MAX_MODEL_LEN, MAX_NAME_LEN,
    MAX_TEXT_LEN, Phase, PlayerId, PlayerView, Poll, PublicEvent, PublicPlayer, Receipt, Role,
    SettingsBody, StakeInfo, Team, Turn, TurnKind,
};
use werewolf_verify::{
    Transcript,
    alloy_primitives::U256,
    encode,
    transcript::{BuildError, Header, Rules},
};

use crate::{
    chat::{ChatRules, Limits},
    config::{Config, STAKED_WOLVES},
    driver,
    error::HttpError,
    game_log::{GameLog, GameSummary, PlayerSummary, ShotSummary},
    sanitize,
    settings::Settings,
    site::Site,
    spectator::{Broadcast, ChatState, PendingTurn, RoleEntry, SpectatorEvent, Viewer},
    stake::{self, During, Failure, Force, Ledger, OnChain, Signed, Stake, StakeError, Started},
};

/// Events a slow spectator may fall behind before it gets a fresh snapshot.
const EVENT_BUFFER: usize = 1024;
/// How long a staked chat act waits for the driver to post it and sign its receipt.
const CHAT_RECEIPT_WAIT: Duration = Duration::from_secs(5);
/// Bound on the chain reads behind `/v1/join` and `/admin/start`.
const RPC_WAIT: Duration = Duration::from_secs(10);
/// Bound on the chain read behind `/readyz`.
const READY_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Lobby,
    /// Staked games: `start` is on its way to the chain.
    Starting,
    Running,
    Over,
}

/// The escrow game behind the lobby.
enum Escrow {
    /// Free games.
    None,
    Creating,
    /// `create` failed; a reset tries again.
    Failed,
    Ready(Arc<OnChain>),
}

pub struct AppState {
    config: Config,
    spectator_key: String,
    seed: u64,
    site: Site,
    inner: Mutex<Inner>,
    /// Bumped whenever a player's poll result may have changed.
    changed: watch::Sender<u64>,
    /// Wakes the driver when a player answers.
    answered: Notify,
    shutdown: CancellationToken,
    /// Staked games only.
    stake: Option<Arc<Stake>>,
}

struct Inner {
    status: Status,
    /// Phase as last shown to spectators; can trail the engine during pacing.
    phase: Phase,
    winner: Option<Team>,
    seats: Vec<Seat>,
    roles: Vec<Role>,
    feed: Vec<SpectatorEvent>,
    /// The open chat window and when it ends.
    chat: Option<(TurnKind, Instant)>,
    /// Host-adjustable settings; they survive resets and apply at each start.
    settings: Settings,
    next_turn_id: u64,
    /// Orders answers by arrival, so chat posts publish in the order they came in.
    next_answer_seq: u64,
    /// Step and round numbers of free games; staked games take them from the ledger.
    next_step: u32,
    next_round: u32,
    games_started: u64,
    log: Option<GameLog>,
    events: broadcast::Sender<Arc<Broadcast>>,
    escrow: Escrow,
    /// The running staked game's signed record.
    ledger: Option<Ledger>,
    /// The latest staked game's transcript JSON.
    transcript: Option<Arc<[u8]>>,
}

/// Who asks to watch the event stream, before their token is checked.
pub enum Watcher {
    Spectator,
    Audience,
    Player(String),
}

/// A filtered snapshot plus a receiver for every event after it.
pub struct Subscription {
    pub viewer: Viewer,
    pub snapshot: String,
    /// Roles by seat, for filtering the events that follow.
    pub roles: Vec<Role>,
    pub over: bool,
    pub events: broadcast::Receiver<Arc<Broadcast>>,
}

struct Seat {
    public: PublicPlayer,
    token: String,
    pending: Option<Pending>,
    answer: Option<Submitted>,
    final_view: Option<PlayerView>,
    timeouts: u32,
    invalid_actions: u32,
}

struct Pending {
    turn: Turn,
    deadline: Instant,
}

/// An accepted answer waiting for the driver.
struct Submitted {
    action: Action,
    reasoning: Option<String>,
    seq: u64,
    signed: Option<Signed>,
    reply: Option<oneshot::Sender<Option<Receipt>>>,
}

/// One turn's outcome as handed to the driver.
pub struct Collected {
    pub answer: Answer,
    pub kind: TurnKind,
    pub reasoning: Option<String>,
    /// Staked games: the signature behind `answer`.
    pub signed: Option<Signed>,
    /// Staked chat: takes the receipt once posted, or `None` if the window dropped it.
    pub reply: Option<oneshot::Sender<Option<Receipt>>>,
}

/// When an accepted act gets its receipt.
#[derive(Debug)]
enum ReceiptPlan {
    /// Free games, and passes, which never enter the transcript.
    None,
    /// A signed target answer always enters the transcript, so it is receipted at once.
    Sign(Signed),
    /// Chat enters the transcript only once the driver posts it.
    Await(oneshot::Receiver<Option<Receipt>>),
}

impl AppState {
    pub fn new(
        config: Config,
        spectator_key: String,
        seed: u64,
        site: Site,
        shutdown: CancellationToken,
        stake: Option<Stake>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let settings = Settings::from_config(&config);
        let escrow = if stake.is_some() {
            Escrow::Creating
        } else {
            Escrow::None
        };
        Arc::new(Self {
            config,
            spectator_key,
            seed,
            site,
            inner: Mutex::new(Inner {
                status: Status::Lobby,
                phase: Phase::Lobby,
                winner: None,
                seats: Vec::new(),
                roles: Vec::new(),
                feed: Vec::new(),
                chat: None,
                settings,
                next_turn_id: 1,
                next_answer_seq: 0,
                next_step: 0,
                next_round: 0,
                games_started: 0,
                log: None,
                events,
                escrow,
                ledger: None,
                transcript: None,
            }),
            changed: watch::Sender::new(0),
            answered: Notify::new(),
            shutdown,
            stake: stake.map(Arc::new),
        })
    }

    /// Creates the first lobby's escrow game; a no-op for free games.
    pub fn open_lobby(self: &Arc<Self>) {
        let mut inner = self.inner.lock();
        self.open_escrow(&mut inner);
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub fn site(&self) -> &Site {
        &self.site
    }

    pub fn check_key(&self, key: Option<&str>) -> Result<(), HttpError> {
        if key == Some(self.spectator_key.as_str()) {
            Ok(())
        } else {
            Err(HttpError::BadKey)
        }
    }

    pub fn join(self: &Arc<Self>, req: JoinRequest) -> Result<JoinResponse, HttpError> {
        let (name, model) = player_fields(&req)?;
        let token = new_token()?;

        let mut inner = self.inner.lock();
        if inner.status != Status::Lobby {
            return Err(HttpError::conflict(
                "game already started; wait for the next lobby",
            ));
        }
        if inner.seats.len() >= self.config.max_players {
            return Err(HttpError::conflict("lobby is full"));
        }
        let folded = name.to_lowercase();
        if inner
            .seats
            .iter()
            .any(|s| s.public.name.to_lowercase() == folded)
        {
            return Err(HttpError::conflict("name already taken"));
        }

        let player_id = inner.seats.len() as PlayerId;
        inner.seat_up(player_id, name, model, req.human, token.clone());

        if self
            .config
            .auto_start
            .is_some_and(|n| inner.seats.len() >= n)
            && let Err(err) = self.start_locked(&mut inner)
        {
            warn!(%err, "auto-start failed");
        }
        Ok(JoinResponse { player_id, token })
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), HttpError> {
        match &self.stake {
            None => self.start_locked(&mut self.inner.lock()),
            Some(stake) => self.start_staked(Arc::clone(stake)).await,
        }
    }

    fn start_locked(self: &Arc<Self>, inner: &mut Inner) -> Result<(), HttpError> {
        if inner.status != Status::Lobby {
            return Err(HttpError::conflict(
                "game already started; reset once it is over",
            ));
        }
        let (min, have) = (self.config.min_players, inner.seats.len());
        if have < min {
            return Err(HttpError::conflict(format!(
                "need at least {min} players, have {have}"
            )));
        }
        let seed = self.seed.wrapping_add(inner.games_started);
        let roster = inner.seats.iter().map(|s| s.public.clone()).collect();
        let (game, events) = Game::new(roster, inner.settings.setup, deal_seed(seed))
            .map_err(|err| HttpError::conflict(err.to_string()))?;
        self.launch(inner, game, events, seed, None);
        Ok(())
    }

    /// Runs a dealt game: logs and publishes the deal, then hands the game to the driver.
    fn launch(
        self: &Arc<Self>,
        inner: &mut Inner,
        game: Game,
        events: Vec<GameEvent>,
        seed: u64,
        staked: Option<driver::Staked>,
    ) {
        inner.games_started += 1;
        inner.status = Status::Running;
        inner.next_step = 0;
        inner.next_round = 0;
        let log = GameLog::create(&self.config.log_dir, seed);
        // Replay the lobby so each log file stands alone.
        for seat in &inner.seats {
            if let Some(line) = to_json(&SpectatorEvent::PlayerJoined {
                player: seat.public.clone(),
            }) {
                log.append(line);
            }
        }
        inner.log = Some(log);
        inner.roles = game.roles().to_vec();
        let roles = role_entries(&inner.roles);
        inner.publish(SpectatorEvent::Roles { roles });
        for event in events {
            inner.publish(event.into());
        }

        let game = tokio::spawn(driver::run(Arc::clone(self), game, seed, staked));
        let app = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = game.await
                && err.is_panic()
            {
                error!(%err, "game driver panicked; game aborted, reset to continue");
                app.abort();
            }
        });
    }

    /// Claims the lobby for `start`, checks the chain allows it, then starts in the background.
    async fn start_staked(self: &Arc<Self>, stake: Arc<Stake>) -> Result<(), HttpError> {
        let game = {
            let mut inner = self.inner.lock();
            if inner.status != Status::Lobby {
                return Err(HttpError::conflict(
                    "game already started; reset once it is over",
                ));
            }
            let game = inner.escrow_game()?;
            inner.status = Status::Starting;
            game
        };
        if let Err(err) = check_startable(&stake, &game).await {
            let mut inner = self.inner.lock();
            if inner.status == Status::Starting {
                inner.status = Status::Lobby;
            }
            return Err(err);
        }
        let app = Arc::clone(self);
        tokio::spawn(async move {
            let started = stake.start(&game).await.during("start");
            app.launch_staked(stake, game, started);
        });
        Ok(())
    }

    /// Deals from the on-chain roster and randomness and starts the driver.
    fn launch_staked(
        self: &Arc<Self>,
        stake: Arc<Stake>,
        game: Arc<OnChain>,
        started: Result<Started, Failure>,
    ) {
        let started = match started {
            Ok(started) => started,
            Err(failure) => return self.halt(&game, &failure),
        };
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        if inner.status != Status::Starting {
            error!(game = %game.id, status = ?inner.status, "started on-chain, but the lobby moved on");
            return;
        }
        // Seats that never joined over HTTP still play; all their turns time out.
        let mut joined = std::mem::take(&mut inner.seats);
        for id in (0..).take(started.seats.len()) {
            match joined.iter().position(|s| s.public.id == id) {
                Some(index) => inner.seats.push(joined.swap_remove(index)),
                None => inner.seat_up(id, format!("Seat {id}"), None, false, String::new()),
            }
        }
        let header = Header {
            chain_id: game.domain.chain_id,
            escrow: game.domain.escrow,
            game_id: game.id,
            rules: game.rules,
            seats: started.seats,
            names: inner.seats.iter().map(|s| s.public.name.clone()).collect(),
            seed: game.seed(),
            randomness: started.randomness,
            enc_key: game.enc_key(),
        };
        let deal = encode::deal_seed(&game.seed(), &started.randomness, &started.roster_hash);
        let setup = Setup {
            wolves: NonZeroUsize::new(game.rules.wolves.into()),
            sheriff: game.rules.sheriff,
            cupid: game.rules.cupid,
        };
        let roster = inner.seats.iter().map(|s| s.public.clone()).collect();
        let dealt = Game::new(roster, setup, deal)
            .during("deal")
            .and_then(|(dealt, events)| {
                let ledger = Ledger::new(header, &events).during("deal")?;
                Ok((dealt, events, ledger))
            });
        let (dealt, events, ledger) = match dealt {
            Ok(dealt) => dealt,
            Err(failure) => {
                drop(guard);
                return self.halt(&game, &failure);
            }
        };
        inner.ledger = Some(ledger);
        let staked = driver::Staked {
            stake,
            cursor: started.cursor,
            game: Arc::clone(&game),
        };
        // Names the game's log files and seeds its chat jitter.
        let seed = game.id.saturating_to::<u64>();
        self.launch(inner, dealt, events, seed, Some(staked));
        drop(guard);
        self.bump();
    }

    /// A staked join: checks the session-key proof and the on-chain seat. Rejoining the same seat,
    /// at any point in the game, issues a fresh token.
    pub async fn join_staked(
        self: &Arc<Self>,
        req: JoinRequest,
    ) -> Result<JoinResponse, HttpError> {
        let (Some(stake), Some(proof)) = (&self.stake, &req.stake) else {
            return Err(HttpError::invalid("this game needs a stake; see /play"));
        };
        let (name, model) = player_fields(&req)?;
        let game = self.inner.lock().escrow_game()?;
        let (address, session_key) =
            stake::check_join(&game.domain, proof).map_err(HttpError::Invalid)?;
        let (players, keys) = tokio::time::timeout(RPC_WAIT, stake.client.seats(game.id))
            .await
            .map_err(|_| HttpError::unavailable("the chain RPC timed out; try again"))?
            .map_err(|err| {
                let (class, attempts, _) = StakeError::from(err).fields();
                warn!(game = %game.id, op = "seats", class, ?attempts, "join could not read the escrow");
                HttpError::unavailable("could not read the escrow; try again")
            })?;
        let seat = players.iter().position(|p| *p == address).ok_or_else(|| {
            HttpError::invalid(format!(
                "{address:#x} holds no seat in staked game {}; join the escrow first",
                game.id
            ))
        })?;
        if keys.get(seat) != Some(&session_key) {
            return Err(HttpError::invalid(format!(
                "this session key is not the one seat {seat} registered on-chain"
            )));
        }
        // The escrow allows at most 12 seats.
        let player_id = seat as PlayerId;
        let token = new_token()?;

        let mut inner = self.inner.lock();
        if !matches!(&inner.escrow, Escrow::Ready(current) if current.id == game.id) {
            return Err(HttpError::conflict(
                "the staked game changed while you joined; read /v1/info and join again",
            ));
        }
        if let Some(existing) = inner.seats.iter_mut().find(|s| s.public.id == player_id) {
            existing.token = token.clone();
            return Ok(JoinResponse { player_id, token });
        }
        if !matches!(inner.status, Status::Lobby | Status::Starting) {
            return Err(HttpError::conflict(
                "game already started; wait for the next lobby",
            ));
        }
        let folded = name.to_lowercase();
        if inner
            .seats
            .iter()
            .any(|s| s.public.name.to_lowercase() == folded)
        {
            return Err(HttpError::conflict("name already taken"));
        }
        inner.seat_up(player_id, name, model, req.human, token.clone());
        let auto_start = inner.status == Status::Lobby
            && self
                .config
                .auto_start
                .is_some_and(|n| inner.seats.len() >= n);
        drop(inner);
        if auto_start {
            let app = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(err) = app.start().await {
                    warn!(%err, "auto-start failed");
                }
            });
        }
        Ok(JoinResponse { player_id, token })
    }

    /// Creates a new escrow game for the lobby in the background; a no-op for free games.
    fn open_escrow(self: &Arc<Self>, inner: &mut Inner) {
        let Some(stake) = &self.stake else { return };
        inner.escrow = Escrow::Creating;
        let setup = inner.settings.setup;
        let wolves = setup.wolves.map_or(STAKED_WOLVES, NonZeroUsize::get);
        let rules = Rules {
            // Config and settings validation keep the wolves within the 12 seats.
            wolves: u8::try_from(wolves).unwrap_or(u8::MAX),
            sheriff: setup.sheriff,
            cupid: setup.cupid,
        };
        let need = players_needed(wolves, setup.sheriff, setup.cupid);
        let min_seats = u8::try_from(self.config.min_players.max(need)).unwrap_or(u8::MAX);
        let max_seats = u8::try_from(self.config.max_players).unwrap_or(u8::MAX);
        let (app, stake) = (Arc::clone(self), Arc::clone(stake));
        tokio::spawn(async move {
            let created = stake.create(rules, min_seats, max_seats).await;
            app.escrow_created(stake, created.during("create"));
        });
    }

    fn escrow_created(self: &Arc<Self>, stake: Arc<Stake>, created: Result<OnChain, Failure>) {
        let mut inner = self.inner.lock();
        match created {
            Ok(game) => {
                let game = Arc::new(game);
                println!("staked game {} created: join by {}", game.id, game.join_by);
                let (app, id) = (Arc::clone(self), game.id);
                tokio::spawn(stake::settle(
                    stake,
                    Arc::clone(&game),
                    self.shutdown.clone(),
                    move || app.escrow_cancelled(id),
                ));
                inner.escrow = Escrow::Ready(game);
            }
            Err(failure) => {
                failure.log(None);
                inner.escrow = Escrow::Failed;
                inner.publish(SpectatorEvent::Error {
                    message: "could not create the staked game on-chain; reset to try again"
                        .to_owned(),
                });
            }
        }
    }

    /// Settlement cancelled game `id` because joins closed before it started. If it still backs
    /// the lobby, opens a new lobby with a new game.
    fn escrow_cancelled(self: &Arc<Self>, id: U256) {
        let mut inner = self.inner.lock();
        let current = matches!(&inner.escrow, Escrow::Ready(game) if game.id == id);
        if !current || inner.status != Status::Lobby {
            return;
        }
        inner.clear();
        inner.publish(SpectatorEvent::Error {
            message: format!(
                "joins for staked game {id} closed before it started; it is cancelled and every \
                 stake can be claimed back. A new game is being created."
            ),
        });
        self.open_escrow(&mut inner);
        drop(inner);
        self.bump();
    }

    /// Stops staked game `game` after `failure`; its deadlines settle it on-chain.
    pub(crate) fn halt(&self, game: &OnChain, failure: &Failure) {
        failure.log(Some(game.id));
        let mut inner = self.inner.lock();
        let current = matches!(&inner.escrow, Escrow::Ready(g) if g.id == game.id);
        if !current {
            return;
        }
        inner.publish(SpectatorEvent::Error {
            message: failure.public_message(game.id),
        });
        if matches!(inner.status, Status::Starting | Status::Running) {
            for seat in &mut inner.seats {
                seat.pending = None;
                // Drops chat receipt replies, so waiting acts get a 503.
                seat.answer = None;
            }
            inner.status = Status::Over;
            inner.log = None;
            inner.ledger = None;
        }
        drop(inner);
        self.bump();
    }

    /// The lobby game's escrow terms, once it exists on-chain.
    pub fn stake_info(&self) -> Option<StakeInfo> {
        match &self.inner.lock().escrow {
            Escrow::Ready(game) => Some(game.info.clone()),
            Escrow::None | Escrow::Creating | Escrow::Failed => None,
        }
    }

    /// Readiness: in staked games the chain RPC must answer and the lobby game must exist.
    pub async fn ready(&self) -> Result<(), String> {
        let Some(stake) = &self.stake else {
            return Ok(());
        };
        if matches!(self.inner.lock().escrow, Escrow::Failed) {
            return Err("the staked game could not be created; reset to retry".to_owned());
        }
        match tokio::time::timeout(READY_WAIT, stake.client.block_number()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => {
                let (class, ..) = StakeError::from(err).fields();
                Err(format!("escrow RPC failed ({class})"))
            }
            Err(_) => Err(format!("escrow RPC gave no answer within {READY_WAIT:?}")),
        }
    }

    /// The latest staked game's transcript, or game `id`'s from disk.
    pub async fn transcript(&self, id: Option<U256>) -> Result<Arc<[u8]>, HttpError> {
        let Some(id) = id else {
            return self
                .inner
                .lock()
                .transcript
                .clone()
                .ok_or(HttpError::NotFound);
        };
        let stake = self.stake.as_ref().ok_or(HttpError::NotFound)?;
        let path = stake.transcript_path(id);
        match tokio::fs::read(&path).await {
            Ok(json) => Ok(json.into()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(HttpError::NotFound),
            Err(err) => {
                error!(path = %path.display(), %err, "cannot read a saved transcript");
                Err(HttpError::Internal)
            }
        }
    }

    pub fn reset(self: &Arc<Self>) -> Result<(), HttpError> {
        let mut inner = self.inner.lock();
        if matches!(inner.status, Status::Running | Status::Starting) {
            return Err(HttpError::conflict(
                "game in progress; reset once it is over",
            ));
        }
        // An unstarted escrow game carries over, so its on-chain seats can join again.
        let keep = inner.status == Status::Lobby
            && matches!(inner.escrow, Escrow::Creating | Escrow::Ready(_));
        inner.clear();
        if !keep {
            self.open_escrow(&mut inner);
        }
        drop(inner);
        // Wakes pollers holding old tokens so they get 401 and re-join.
        self.bump();
        Ok(())
    }

    pub fn settings(&self) -> SettingsBody {
        self.inner.lock().settings.body()
    }

    /// Replaces the lobby settings; they apply at the next start.
    pub fn update_settings(&self, body: SettingsBody) -> Result<SettingsBody, HttpError> {
        let mut inner = self.inner.lock();
        if inner.status != Status::Lobby {
            return Err(HttpError::conflict(
                "settings can only change in the lobby; reset first",
            ));
        }
        if self.stake.is_some() {
            if body.wolves.is_none() {
                return Err(HttpError::invalid("staked games need a fixed wolf count"));
            }
            let current = inner.settings.body();
            let rules = |b: &SettingsBody| (b.wolves, b.sheriff, b.cupid);
            if rules(&body) != rules(&current)
                && matches!(inner.escrow, Escrow::Creating | Escrow::Ready(_))
            {
                return Err(HttpError::invalid(
                    "wolves, sheriff and cupid are fixed once the staked game exists on-chain; \
                     restart the server with new flags to change them",
                ));
            }
        }
        inner.settings =
            Settings::from_body(body, self.config.max_players).map_err(HttpError::Invalid)?;
        let settings = inner.settings.body();
        inner.publish(SpectatorEvent::Settings { settings });
        Ok(settings)
    }

    /// Long-polls for this player's next turn or the game result.
    pub async fn poll(&self, token: &str, wait: Duration) -> Result<Poll, HttpError> {
        let deadline = Instant::now() + wait;
        let mut changed = self.changed.subscribe();
        loop {
            if let Some(poll) = self.poll_now(token)? {
                return Ok(poll);
            }
            tokio::select! {
                res = changed.changed() => if res.is_err() { return Ok(Poll::Wait) },
                () = tokio::time::sleep_until(deadline) => return Ok(Poll::Wait),
                () = self.shutdown.cancelled() => return Ok(Poll::Wait),
            }
        }
    }

    fn poll_now(&self, token: &str) -> Result<Option<Poll>, HttpError> {
        let inner = self.inner.lock();
        let seat = inner.seat(token).ok_or(HttpError::Unauthorized)?;
        if let (Some(winner), Some(view)) = (inner.winner, &seat.final_view) {
            return Ok(Some(Poll::GameOver {
                winner,
                view: view.clone(),
            }));
        }
        Ok(seat.pending.as_ref().map(|p| {
            Poll::Turn(Turn {
                deadline_ms: remaining_ms(p.deadline),
                ..p.turn.clone()
            })
        }))
    }

    /// Validates and records an answer to the player's pending turn; staked games get a receipt
    /// once it is certain to enter the transcript.
    pub async fn act(
        &self,
        token: &str,
        body: Result<ActRequest, HttpError>,
    ) -> Result<ActResponse, HttpError> {
        let (player, plan) = self.accept(token, body)?;
        let receipt = match plan {
            ReceiptPlan::None => None,
            ReceiptPlan::Sign(signed) => Some(self.receipt(player, &signed)?),
            ReceiptPlan::Await(reply) => match tokio::time::timeout(CHAT_RECEIPT_WAIT, reply).await
            {
                Ok(Ok(receipt)) => receipt,
                Ok(Err(_)) => {
                    return Err(HttpError::unavailable(
                        "the game stopped before your message was posted",
                    ));
                }
                Err(_) => {
                    warn!(
                        player,
                        "a chat message was not posted within the receipt wait"
                    );
                    return Err(HttpError::unavailable(
                        "your message was not posted in time; poll again",
                    ));
                }
            },
        };
        Ok(ActResponse { receipt })
    }

    fn accept(
        &self,
        token: &str,
        body: Result<ActRequest, HttpError>,
    ) -> Result<(PlayerId, ReceiptPlan), HttpError> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let seq = inner.next_answer_seq;
        inner.next_answer_seq += 1;
        let seat = inner
            .seats
            .iter_mut()
            .find(|s| is_token(s, token))
            .ok_or(HttpError::Unauthorized)?;
        let player = seat.public.id;
        let (kind, plan) = match seat.accept(body, seq, inner.ledger.as_ref()) {
            Ok(accepted) => accepted,
            Err(err) => {
                // Late answers after a timeout are already counted as timeouts.
                if !matches!(err, HttpError::Conflict(_)) {
                    seat.invalid_actions += 1;
                }
                return Err(err);
            }
        };
        // Chat turns are shown through `chat_window`, not per-turn events.
        if !is_chat(kind) {
            inner.publish(SpectatorEvent::TurnEnded {
                player,
                kind,
                timed_out: false,
            });
        }
        drop(guard);
        self.answered.notify_one();
        Ok((player, plan))
    }

    /// Signs the operator's receipt for `player`'s act.
    pub(crate) fn receipt(&self, player: PlayerId, signed: &Signed) -> Result<Receipt, HttpError> {
        let stake = self.stake.as_ref().ok_or(HttpError::Internal)?;
        stake.receipt(player, signed).map_err(|err| {
            error!(player, %err, "cannot sign a receipt");
            HttpError::Internal
        })
    }

    /// Subscribes under the lock, so the snapshot and the stream after it neither gap nor overlap.
    pub fn subscribe(&self, watcher: &Watcher) -> Result<Subscription, HttpError> {
        let inner = self.inner.lock();
        let viewer = match watcher {
            Watcher::Spectator => Viewer::Spectator,
            Watcher::Audience => Viewer::Audience,
            Watcher::Player(token) => Viewer::Player {
                id: inner.seat(token).ok_or(HttpError::Unauthorized)?.public.id,
            },
        };
        let snapshot = to_json(&inner.snapshot(viewer)).ok_or(HttpError::Internal)?;
        Ok(Subscription {
            viewer,
            snapshot,
            roles: inner.roles.clone(),
            over: inner.winner.is_some(),
            events: inner.events.subscribe(),
        })
    }

    pub(crate) fn publish(&self, event: SpectatorEvent) {
        self.inner.lock().publish(event);
    }

    /// Hands `turns`, target round `round` and `step`, to their players; humans get the longer
    /// timeout.
    pub(crate) fn begin_turns(&self, game: &Game, turns: &[TurnSpec], step: u32, round: u32) {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let human_timeout = inner.settings.human_turn_timeout;
        for spec in turns {
            let turn_id = inner.next_turn_id;
            inner.next_turn_id += 1;
            let (Some(seat), Some(view)) = (
                inner.seats.get_mut(spec.player as usize),
                game.view(spec.player),
            ) else {
                error!(
                    player = spec.player,
                    "turn for a seat the server does not know; skipping"
                );
                continue;
            };
            let timeout = if seat.public.human {
                human_timeout
            } else {
                Duration::from_secs(self.config.turn_timeout_secs)
            };
            let deadline_ms = millis(timeout);
            seat.answer = None;
            seat.pending = Some(Pending {
                turn: Turn {
                    turn_id,
                    step,
                    round: Some(round),
                    kind: spec.kind,
                    candidates: spec.candidates.clone(),
                    deadline_ms,
                    view,
                },
                deadline: now + timeout,
            });
            inner.publish(SpectatorEvent::TurnStarted {
                player: spec.player,
                kind: spec.kind,
                deadline_ms,
            });
        }
        drop(inner);
        self.bump();
    }

    /// Waits until every turn is answered or has expired. False on shutdown.
    pub(crate) async fn wait_for_answers(&self, turns: &[TurnSpec]) -> bool {
        loop {
            // `notify_one` stores a permit, so an answer landing before this await isn't lost.
            let answered = self.answered.notified();
            let Some(next_deadline) = self.expire_overdue(turns) else {
                return true;
            };
            tokio::select! {
                () = answered => {}
                () = tokio::time::sleep_until(next_deadline) => {}
                () = self.shutdown.cancelled() => return false,
            }
        }
    }

    /// Times out overdue turns and returns the earliest deadline still open, if any.
    fn expire_overdue(&self, turns: &[TurnSpec]) -> Option<Instant> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        // A staked seat may still have forced its move; the ledger logs real timeouts.
        let staked = inner.ledger.is_some();
        let mut next: Option<Instant> = None;
        for spec in turns {
            let Some(seat) = inner.seats.get_mut(spec.player as usize) else {
                continue;
            };
            // No pending turn means it was answered or already expired.
            let Some(deadline) = seat.pending.as_ref().map(|p| p.deadline) else {
                continue;
            };
            if deadline > now {
                next = Some(next.map_or(deadline, |n| n.min(deadline)));
                continue;
            }
            seat.pending = None;
            seat.timeouts += 1;
            if !staked {
                warn!(
                    player = spec.player,
                    name = %seat.public.name,
                    kind = ?spec.kind,
                    "turn timed out; applying default action"
                );
            }
            inner.publish(SpectatorEvent::TurnEnded {
                player: spec.player,
                kind: spec.kind,
                timed_out: true,
            });
        }
        next
    }

    pub(crate) fn chat_rules(&self) -> ChatRules {
        let config = &self.config;
        let settings = self.inner.lock().settings;
        ChatRules {
            human: Limits {
                cooldown: config.chat_cooldown_secs,
                max_messages: config.max_chat_messages,
            },
            ai: Limits {
                cooldown: settings.ai_chat_cooldown,
                max_messages: settings.ai_max_chat_messages,
            },
            ai_jitter: config.ai_chat_jitter_secs,
            quiet: config.quiet_secs,
        }
    }

    pub(crate) fn chat_length(&self, kind: TurnKind) -> Duration {
        match kind {
            TurnKind::WolfChat => self.config.wolf_chat_secs,
            _ => self.inner.lock().settings.discussion,
        }
    }

    /// `players` paired with whether each is human.
    pub(crate) fn chat_members(&self, players: &[PlayerId]) -> Vec<(PlayerId, bool)> {
        let inner = self.inner.lock();
        players
            .iter()
            .map(|&p| {
                let human = inner.seats.get(p as usize).is_some_and(|s| s.public.human);
                (p, human)
            })
            .collect()
    }

    /// Hands chat turns for `step` to `players`, each lasting until the window `ends`.
    pub(crate) fn issue_chat_turns(
        &self,
        game: &Game,
        kind: TurnKind,
        step: u32,
        players: &[PlayerId],
        ends: Instant,
    ) {
        let mut inner = self.inner.lock();
        for &player in players {
            let turn_id = inner.next_turn_id;
            inner.next_turn_id += 1;
            let (Some(seat), Some(view)) =
                (inner.seats.get_mut(player as usize), game.view(player))
            else {
                error!(
                    player,
                    "chat turn for a seat the server does not know; skipping"
                );
                continue;
            };
            seat.answer = None;
            seat.pending = Some(Pending {
                turn: Turn {
                    turn_id,
                    step,
                    round: None,
                    kind,
                    candidates: Vec::new(),
                    deadline_ms: remaining_ms(ends),
                    view,
                },
                deadline: ends,
            });
        }
        drop(inner);
        self.bump();
    }

    /// Answers that have arrived from `players`, in arrival order.
    pub(crate) fn take_answers(&self, kind: TurnKind, players: &[PlayerId]) -> Vec<Collected> {
        self.inner.lock().take_answers(kind, players)
    }

    /// Cancels open chat turns (closing is not a timeout) and returns answers that raced the close.
    pub(crate) fn cancel_chat(&self, kind: TurnKind, players: &[PlayerId]) -> Vec<Collected> {
        let mut inner = self.inner.lock();
        for &player in players {
            if let Some(seat) = inner.seats.get_mut(player as usize) {
                seat.pending = None;
            }
        }
        let leftovers = inner.take_answers(kind, players);
        drop(inner);
        self.bump();
        leftovers
    }

    /// Waits for the next answer or `until`. False on shutdown.
    pub(crate) async fn wait_for_answer(&self, until: Instant) -> bool {
        tokio::select! {
            () = self.answered.notified() => true,
            () = tokio::time::sleep_until(until) => true,
            () = self.shutdown.cancelled() => false,
        }
    }

    /// Takes the answers; expired turns yield `None`, the default action.
    pub(crate) fn collect_answers(&self, turns: &[TurnSpec]) -> Vec<Collected> {
        let mut inner = self.inner.lock();
        turns
            .iter()
            .map(|spec| {
                let submitted = inner
                    .seats
                    .get_mut(spec.player as usize)
                    .and_then(|seat| seat.answer.take());
                match submitted {
                    Some(submitted) => submitted.collected(spec.player, spec.kind),
                    None => Collected {
                        answer: Answer {
                            player: spec.player,
                            action: None,
                        },
                        kind: spec.kind,
                        reasoning: None,
                        signed: None,
                        reply: None,
                    },
                }
            })
            .collect()
    }

    /// Numbers the next chat window: its step.
    pub(crate) fn begin_chat(&self, kind: TurnKind) -> u32 {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        match &mut inner.ledger {
            Some(ledger) => ledger.begin_chat(kind),
            None => {
                let step = inner.next_step;
                inner.next_step += 1;
                step
            }
        }
    }

    /// Numbers the next target round: its step and round.
    pub(crate) fn begin_round(&self) -> (u32, u32) {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        match &mut inner.ledger {
            Some(ledger) => ledger.begin_round(),
            None => {
                let numbers = (inner.next_step, inner.next_round);
                inner.next_step += 1;
                inner.next_round += 1;
                numbers
            }
        }
    }

    /// Feeds engine events to a staked game's public log hash chain.
    pub(crate) fn extend_log(&self, events: &[GameEvent]) -> Result<(), serde_json::Error> {
        match &mut self.inner.lock().ledger {
            Some(ledger) => ledger.extend(events),
            None => Ok(()),
        }
    }

    /// Records a posted chat message in the staked transcript.
    pub(crate) fn record_message(
        &self,
        player: PlayerId,
        text: &str,
        signed: &Signed,
    ) -> Result<(), StakeError> {
        let mut inner = self.inner.lock();
        let ledger = inner.ledger.as_mut().ok_or_else(no_ledger)?;
        Ok(ledger.record_message(player, text, signed)?)
    }

    /// Records a checkpointed round in the staked transcript; returns the actions to apply.
    pub(crate) fn record_round(
        &self,
        round: u32,
        forces: &[Force],
        answers: &[(PlayerId, Option<(Action, Signed)>)],
        enc_secret: &[u8; 32],
    ) -> Result<Vec<Option<Action>>, StakeError> {
        let mut inner = self.inner.lock();
        let ledger = inner.ledger.as_mut().ok_or_else(no_ledger)?;
        Ok(ledger.record_round(round, forces, answers, enc_secret)?)
    }

    /// Seals a staked game's transcript; `None` for free games.
    pub(crate) fn finish_transcript(&self, game: &Game) -> Option<Result<Transcript, BuildError>> {
        let ledger = self.inner.lock().ledger.take()?;
        Some(ledger.finish(game))
    }

    /// Serves `json` as the latest transcript.
    pub(crate) fn set_transcript(&self, json: Arc<[u8]>) {
        self.inner.lock().transcript = Some(json);
    }

    /// The end-of-game summary and where to write it.
    pub(crate) fn summary(&self, game: &Game, seed: u64) -> (GameSummary, Option<PathBuf>) {
        let inner = self.inner.lock();
        let players = inner
            .seats
            .iter()
            .zip(game.roles())
            .map(|(seat, &role)| {
                let died_on = game.died_on(seat.public.id);
                let shot = game
                    .shot()
                    .filter(|s| s.sheriff == seat.public.id)
                    .map(|s| ShotSummary {
                        target: s.target,
                        hit_wolf: s.hit_wolf,
                    });
                PlayerSummary {
                    id: seat.public.id,
                    name: seat.public.name.clone(),
                    model: seat.public.model.clone(),
                    human: seat.public.human,
                    role,
                    survived: died_on.is_none(),
                    died_on,
                    timeouts: seat.timeouts,
                    invalid_actions: seat.invalid_actions,
                    shot,
                    lover_of: game.lover_of(seat.public.id),
                    won: game.won(seat.public.id),
                }
            })
            .collect();
        let summary = GameSummary {
            seed,
            winner: game.winner(),
            days_played: game.days_played(),
            players,
        };
        (
            summary,
            inner.log.as_ref().map(|log| log.summary_path().to_owned()),
        )
    }

    /// Reveals every role, gives every player its final view, and closes the game.
    pub(crate) fn finish(&self, game: &Game) {
        let mut inner = self.inner.lock();
        let roles = role_entries(&inner.roles);
        inner.publish(SpectatorEvent::Roles { roles });
        for seat in &mut inner.seats {
            seat.final_view = game.view(seat.public.id);
            seat.pending = None;
        }
        inner.status = Status::Over;
        inner.log = None;
        drop(inner);
        self.bump();
    }

    /// Ends a game whose driver died so that it can be reset.
    fn abort(&self) {
        let mut inner = self.inner.lock();
        for seat in &mut inner.seats {
            seat.pending = None;
        }
        inner.status = Status::Over;
        inner.log = None;
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
}

impl Inner {
    fn seat(&self, token: &str) -> Option<&Seat> {
        self.seats.iter().find(|s| is_token(s, token))
    }

    /// Seats a player and announces it.
    fn seat_up(
        &mut self,
        id: PlayerId,
        name: String,
        model: Option<String>,
        human: bool,
        token: String,
    ) {
        let public = PublicPlayer {
            id,
            name,
            model,
            alive: true,
            revealed_role: None,
            human,
        };
        self.seats.push(Seat {
            public: public.clone(),
            token,
            pending: None,
            answer: None,
            final_view: None,
            timeouts: 0,
            invalid_actions: 0,
        });
        self.publish(SpectatorEvent::PlayerJoined { player: public });
    }

    /// Empties the lobby; tokens die with it.
    fn clear(&mut self) {
        self.status = Status::Lobby;
        self.phase = Phase::Lobby;
        self.winner = None;
        self.seats.clear();
        self.roles.clear();
        self.feed.clear();
        self.chat = None;
        self.log = None;
        self.ledger = None;
        self.publish(SpectatorEvent::Reset);
    }

    /// The lobby's escrow game, once it exists.
    fn escrow_game(&self) -> Result<Arc<OnChain>, HttpError> {
        match &self.escrow {
            Escrow::Ready(game) => Ok(Arc::clone(game)),
            Escrow::Creating => Err(HttpError::unavailable(
                "the staked game is still being created on-chain; try again shortly",
            )),
            Escrow::Failed => Err(HttpError::unavailable(
                "creating the staked game failed; the host must reset",
            )),
            Escrow::None => Err(HttpError::Internal),
        }
    }

    fn take_answers(&mut self, kind: TurnKind, players: &[PlayerId]) -> Vec<Collected> {
        let mut answers: Vec<(u64, Collected)> = players
            .iter()
            .filter_map(|&player| {
                let submitted = self.seats.get_mut(player as usize)?.answer.take()?;
                Some((submitted.seq, submitted.collected(player, kind)))
            })
            .collect();
        answers.sort_by_key(|(seq, _)| *seq);
        answers
            .into_iter()
            .map(|(_, collected)| collected)
            .collect()
    }

    /// Logs, records, and broadcasts a spectator event.
    fn publish(&mut self, event: SpectatorEvent) {
        match &event {
            SpectatorEvent::Public { event } => self.apply(event),
            SpectatorEvent::ChatWindow {
                kind,
                open,
                ends_in_ms,
            } => {
                let ends = Instant::now() + Duration::from_millis(*ends_in_ms);
                self.chat = open.then_some((*kind, ends));
            }
            _ => {}
        }
        let Some(json) = to_json(&event) else { return };
        if let Some(log) = &self.log {
            log.append(json.clone());
        }
        if event.is_feed() {
            self.feed.push(event.clone());
        }
        // Fails only when no one is watching.
        let _ = self.events.send(Arc::new(Broadcast {
            event,
            json: json.into(),
        }));
    }

    fn apply(&mut self, event: &PublicEvent) {
        match *event {
            PublicEvent::PhaseStarted { phase } => self.phase = phase,
            PublicEvent::Died { player, role, .. } => {
                if let Some(seat) = self.seats.get_mut(player as usize) {
                    seat.public.alive = false;
                    seat.public.revealed_role = Some(role);
                }
            }
            PublicEvent::GameOver { winner } => {
                self.phase = Phase::Over;
                self.winner = Some(winner);
            }
            PublicEvent::Shot { sheriff, .. } => {
                if let Some(seat) = self.seats.get_mut(sheriff as usize) {
                    seat.public.revealed_role = Some(Role::Sheriff);
                }
            }
            _ => {}
        }
    }

    /// Full state as `viewer` may see it.
    fn snapshot(&self, viewer: Viewer) -> SpectatorEvent {
        let roles = &self.roles;
        let over = self.winner.is_some();
        SpectatorEvent::Snapshot {
            phase: self.phase,
            winner: self.winner,
            settings: self.settings.body(),
            players: self.seats.iter().map(|s| s.public.clone()).collect(),
            roles: viewer.visible_roles(&role_entries(roles), roles, over),
            events: self
                .feed
                .iter()
                .filter_map(|e| e.filtered(viewer, roles, over))
                .map(Cow::into_owned)
                .collect(),
            pending: self
                .seats
                .iter()
                .filter_map(|s| {
                    let pending = s.pending.as_ref()?;
                    let kind = pending.turn.kind;
                    (!is_chat(kind) && viewer.sees_turn(s.public.id, kind, roles)).then(|| {
                        PendingTurn {
                            player: s.public.id,
                            kind,
                            deadline_ms: remaining_ms(pending.deadline),
                        }
                    })
                })
                .collect(),
            chat: self
                .chat
                .filter(|&(kind, _)| viewer.sees_chat(kind, roles))
                .map(|(kind, ends)| ChatState {
                    kind,
                    ends_in_ms: remaining_ms(ends),
                }),
        }
    }
}

impl Seat {
    /// Checks an answer against the pending turn and stores it; returns the turn kind and when
    /// the act gets its receipt. Staked games (`ledger`) also check the signature.
    fn accept(
        &mut self,
        body: Result<ActRequest, HttpError>,
        seq: u64,
        ledger: Option<&Ledger>,
    ) -> Result<(TurnKind, ReceiptPlan), HttpError> {
        let req = body?;
        let pending = self
            .pending
            .as_ref()
            .filter(|p| p.turn.turn_id == req.turn_id)
            .ok_or_else(|| HttpError::conflict("stale turn; poll again"))?;
        let kind = pending.turn.kind;
        let speaks = is_chat(kind);
        let action = match (speaks, req.action) {
            (true, Action::Speak { text }) => {
                let clean = sanitize::text(&text, MAX_TEXT_LEN);
                // Signed text is replayed verbatim, so it can't be cleaned after the fact.
                if ledger.is_some() && clean != text {
                    return Err(HttpError::invalid(
                        "staked speech must already be clean: trimmed, at most 600 characters, \
                         no control or bidi characters except \\n; send \"\" to pass",
                    ));
                }
                Action::Speak { text: clean }
            }
            (false, Action::Target { target }) => {
                if target.is_some_and(|t| !pending.turn.candidates.contains(&t)) {
                    return Err(HttpError::invalid(
                        "target is not one of the turn's candidates",
                    ));
                }
                Action::Target { target }
            }
            (true, Action::Target { .. }) => {
                return Err(HttpError::invalid("this turn needs a `speak` action"));
            }
            (false, Action::Speak { .. }) => {
                return Err(HttpError::invalid("this turn needs a `target` action"));
            }
        };
        let signed = match ledger {
            Some(ledger) => Some(ledger.check(
                &pending.turn,
                self.public.id,
                &action,
                req.log_hash.as_deref(),
                req.signature.as_deref(),
            )?),
            None => None,
        };
        let passes = matches!(&action, Action::Speak { text } if text.is_empty());
        let (plan, reply) = match signed {
            Some(signed) if !speaks => (ReceiptPlan::Sign(signed), None),
            Some(_) if !passes => {
                let (reply, receipt) = oneshot::channel();
                (ReceiptPlan::Await(receipt), Some(reply))
            }
            _ => (ReceiptPlan::None, None),
        };
        let reasoning = req
            .reasoning
            .map(|r| sanitize::text(&r, MAX_TEXT_LEN))
            .filter(|r| !r.is_empty());
        self.pending = None;
        self.answer = Some(Submitted {
            action,
            reasoning,
            seq,
            signed,
            reply,
        });
        Ok((kind, plan))
    }

    /// A seat that staked but never joined over HTTP; no token reaches it.
    fn is_placeholder(&self) -> bool {
        self.token.is_empty()
    }
}

impl Submitted {
    fn collected(self, player: PlayerId, kind: TurnKind) -> Collected {
        Collected {
            answer: Answer {
                player,
                action: Some(self.action),
            },
            kind,
            reasoning: self.reasoning,
            signed: self.signed,
            reply: self.reply,
        }
    }
}

/// Whether `token` is `seat`'s; placeholder seats have none.
fn is_token(seat: &Seat, token: &str) -> bool {
    !seat.is_placeholder() && seat.token == token
}

/// Sanitized name and model from a join.
fn player_fields(req: &JoinRequest) -> Result<(String, Option<String>), HttpError> {
    let name = sanitize::line(&req.name, MAX_NAME_LEN);
    if name.is_empty() {
        return Err(HttpError::invalid("name must not be empty"));
    }
    let model = req
        .model
        .as_deref()
        .map(|m| sanitize::line(m, MAX_MODEL_LEN))
        .filter(|m| !m.is_empty());
    Ok((name, model))
}

fn new_token() -> Result<String, HttpError> {
    random_hex(32).map_err(|err| {
        error!(%err, "OS RNG failed while issuing a player token");
        HttpError::Internal
    })
}

fn no_ledger() -> StakeError {
    StakeError::Invalid("no staked game is running".to_owned())
}

/// Refuses `start` once joins have closed or while the escrow lacks `minSeats`.
async fn check_startable(stake: &Stake, game: &OnChain) -> Result<(), HttpError> {
    let client = &stake.client;
    let read = async { tokio::try_join!(client.game_info(game.id), client.latest_timestamp()) };
    let (info, now) = tokio::time::timeout(RPC_WAIT, read)
        .await
        .map_err(|_| HttpError::unavailable("the chain RPC timed out; try again"))?
        .map_err(|err| {
            let (class, attempts, _) = StakeError::from(err).fields();
            warn!(game = %game.id, op = "gameInfo", class, ?attempts, "start could not read the escrow");
            HttpError::unavailable("could not read the escrow; try again")
        })?;
    if now >= info.joinBy {
        return Err(HttpError::conflict(
            "joins have closed; this game will be cancelled and a new one created",
        ));
    }
    if info.seatCount < u32::from(info.minSeats) {
        return Err(HttpError::conflict(format!(
            "need at least {} seats on-chain, have {}",
            info.minSeats, info.seatCount
        )));
    }
    Ok(())
}

/// The engine seed for a `--seed` game: the u64 big-endian in the first 8 bytes, zeros after.
fn deal_seed(seed: u64) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&seed.to_be_bytes());
    bytes
}

fn is_chat(kind: TurnKind) -> bool {
    matches!(kind, TurnKind::DaySpeak | TurnKind::WolfChat)
}

fn role_entries(roles: &[Role]) -> Vec<RoleEntry> {
    (0..)
        .zip(roles)
        .map(|(player, &role)| RoleEntry { player, role })
        .collect()
}

pub(crate) fn random_hex(bytes: usize) -> Result<String, SysError> {
    let mut buf = vec![0; bytes];
    SysRng.try_fill_bytes(&mut buf)?;
    Ok(hex::encode(buf))
}

fn to_json(event: &SpectatorEvent) -> Option<String> {
    serde_json::to_string(event)
        .inspect_err(|err| error!(%err, "failed to serialize spectator event"))
        .ok()
}

fn remaining_ms(deadline: Instant) -> u64 {
    millis(deadline.saturating_duration_since(Instant::now()))
}

pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stake::ledger_tests::{ledger, sign, turn};

    fn seat(turn: Turn) -> Seat {
        Seat {
            public: PublicPlayer {
                id: 4,
                name: "Seat 4".to_owned(),
                model: None,
                alive: true,
                revealed_role: None,
                human: false,
            },
            token: "token".to_owned(),
            pending: Some(Pending {
                turn,
                deadline: Instant::now() + Duration::from_secs(60),
            }),
            answer: None,
            final_view: None,
            timeouts: 0,
            invalid_actions: 0,
        }
    }

    /// Accepts `action` on `turn` for seat 4, signed with `step`.
    fn accept(
        ledger: &Ledger,
        turn: &Turn,
        step: u32,
        action: Action,
    ) -> Result<ReceiptPlan, HttpError> {
        let (log_hash, signature) = sign(ledger, turn, step, 4, &action);
        let req = ActRequest {
            turn_id: turn.turn_id,
            action,
            reasoning: None,
            log_hash: Some(log_hash),
            signature: Some(signature),
        };
        seat(turn.clone())
            .accept(Ok(req), 0, Some(ledger))
            .map(|(_, plan)| plan)
    }

    fn speak(text: &str) -> Action {
        Action::Speak {
            text: text.to_owned(),
        }
    }

    #[test]
    fn staked_acts_are_receipted_only_once_they_enter_the_transcript() {
        let ledger = ledger();
        let vote = turn(3, 1, Some(0), TurnKind::DayVote);
        let chat = turn(4, 0, None, TurnKind::DaySpeak);
        let abstain = Action::Target { target: None };

        assert!(matches!(
            accept(&ledger, &vote, 1, abstain.clone()),
            Ok(ReceiptPlan::Sign(signed)) if signed.turn_id == 3
        ));
        assert!(matches!(
            accept(&ledger, &chat, 0, speak("I saw nothing.")),
            Ok(ReceiptPlan::Await(_))
        ));
        assert!(
            matches!(accept(&ledger, &chat, 0, speak("")), Ok(ReceiptPlan::None)),
            "passes never enter the transcript"
        );

        let mut answered = seat(vote.clone());
        let (log_hash, signature) = sign(&ledger, &vote, 1, 4, &abstain);
        let req = || ActRequest {
            turn_id: 3,
            action: abstain.clone(),
            reasoning: None,
            log_hash: Some(log_hash.clone()),
            signature: Some(signature.clone()),
        };
        assert!(answered.accept(Ok(req()), 0, Some(&ledger)).is_ok());
        assert!(
            matches!(
                answered.accept(Ok(req()), 1, Some(&ledger)),
                Err(HttpError::Conflict(_))
            ),
            "a stale turn gets no receipt"
        );

        let free = ActRequest {
            log_hash: None,
            signature: None,
            ..req()
        };
        assert!(matches!(
            seat(vote.clone()).accept(Ok(free), 0, None),
            Ok((_, ReceiptPlan::None))
        ));
    }

    #[test]
    fn staked_acts_reject_bad_signatures_and_unclean_speech() {
        let ledger = ledger();
        let vote = turn(3, 1, Some(0), TurnKind::DayVote);
        let chat = turn(4, 0, None, TurnKind::DaySpeak);
        let invalid = |result: Result<ReceiptPlan, HttpError>| match result {
            Err(HttpError::Invalid(message)) => message,
            other => panic!("expected a 422, got {other:?}"),
        };
        let wrong_step = invalid(accept(&ledger, &vote, 0, Action::Target { target: None }));
        assert!(wrong_step.contains("does not verify"), "{wrong_step}");
        for unclean in [" padded", "bell\u{7}", "\u{202E}flip", "line\r\nbreak"] {
            let message = invalid(accept(&ledger, &chat, 0, speak(unclean)));
            assert!(
                message.contains("must already be clean"),
                "{unclean:?}: {message}"
            );
        }
        assert!(accept(&ledger, &chat, 0, speak("two\nlines")).is_ok());
        let unsigned = ActRequest {
            turn_id: 3,
            action: Action::Target { target: None },
            reasoning: None,
            log_hash: None,
            signature: None,
        };
        let message = invalid(
            seat(vote)
                .accept(Ok(unsigned), 0, Some(&ledger))
                .map(|(_, plan)| plan),
        );
        assert!(message.contains("signature"), "{message}");
    }
}
