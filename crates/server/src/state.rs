//! Shared server state, where HTTP handlers and the game driver meet.

use std::{borrow::Cow, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use rand::{
    TryRng,
    rngs::{SysError, SysRng},
};
use tokio::{
    sync::{Notify, broadcast, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use werewolf_protocol::{
    ActRequest, Action, JoinRequest, JoinResponse, MAX_MODEL_LEN, MAX_NAME_LEN, MAX_TEXT_LEN,
    Phase, PlayerId, PlayerView, Poll, PublicEvent, PublicPlayer, Role, Team, Turn, TurnKind,
};

use crate::{
    chat::{ChatRules, Limits},
    config::Config,
    driver,
    engine::{Answer, Game, TurnSpec},
    error::HttpError,
    game_log::{GameLog, GameSummary, PlayerSummary, ShotSummary},
    sanitize,
    settings::{Settings, SettingsBody},
    site::Site,
    spectator::{Broadcast, ChatState, PendingTurn, RoleEntry, SpectatorEvent, Viewer},
};

/// Events a slow spectator may fall behind before it gets a fresh snapshot.
const EVENT_BUFFER: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Lobby,
    Running,
    Over,
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
    games_started: u64,
    log: Option<GameLog>,
    events: broadcast::Sender<Arc<Broadcast>>,
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
}

/// One turn's outcome as handed to the driver.
pub struct Collected {
    pub answer: Answer,
    pub kind: TurnKind,
    pub reasoning: Option<String>,
}

impl AppState {
    pub fn new(
        config: Config,
        spectator_key: String,
        seed: u64,
        site: Site,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let settings = Settings::from_config(&config);
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
                games_started: 0,
                log: None,
                events,
            }),
            changed: watch::Sender::new(0),
            answered: Notify::new(),
            shutdown,
        })
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
        let name = sanitize::line(&req.name, MAX_NAME_LEN);
        if name.is_empty() {
            return Err(HttpError::invalid("name must not be empty"));
        }
        let model = req
            .model
            .map(|m| sanitize::line(&m, MAX_MODEL_LEN))
            .filter(|m| !m.is_empty());
        let token = random_hex(32).map_err(|err| {
            error!(%err, "OS RNG failed while issuing a player token");
            HttpError::Internal
        })?;

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
        let public = PublicPlayer {
            id: player_id,
            name,
            model,
            alive: true,
            revealed_role: None,
            human: req.human,
        };
        inner.seats.push(Seat {
            public: public.clone(),
            token: token.clone(),
            pending: None,
            answer: None,
            final_view: None,
            timeouts: 0,
            invalid_actions: 0,
        });
        inner.publish(SpectatorEvent::PlayerJoined { player: public });

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

    pub fn start(self: &Arc<Self>) -> Result<(), HttpError> {
        self.start_locked(&mut self.inner.lock())
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
        let (game, events) = Game::new(roster, inner.settings.setup, seed)
            .map_err(|err| HttpError::conflict(err.to_string()))?;

        inner.games_started += 1;
        inner.status = Status::Running;
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

        let game = tokio::spawn(driver::run(Arc::clone(self), game, seed));
        let app = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = game.await
                && err.is_panic()
            {
                error!(%err, "game driver panicked; game aborted, reset to continue");
                app.abort();
            }
        });
        Ok(())
    }

    pub fn reset(&self) -> Result<(), HttpError> {
        let mut inner = self.inner.lock();
        if inner.status == Status::Running {
            return Err(HttpError::conflict(
                "game in progress; reset once it is over",
            ));
        }
        inner.status = Status::Lobby;
        inner.phase = Phase::Lobby;
        inner.winner = None;
        inner.seats.clear();
        inner.roles.clear();
        inner.feed.clear();
        inner.chat = None;
        inner.log = None;
        inner.publish(SpectatorEvent::Reset);
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
        inner.settings = body
            .validate(self.config.max_players)
            .map_err(HttpError::Invalid)?;
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

    /// Validates and records an answer to the player's pending turn.
    pub fn act(&self, token: &str, body: Result<ActRequest, HttpError>) -> Result<(), HttpError> {
        let mut inner = self.inner.lock();
        let seq = inner.next_answer_seq;
        inner.next_answer_seq += 1;
        let seat = inner.seat_mut(token).ok_or(HttpError::Unauthorized)?;
        let player = seat.public.id;
        let kind = match seat.accept(body, seq) {
            Ok(kind) => kind,
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
        drop(inner);
        self.answered.notify_one();
        Ok(())
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

    /// Hands `turns` to their players; humans get the longer timeout.
    pub(crate) fn begin_turns(&self, game: &Game, turns: &[TurnSpec]) {
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
            warn!(
                player = spec.player,
                name = %seat.public.name,
                kind = ?spec.kind,
                "turn timed out; applying default action"
            );
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

    /// Hands chat turns to `players`, each lasting until the window `ends`.
    pub(crate) fn issue_chat_turns(
        &self,
        game: &Game,
        kind: TurnKind,
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
                let (action, reasoning) =
                    submitted.map_or((None, None), |s| (Some(s.action), s.reasoning));
                Collected {
                    answer: Answer {
                        player: spec.player,
                        action,
                    },
                    kind: spec.kind,
                    reasoning,
                }
            })
            .collect()
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
        self.seats.iter().find(|s| s.token == token)
    }

    fn seat_mut(&mut self, token: &str) -> Option<&mut Seat> {
        self.seats.iter_mut().find(|s| s.token == token)
    }

    fn take_answers(&mut self, kind: TurnKind, players: &[PlayerId]) -> Vec<Collected> {
        let mut answers: Vec<(u64, Collected)> = players
            .iter()
            .filter_map(|&player| {
                let submitted = self.seats.get_mut(player as usize)?.answer.take()?;
                Some((
                    submitted.seq,
                    Collected {
                        answer: Answer {
                            player,
                            action: Some(submitted.action),
                        },
                        kind,
                        reasoning: submitted.reasoning,
                    },
                ))
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
    /// Checks an answer against the pending turn and stores it; returns the turn kind.
    fn accept(
        &mut self,
        body: Result<ActRequest, HttpError>,
        seq: u64,
    ) -> Result<TurnKind, HttpError> {
        let req = body?;
        let pending = self
            .pending
            .as_ref()
            .filter(|p| p.turn.turn_id == req.turn_id)
            .ok_or_else(|| HttpError::conflict("stale turn; poll again"))?;
        let kind = pending.turn.kind;
        let speaks = is_chat(kind);
        let action = match (speaks, req.action) {
            (true, Action::Speak { text }) => Action::Speak {
                text: sanitize::text(&text, MAX_TEXT_LEN),
            },
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
        let reasoning = req
            .reasoning
            .map(|r| sanitize::text(&r, MAX_TEXT_LEN))
            .filter(|r| !r.is_empty());
        self.pending = None;
        self.answer = Some(Submitted {
            action,
            reasoning,
            seq,
        });
        Ok(kind)
    }
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
