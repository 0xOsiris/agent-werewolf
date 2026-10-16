//! Pure, deterministic game rules. No I/O: the driver hands out [`Game::turns`] and feeds the
//! answers back through [`Game::submit`].

use std::{collections::BTreeMap, num::NonZeroUsize};

use rand::{
    SeedableRng,
    rngs::StdRng,
    seq::{IndexedRandom, SliceRandom},
};
use serde::Serialize;
use werewolf_protocol::{
    Action, DeathCause, Phase, PlayerId, PlayerView, PrivateEvent, PublicEvent, PublicPlayer, Role,
    Team, TurnKind,
};

/// Fewest seats a game can start with, per `PROTOCOL.md`.
pub const MIN_PLAYERS: usize = 5;
/// Wolves win if this day ends without a winner.
pub const LAST_DAY: u32 = 10;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EngineError {
    #[error("need at least {MIN_PLAYERS} players, got {0}")]
    TooFewPlayers(usize),
    /// `setup` reads like "2 wolves + sheriff + cupid".
    #[error("need at least {need} players for {setup}")]
    TooFewForSetup { setup: String, need: usize },
}

/// Which special roles a game deals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Setup {
    /// Fixed wolf count; by default max(1, n / 4).
    pub wolves: Option<NonZeroUsize>,
    /// Deal one villager the sheriff role.
    pub sheriff: bool,
    /// Deal one villager the cupid role.
    pub cupid: bool,
}

/// Seats needed: fewer wolves than others, plus a seer, doctor, sheriff and cupid if on, and a
/// villager.
pub fn players_needed(wolves: usize, sheriff: bool, cupid: bool) -> usize {
    (2 * wolves + 1)
        .max(wolves + 2 + usize::from(sheriff) + usize::from(cupid) + 1)
        .max(MIN_PLAYERS)
}

/// Names a setup in errors, like "2 wolves + sheriff + cupid".
pub fn describe_setup(wolves: usize, sheriff: bool, cupid: bool) -> String {
    let sheriff = if sheriff { " + sheriff" } else { "" };
    let cupid = if cupid { " + cupid" } else { "" };
    format!("{wolves} wolves{sheriff}{cupid}")
}

/// The sheriff's one shot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShotRecord {
    pub sheriff: PlayerId,
    pub target: PlayerId,
    pub hit_wolf: bool,
}

/// Who may see a private event: a role, one lover, or the dead.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Wolves,
    Seer,
    Doctor,
    Cupid,
    /// Serializes as `{"lover": <seat>}`.
    Lover(PlayerId),
    /// Players who were already dead when the event happened.
    Dead,
}

impl Audience {
    /// Whether `player`, dealt `role`, may see the event; `dead` says they had died by then.
    pub(crate) fn admits(self, player: PlayerId, role: Role, dead: bool) -> bool {
        match self {
            Self::Wolves => role == Role::Werewolf,
            Self::Seer => role == Role::Seer,
            Self::Doctor => role == Role::Doctor,
            Self::Cupid => role == Role::Cupid,
            Self::Lover(lover) => lover == player,
            Self::Dead => dead,
        }
    }
}

#[derive(Clone, Debug)]
pub enum GameEvent {
    Public(PublicEvent),
    Private(Audience, PrivateEvent),
}

/// A turn the engine is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnSpec {
    pub player: PlayerId,
    pub kind: TurnKind,
    pub candidates: Vec<PlayerId>,
}

/// A player's answer; `None` (timeout) applies the default action.
#[derive(Clone, Debug)]
pub struct Answer {
    pub player: PlayerId,
    pub action: Option<Action>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Cupid picks a Lover on night 1; `first` holds the first pick while the second is due.
    CupidLink {
        first: Option<PlayerId>,
    },
    /// Open chat among living wolves.
    WolfChat,
    NightActions,
    /// Open chat among all living players; the armed sheriff may fire at any point.
    Discussion,
    DayVote,
    Over,
}

pub struct Game {
    rng: StdRng,
    players: Vec<PublicPlayer>,
    roles: Vec<Role>,
    died_on: Vec<Option<Phase>>,
    /// Length of `private_log` at each player's death; they see dead chat from there on.
    ghost_since: Vec<Option<usize>>,
    phase: Phase,
    step: Step,
    /// Number of the current night/day; night n precedes day n.
    day: u32,
    last_protected: Option<PlayerId>,
    /// The sheriff still holds their bullet.
    bullet: bool,
    shot: Option<ShotRecord>,
    lovers: Option<(PlayerId, PlayerId)>,
    winner: Option<Team>,
    public_log: Vec<PublicEvent>,
    private_log: Vec<(Audience, PrivateEvent)>,
    outbox: Vec<GameEvent>,
}

impl Game {
    /// Seats `roster` in order (ids become seat numbers), deals roles, and starts night 1.
    pub fn new(
        roster: Vec<PublicPlayer>,
        setup: Setup,
        seed: u64,
    ) -> Result<(Self, Vec<GameEvent>), EngineError> {
        let n = roster.len();
        let wolves = match setup.wolves.map(NonZeroUsize::get) {
            Some(wolves) => wolves,
            None if n < MIN_PLAYERS => return Err(EngineError::TooFewPlayers(n)),
            None => (n / 4).max(1),
        };
        let need = players_needed(wolves, setup.sheriff, setup.cupid);
        if n < need {
            return Err(EngineError::TooFewForSetup {
                setup: describe_setup(wolves, setup.sheriff, setup.cupid),
                need,
            });
        }
        let mut rng = StdRng::seed_from_u64(seed);
        let mut roles = vec![Role::Werewolf; wolves];
        roles.extend([Role::Seer, Role::Doctor]);
        if setup.sheriff {
            roles.push(Role::Sheriff);
        }
        if setup.cupid {
            roles.push(Role::Cupid);
        }
        roles.resize(n, Role::Villager);
        roles.shuffle(&mut rng);

        let players = roster
            .into_iter()
            .zip(0..)
            .map(|(player, id)| PublicPlayer {
                id,
                alive: true,
                revealed_role: None,
                ..player
            })
            .collect();
        let mut game = Self {
            rng,
            players,
            roles,
            died_on: vec![None; n],
            ghost_since: vec![None; n],
            phase: Phase::Lobby,
            step: Step::Over,
            day: 0,
            last_protected: None,
            bullet: setup.sheriff,
            shot: None,
            lovers: None,
            winner: None,
            public_log: Vec::new(),
            private_log: Vec::new(),
            outbox: Vec::new(),
        };
        game.start_night(1);
        let events = game.take_events();
        Ok((game, events))
    }

    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    pub fn winner(&self) -> Option<Team> {
        self.winner
    }

    pub fn is_over(&self) -> bool {
        self.step == Step::Over
    }

    pub fn days_played(&self) -> u32 {
        self.day
    }

    pub fn died_on(&self, player: PlayerId) -> Option<Phase> {
        self.died_on.get(player as usize).copied().flatten()
    }

    pub fn shot(&self) -> Option<ShotRecord> {
        self.shot
    }

    /// `player`'s partner, if Cupid linked them.
    pub fn lover_of(&self, player: PlayerId) -> Option<PlayerId> {
        match self.lovers? {
            (a, b) if a == player => Some(b),
            (a, b) if b == player => Some(a),
            _ => None,
        }
    }

    /// Whether `player` shares the win; cross-team lovers win only as [`Team::Lovers`].
    pub fn won(&self, player: PlayerId) -> bool {
        match self.winner {
            Some(Team::Lovers) => self.lover_of(player).is_some(),
            Some(team) => {
                let rogue = self
                    .rogue_lovers()
                    .is_some_and(|(a, b)| player == a || player == b);
                self.role(player).team() == team && !rogue
            }
            None => false,
        }
    }

    /// The open chat window's turn kind, if the game is waiting on chat.
    pub fn chat(&self) -> Option<TurnKind> {
        match self.step {
            Step::WolfChat => Some(TurnKind::WolfChat),
            Step::Discussion => Some(TurnKind::DaySpeak),
            Step::CupidLink { .. } | Step::NightActions | Step::DayVote | Step::Over => None,
        }
    }

    /// Dead players who may use the dead chat: during the day discussion, once two or more are dead.
    pub fn ghosts(&self) -> Vec<PlayerId> {
        let dead: Vec<PlayerId> = self.seats().filter(|&p| !self.is_alive(p)).collect();
        if self.step == Step::Discussion && dead.len() >= 2 {
            dead
        } else {
            Vec::new()
        }
    }

    /// Posts `text` to the open chat window, or a ghost's to the dead chat, unless blank or barred.
    pub fn speak(&mut self, player: PlayerId, text: &str) -> Vec<GameEvent> {
        if !text.trim().is_empty() {
            let alive = self.is_alive(player);
            let text = text.to_owned();
            match self.step {
                Step::Discussion if alive => self.emit_public(PublicEvent::Spoke { player, text }),
                Step::Discussion if self.ghosts().contains(&player) => {
                    self.emit_private(Audience::Dead, PrivateEvent::DeadSpoke { player, text })
                }
                Step::WolfChat if alive && self.is_wolf(player) => {
                    self.emit_private(Audience::Wolves, PrivateEvent::WolfSpoke { player, text })
                }
                _ => {}
            }
        }
        self.take_events()
    }

    /// Fires the armed sheriff's bullet during the discussion; ignored if `target` isn't theirs to shoot.
    pub fn shoot(&mut self, sheriff: PlayerId, target: PlayerId) -> Vec<GameEvent> {
        if self.shoot_candidates(sheriff).contains(&target) {
            self.fire(sheriff, target);
        }
        self.take_events()
    }

    /// Who `player` may shoot now: every other living player if they are the armed sheriff in the
    /// discussion, else nobody.
    pub fn shoot_candidates(&self, player: PlayerId) -> Vec<PlayerId> {
        if self.step == Step::Discussion && self.armed_sheriff() == Some(player) {
            self.living().filter(|&p| p != player).collect()
        } else {
            Vec::new()
        }
    }

    /// Ends the open chat window and moves on to the vote or the night actions.
    pub fn close_chat(&mut self) -> Vec<GameEvent> {
        match self.step {
            Step::WolfChat => self.step = Step::NightActions,
            Step::Discussion => self.step = Step::DayVote,
            Step::CupidLink { .. } | Step::NightActions | Step::DayVote | Step::Over => {}
        }
        self.take_events()
    }

    /// Turns to hand out now, in seat order; for chat, everyone who may speak. Empty only once
    /// the game is over.
    pub fn turns(&self) -> Vec<TurnSpec> {
        let living: Vec<PlayerId> = self.living().collect();
        let except = |skip: Option<PlayerId>| -> Vec<PlayerId> {
            living
                .iter()
                .copied()
                .filter(|&p| Some(p) != skip)
                .collect()
        };
        let spec = |player, kind, candidates| TurnSpec {
            player,
            kind,
            candidates,
        };
        match self.step {
            Step::CupidLink { first } => living
                .iter()
                .filter(|&&p| self.role(p) == Role::Cupid)
                .map(|&p| spec(p, TurnKind::CupidLink, except(first)))
                .collect(),
            Step::WolfChat => living
                .iter()
                .filter(|&&p| self.is_wolf(p))
                .map(|&p| spec(p, TurnKind::WolfChat, Vec::new()))
                .collect(),
            Step::Discussion => living
                .iter()
                .map(|&p| spec(p, TurnKind::DaySpeak, self.shoot_candidates(p)))
                .collect(),
            Step::NightActions => living
                .iter()
                .filter_map(|&p| match self.role(p) {
                    Role::Werewolf => {
                        let prey = living
                            .iter()
                            .copied()
                            .filter(|&t| !self.is_wolf(t))
                            .collect();
                        Some(spec(p, TurnKind::WolfKill, prey))
                    }
                    Role::Seer => Some(spec(p, TurnKind::SeerInspect, except(Some(p)))),
                    Role::Doctor => Some(spec(
                        p,
                        TurnKind::DoctorProtect,
                        except(self.last_protected),
                    )),
                    Role::Villager | Role::Sheriff | Role::Cupid => None,
                })
                .collect(),
            Step::DayVote => living
                .iter()
                .map(|&p| spec(p, TurnKind::DayVote, except(Some(p))))
                .collect(),
            Step::Over => Vec::new(),
        }
    }

    /// Applies answers to the current [`Game::turns`] and advances. Missing or invalid answers
    /// get the default action (silence / abstain). For chat, this applies one answer per player in
    /// seat order and closes the window; the driver uses [`Game::speak`] and [`Game::shoot`] instead.
    pub fn submit(&mut self, answers: &[Answer]) -> Vec<GameEvent> {
        let turns = self.turns();
        match self.step {
            Step::WolfChat | Step::Discussion => {
                let mut events = Vec::new();
                for turn in &turns {
                    // The sheriff's shot can kill a player whose answer comes later.
                    if !self.is_alive(turn.player) {
                        continue;
                    }
                    let action = answers.iter().find(|a| a.player == turn.player);
                    match action.and_then(|a| a.action.as_ref()) {
                        Some(Action::Speak { text }) => {
                            events.extend(self.speak(turn.player, text));
                        }
                        Some(Action::Target {
                            target: Some(target),
                        }) => events.extend(self.shoot(turn.player, *target)),
                        _ => {}
                    }
                }
                events.extend(self.close_chat());
                return events;
            }
            Step::CupidLink { first } => self.resolve_link(&turns, answers, first),
            Step::NightActions => self.resolve_night(&turns, answers),
            Step::DayVote => self.resolve_vote(&turns, answers),
            Step::Over => {}
        }
        self.take_events()
    }

    /// Everything `you` may know. `None` for an unknown seat.
    pub fn view(&self, you: PlayerId) -> Option<PlayerView> {
        let role = *self.roles.get(you as usize)?;
        let ghost_since = self.ghost_since.get(you as usize).copied().flatten();
        let wolf_team = match role {
            Role::Werewolf => self
                .seats()
                .filter(|&p| p != you && self.is_wolf(p))
                .collect(),
            _ => Vec::new(),
        };
        Some(PlayerView {
            you,
            role,
            phase: self.phase,
            players: self.players.clone(),
            wolf_team,
            public_log: self.public_log.clone(),
            private_log: self
                .private_log
                .iter()
                .enumerate()
                .filter(|(i, (seen_by, _))| {
                    seen_by.admits(you, role, ghost_since.is_some_and(|since| *i >= since))
                })
                .map(|(_, (_, event))| event.clone())
                .collect(),
        })
    }

    /// Records Cupid's pick; the second links the Lovers. Null or a timeout on either means none.
    fn resolve_link(&mut self, turns: &[TurnSpec], answers: &[Answer], first: Option<PlayerId>) {
        let pick = turns.iter().find_map(|turn| chosen(turn, answers));
        match (first, pick) {
            (None, Some(a)) => self.step = Step::CupidLink { first: Some(a) },
            (Some(a), Some(b)) => {
                self.link(a, b);
                self.step = self.night_step();
            }
            (_, None) => self.step = self.night_step(),
        }
    }

    fn link(&mut self, a: PlayerId, b: PlayerId) {
        self.lovers = Some((a, b));
        self.emit_private(Audience::Cupid, PrivateEvent::Linked { a, b });
        for (lover, partner) in [(a, b), (b, a)] {
            let partner_role = self.role(partner);
            self.emit_private(
                Audience::Lover(lover),
                PrivateEvent::InLove {
                    partner,
                    partner_role,
                },
            );
        }
    }

    fn resolve_night(&mut self, turns: &[TurnSpec], answers: &[Answer]) {
        let night = self.phase;
        let mut wolf_votes = Vec::new();
        let mut protected = None;
        for turn in turns {
            let target = chosen(turn, answers);
            match turn.kind {
                TurnKind::WolfKill => {
                    wolf_votes.push(target);
                    self.emit_private(
                        Audience::Wolves,
                        PrivateEvent::WolfVoted {
                            voter: turn.player,
                            target,
                        },
                    );
                }
                TurnKind::SeerInspect => {
                    if let Some(target) = target {
                        let is_werewolf = self.is_wolf(target);
                        self.emit_private(
                            Audience::Seer,
                            PrivateEvent::Inspected {
                                target,
                                is_werewolf,
                            },
                        );
                    }
                }
                TurnKind::DoctorProtect => {
                    protected = target;
                    self.last_protected = target;
                    if let Some(target) = target {
                        self.emit_private(Audience::Doctor, PrivateEvent::Protected { target });
                    }
                }
                TurnKind::DaySpeak
                | TurnKind::DayVote
                | TurnKind::WolfChat
                | TurnKind::SheriffShoot
                | TurnKind::CupidLink
                | TurnKind::DeadChat => {}
            }
        }
        let victim = leaders(&wolf_votes).choose(&mut self.rng).copied();

        self.set_phase(Phase::Day { n: self.day });
        match victim {
            Some(victim) if protected != Some(victim) => {
                self.kill(victim, DeathCause::Night, night);
            }
            _ => self.emit_public(PublicEvent::PeacefulNight),
        }
        if !self.is_over() {
            self.step = Step::Discussion;
        }
    }

    /// Spends the bullet: reveals the sheriff, kills the target, and the sheriff too on a miss.
    fn fire(&mut self, sheriff: PlayerId, target: PlayerId) {
        self.bullet = false;
        if let Some(seat) = self.players.get_mut(sheriff as usize) {
            seat.revealed_role = Some(Role::Sheriff);
        }
        self.emit_public(PublicEvent::Shot { sheriff, target });
        let hit_wolf = self.is_wolf(target);
        self.shot = Some(ShotRecord {
            sheriff,
            target,
            hit_wolf,
        });
        let when = self.phase;
        self.bury(target, DeathCause::Shot, when);
        // A sheriff who shot their own lover has already died of heartbreak.
        if !hit_wolf && self.is_alive(sheriff) {
            self.bury(sheriff, DeathCause::Backfire, when);
        }
        self.settle();
    }

    /// The living sheriff, if they still hold their bullet.
    fn armed_sheriff(&self) -> Option<PlayerId> {
        self.living()
            .find(|&p| self.bullet && self.role(p) == Role::Sheriff)
    }

    fn resolve_vote(&mut self, turns: &[TurnSpec], answers: &[Answer]) {
        let mut votes = Vec::new();
        for turn in turns {
            let target = chosen(turn, answers);
            votes.push(target);
            self.emit_public(PublicEvent::Voted {
                voter: turn.player,
                target,
            });
        }
        match leaders(&votes).as_slice() {
            [condemned] => self.kill(*condemned, DeathCause::Vote, self.phase),
            _ => self.emit_public(PublicEvent::NoElimination),
        }
        if self.is_over() {
            return;
        }
        if self.day >= LAST_DAY {
            self.finish(Team::Wolves);
        } else {
            self.start_night(self.day + 1);
        }
    }

    fn start_night(&mut self, n: u32) {
        self.day = n;
        self.set_phase(Phase::Night { n });
        self.step = if n == 1 && self.roles.contains(&Role::Cupid) {
            Step::CupidLink { first: None }
        } else {
            self.night_step()
        };
    }

    /// Wolf chat if two or more wolves live, else straight to the night actions.
    fn night_step(&self) -> Step {
        let wolves = self.living().filter(|&p| self.is_wolf(p)).count();
        if wolves >= 2 {
            Step::WolfChat
        } else {
            Step::NightActions
        }
    }

    fn kill(&mut self, player: PlayerId, cause: DeathCause, when: Phase) {
        self.bury(player, cause, when);
        self.settle();
    }

    /// Records a death, and the partner's heartbreak, without checking for a win.
    fn bury(&mut self, player: PlayerId, cause: DeathCause, when: Phase) {
        let role = self.role(player);
        if let Some(seat) = self.players.get_mut(player as usize) {
            seat.alive = false;
            seat.revealed_role = Some(role);
        }
        if let Some(slot) = self.died_on.get_mut(player as usize) {
            *slot = Some(when);
        }
        if let Some(slot) = self.ghost_since.get_mut(player as usize) {
            *slot = Some(self.private_log.len());
        }
        self.emit_public(PublicEvent::Died {
            player,
            role,
            cause,
        });
        // The partner is already dead when this runs for the heartbreak itself.
        if let Some(partner) = self.lover_of(player).filter(|&p| self.is_alive(p)) {
            self.bury(partner, DeathCause::Heartbreak, when);
        }
    }

    fn settle(&mut self) {
        if let Some(winner) = self.check_win() {
            self.finish(winner);
        }
    }

    fn check_win(&self) -> Option<Team> {
        if let Some((a, b)) = self.rogue_lovers()
            && self.is_alive(a)
            && self.is_alive(b)
            && self.living().all(|p| p == a || p == b)
        {
            return Some(Team::Lovers);
        }
        let (wolves, village) = self.living().fold((0, 0), |(w, v), p| {
            if self.is_wolf(p) {
                (w + 1, v)
            } else {
                (w, v + 1)
            }
        });
        if wolves == 0 {
            Some(Team::Village)
        } else if wolves >= village {
            Some(Team::Wolves)
        } else {
            None
        }
    }

    fn finish(&mut self, winner: Team) {
        self.winner = Some(winner);
        self.phase = Phase::Over;
        self.step = Step::Over;
        self.emit_public(PublicEvent::GameOver { winner });
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.emit_public(PublicEvent::PhaseStarted { phase });
    }

    fn seats(&self) -> impl Iterator<Item = PlayerId> + '_ {
        self.players.iter().map(|p| p.id)
    }

    fn living(&self) -> impl Iterator<Item = PlayerId> + '_ {
        self.players.iter().filter(|p| p.alive).map(|p| p.id)
    }

    /// Lovers from opposite teams, who can only win together.
    fn rogue_lovers(&self) -> Option<(PlayerId, PlayerId)> {
        self.lovers
            .filter(|&(a, b)| self.role(a).team() != self.role(b).team())
    }

    pub fn is_alive(&self, player: PlayerId) -> bool {
        self.players.get(player as usize).is_some_and(|p| p.alive)
    }

    fn role(&self, player: PlayerId) -> Role {
        // Seats are dense `0..n`; ids only ever come from `self.players`.
        self.roles
            .get(player as usize)
            .copied()
            .unwrap_or(Role::Villager)
    }

    fn is_wolf(&self, player: PlayerId) -> bool {
        self.role(player) == Role::Werewolf
    }

    fn emit_public(&mut self, event: PublicEvent) {
        self.public_log.push(event.clone());
        self.outbox.push(GameEvent::Public(event));
    }

    fn emit_private(&mut self, audience: Audience, event: PrivateEvent) {
        self.private_log.push((audience, event.clone()));
        self.outbox.push(GameEvent::Private(audience, event));
    }

    fn take_events(&mut self) -> Vec<GameEvent> {
        std::mem::take(&mut self.outbox)
    }
}

/// The player's valid target, or `None` for abstain, timeout, or an invalid answer.
fn chosen(turn: &TurnSpec, answers: &[Answer]) -> Option<PlayerId> {
    match answers.iter().find(|a| a.player == turn.player)?.action {
        Some(Action::Target {
            target: Some(target),
        }) if turn.candidates.contains(&target) => Some(target),
        _ => None,
    }
}

/// Targets tied for the most votes, in seat order; empty if everyone abstained.
fn leaders(votes: &[Option<PlayerId>]) -> Vec<PlayerId> {
    let mut counts = BTreeMap::new();
    for &target in votes.iter().flatten() {
        *counts.entry(target).or_insert(0u32) += 1;
    }
    let Some(&max) = counts.values().max() else {
        return Vec::new();
    };
    counts
        .into_iter()
        .filter(|&(_, n)| n == max)
        .map(|(target, _)| target)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(n: usize) -> Vec<PublicPlayer> {
        (0..n as PlayerId)
            .map(|id| PublicPlayer {
                id,
                name: format!("p{id}"),
                model: None,
                alive: true,
                revealed_role: None,
                human: id == 0,
                look: 0,
            })
            .collect()
    }

    fn game(n: usize, seed: u64) -> Game {
        Game::new(roster(n), Setup::default(), seed).unwrap().0
    }

    fn speak(text: &str) -> Option<Action> {
        Some(Action::Speak {
            text: text.to_owned(),
        })
    }

    fn target(t: PlayerId) -> Option<Action> {
        Some(Action::Target { target: Some(t) })
    }

    /// Submits one batch, answering each turn with `policy`.
    fn step(
        game: &mut Game,
        mut policy: impl FnMut(&Game, &TurnSpec) -> Option<Action>,
    ) -> Vec<GameEvent> {
        let answers: Vec<Answer> = game
            .turns()
            .iter()
            .map(|t| Answer {
                player: t.player,
                action: policy(game, t),
            })
            .collect();
        game.submit(&answers)
    }

    /// Plays batches until `stop` holds or the game ends.
    fn play_until(
        game: &mut Game,
        mut policy: impl FnMut(&Game, &TurnSpec) -> Option<Action>,
        stop: impl Fn(&Game) -> bool,
    ) -> Vec<GameEvent> {
        let mut events = Vec::new();
        while !game.is_over() && !stop(game) {
            events.extend(step(game, &mut policy));
        }
        events
    }

    fn play(
        game: &mut Game,
        policy: impl FnMut(&Game, &TurnSpec) -> Option<Action>,
    ) -> Vec<GameEvent> {
        play_until(game, policy, |_| false)
    }

    fn seat_with(game: &Game, role: Role) -> PlayerId {
        game.roles().iter().position(|&r| r == role).unwrap() as PlayerId
    }

    fn public(events: &[GameEvent]) -> Vec<&PublicEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                GameEvent::Public(p) => Some(p),
                GameEvent::Private(..) => None,
            })
            .collect()
    }

    #[test]
    fn role_counts() {
        for (n, wolves) in [(5, 1), (8, 2), (12, 3)] {
            let game = game(n, 7);
            let count = |role| game.roles().iter().filter(|&&r| r == role).count();
            assert_eq!(count(Role::Werewolf), wolves, "n={n}");
            assert_eq!(count(Role::Seer), 1, "n={n}");
            assert_eq!(count(Role::Doctor), 1, "n={n}");
            assert_eq!(count(Role::Villager), n - wolves - 2, "n={n}");
        }
    }

    #[test]
    fn too_few_players_is_rejected() {
        assert_eq!(
            Game::new(roster(4), Setup::default(), 0).err(),
            Some(EngineError::TooFewPlayers(4))
        );
    }

    #[test]
    fn fixed_wolf_count() {
        for n in [5, 11] {
            let (game, _) = Game::new(roster(n), setup(Some(2), false, false), 3).unwrap();
            let count = |role| game.roles().iter().filter(|&&r| r == role).count();
            assert_eq!(count(Role::Werewolf), 2, "n={n}");
            assert_eq!((count(Role::Seer), count(Role::Doctor)), (1, 1), "n={n}");
            assert_eq!(count(Role::Villager), n - 4, "n={n}");
        }
        let err = |n, wolves, sheriff| {
            Game::new(roster(n), setup(wolves, sheriff, false), 3)
                .err()
                .map(|e| e.to_string())
        };
        assert_eq!(
            err(4, Some(2), false).as_deref(),
            Some("need at least 5 players for 2 wolves")
        );
        // Three wolves need seven seats: six would leave them at parity from the start.
        assert_eq!(
            err(6, Some(3), false).as_deref(),
            Some("need at least 7 players for 3 wolves")
        );
    }

    fn setup(wolves: Option<usize>, sheriff: bool, cupid: bool) -> Setup {
        Setup {
            wolves: wolves.and_then(NonZeroUsize::new),
            sheriff,
            cupid,
        }
    }

    fn sheriff_game(n: usize, seed: u64) -> Game {
        Game::new(roster(n), setup(None, true, false), seed)
            .unwrap()
            .0
    }

    /// Plays silently to the next discussion, returning the armed sheriff's seat.
    fn to_sheriff_turn(game: &mut Game) -> PlayerId {
        play_until(game, |_, _| None, |g| g.chat() == Some(TurnKind::DaySpeak));
        let turns = game.turns();
        let armed: Vec<&TurnSpec> = turns.iter().filter(|t| !t.candidates.is_empty()).collect();
        assert_eq!(armed.len(), 1, "only the sheriff may shoot");
        assert_eq!(armed[0].kind, TurnKind::DaySpeak);
        assert!(!armed[0].candidates.contains(&armed[0].player));
        armed[0].player
    }

    /// Answers the discussion: the sheriff fires at `at` (or holds), everyone else stays quiet.
    fn shoot(game: &mut Game, at: Option<PlayerId>) -> Vec<GameEvent> {
        step(game, |_, t| {
            (!t.candidates.is_empty()).then_some(Action::Target { target: at })
        })
    }

    #[test]
    fn role_counts_with_sheriff() {
        for (n, wolves) in [(5, 1), (8, 2), (12, 3)] {
            let game = sheriff_game(n, 7);
            let count = |role| game.roles().iter().filter(|&&r| r == role).count();
            assert_eq!(count(Role::Werewolf), wolves, "n={n}");
            assert_eq!(count(Role::Sheriff), 1, "n={n}");
            assert_eq!(count(Role::Villager), n - wolves - 3, "n={n}");
        }
        assert_eq!(
            Game::new(roster(5), setup(Some(2), true, false), 0)
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some("need at least 6 players for 2 wolves + sheriff")
        );
        assert!(Game::new(roster(6), setup(Some(2), true, false), 0).is_ok());
    }

    #[test]
    fn sheriff_hitting_a_wolf_reveals_only_the_sheriff() {
        let mut game = sheriff_game(8, 11);
        let sheriff = to_sheriff_turn(&mut game);
        let wolf = seat_with(&game, Role::Werewolf);
        let other = (0..8).find(|&p| p != sheriff && p != wolf).unwrap();
        let hidden = |g: &Game| g.view(other).unwrap().players[sheriff as usize].revealed_role;
        assert_eq!(hidden(&game), None, "the sheriff is secret until they fire");

        let events = shoot(&mut game, Some(wolf));
        assert!(matches!(
            public(&events)[..],
            [
                PublicEvent::Shot { sheriff: s, target: t },
                PublicEvent::Died { player: d, role: Role::Werewolf, cause: DeathCause::Shot },
            ] if *s == sheriff && *t == wolf && *d == wolf
        ));
        assert_eq!(hidden(&game), Some(Role::Sheriff));
        assert!(game.players[sheriff as usize].alive);
        assert_eq!(
            game.shot(),
            Some(ShotRecord {
                sheriff,
                target: wolf,
                hit_wolf: true
            })
        );
        assert_eq!(game.step, Step::DayVote);
    }

    #[test]
    fn sheriff_misfire_kills_both() {
        let mut game = sheriff_game(8, 12);
        let sheriff = to_sheriff_turn(&mut game);
        let villager = seat_with(&game, Role::Villager);
        let events = shoot(&mut game, Some(villager));
        assert!(matches!(
            public(&events)[..],
            [
                PublicEvent::Shot { .. },
                PublicEvent::Died { player: v, role: Role::Villager, cause: DeathCause::Shot },
                PublicEvent::Died { player: s, role: Role::Sheriff, cause: DeathCause::Backfire },
            ] if *v == villager && *s == sheriff
        ));
        assert!(!game.players[sheriff as usize].alive);
        assert_eq!(game.step, Step::DayVote);
    }

    #[test]
    fn holding_fire_or_timing_out_keeps_the_bullet() {
        let mut game = sheriff_game(8, 13);
        to_sheriff_turn(&mut game);
        assert!(shoot(&mut game, None).is_empty(), "holding fire is silent");
        assert_eq!(game.step, Step::DayVote);
        to_sheriff_turn(&mut game);
        assert_eq!(game.day, 2);
        // No answer at all is a timeout, which also holds fire.
        assert!(game.submit(&[]).is_empty());
        to_sheriff_turn(&mut game);
        assert_eq!(game.day, 3);
        assert_eq!(game.shot(), None);
    }

    #[test]
    fn the_bullet_fires_once() {
        let mut game = sheriff_game(8, 14);
        let sheriff = to_sheriff_turn(&mut game);
        let wolves = seats_with(&game, Role::Werewolf);
        shoot(&mut game, Some(wolves[0]));
        play_until(
            &mut game,
            |_, _| None,
            |g| g.day == 2 && g.chat() == Some(TurnKind::DaySpeak),
        );
        assert!(
            game.turns().iter().all(|t| t.candidates.is_empty()),
            "no second shot on day 2"
        );
        assert!(game.shoot(sheriff, wolves[1]).is_empty());
    }

    #[test]
    fn the_sheriff_fires_mid_discussion_and_the_talk_goes_on() {
        let mut game = sheriff_game(8, 11);
        let sheriff = to_sheriff_turn(&mut game);
        let wolf = seat_with(&game, Role::Werewolf);
        let villager = seat_with(&game, Role::Villager);
        assert!(
            game.shoot(villager, wolf).is_empty(),
            "only the sheriff fires"
        );
        assert!(game.shoot(sheriff, sheriff).is_empty(), "not at themselves");
        assert_eq!(game.speak(villager, "Who is it?").len(), 1);

        let events = game.shoot(sheriff, wolf);
        assert!(matches!(
            public(&events)[..],
            [PublicEvent::Shot { .. }, PublicEvent::Died { player, .. }] if *player == wolf
        ));
        assert_eq!(game.chat(), Some(TurnKind::DaySpeak), "the talk goes on");
        let turns = game.turns();
        assert!(
            turns
                .iter()
                .all(|t| t.player != wolf && t.candidates.is_empty())
        );
        assert_eq!(game.speak(sheriff, "Got one.").len(), 1);
        game.close_chat();
        assert_eq!(game.step, Step::DayVote);
    }

    #[test]
    fn a_shot_can_end_the_game() {
        // One wolf: shooting it wins at once, before any vote.
        let mut game = sheriff_game(5, 15);
        to_sheriff_turn(&mut game);
        let wolf = seat_with(&game, Role::Werewolf);
        let events = shoot(&mut game, Some(wolf));
        assert!(matches!(
            public(&events).last(),
            Some(PublicEvent::GameOver {
                winner: Team::Village
            })
        ));
        assert!(game.is_over());

        // A misfire after a night kill leaves one wolf against one villager.
        let mut game = sheriff_game(5, 16);
        let sheriff = seat_with(&game, Role::Sheriff);
        let wolf = seat_with(&game, Role::Werewolf);
        let prey = (0..5).find(|&p| p != sheriff && p != wolf).unwrap();
        step(&mut game, |_, t| match t.kind {
            TurnKind::WolfKill => target(prey),
            _ => None,
        });
        to_sheriff_turn(&mut game);
        let innocent = (0..5)
            .find(|&p| p != sheriff && p != wolf && p != prey)
            .unwrap();
        let events = shoot(&mut game, Some(innocent));
        assert!(matches!(
            public(&events).last(),
            Some(PublicEvent::GameOver {
                winner: Team::Wolves
            })
        ));
    }

    #[test]
    fn village_wins_when_last_wolf_dies() {
        let mut game = game(5, 1);
        let wolf = seat_with(&game, Role::Werewolf);
        let events = play(&mut game, |_, t| match t.kind {
            TurnKind::DayVote if t.player == wolf => target(t.candidates[0]),
            TurnKind::DayVote => target(wolf),
            _ => None,
        });
        assert_eq!(game.winner(), Some(Team::Village));
        assert!(public(&events).iter().any(|e| matches!(
            e,
            PublicEvent::Died { player, role: Role::Werewolf, cause: DeathCause::Vote } if *player == wolf
        )));
        assert!(matches!(
            public(&events).last(),
            Some(PublicEvent::GameOver {
                winner: Team::Village
            })
        ));
    }

    #[test]
    fn wolves_win_at_parity() {
        let mut game = game(5, 2);
        // Wolf eats someone every night; the village never eliminates anyone.
        play(&mut game, |_, t| match t.kind {
            TurnKind::WolfKill => target(t.candidates[0]),
            _ => None,
        });
        assert_eq!(game.winner(), Some(Team::Wolves));
        // 5 players: deaths on nights 1-3 leave one wolf and one villager.
        assert_eq!(game.days_played(), 3);
        assert_eq!(game.players.iter().filter(|p| p.alive).count(), 2);
    }

    #[test]
    fn doctor_save_gives_peaceful_night() {
        let mut game = game(5, 3);
        let prey = game
            .turns()
            .iter()
            .find(|t| t.kind == TurnKind::WolfKill)
            .unwrap()
            .candidates[0];
        let events = step(&mut game, |_, t| match t.kind {
            TurnKind::WolfKill | TurnKind::DoctorProtect => target(prey),
            _ => None,
        });
        assert!(
            public(&events)
                .iter()
                .any(|e| matches!(e, PublicEvent::PeacefulNight))
        );
        assert!(game.players[prey as usize].alive);
    }

    #[test]
    fn doctor_cannot_protect_same_player_twice_in_a_row() {
        let mut game = game(5, 4);
        let prey = game
            .turns()
            .iter()
            .find(|t| t.kind == TurnKind::WolfKill)
            .unwrap()
            .candidates[0];
        step(&mut game, |_, t| match t.kind {
            TurnKind::DoctorProtect => target(prey),
            _ => None,
        });
        play_until(&mut game, |_, _| None, |g| g.phase == Phase::Night { n: 2 });

        let turns = game.turns();
        let doctor = turns
            .iter()
            .find(|t| t.kind == TurnKind::DoctorProtect)
            .unwrap();
        assert!(!doctor.candidates.contains(&prey));

        // Insisting anyway is ignored, so the wolf's kill lands.
        let events = step(&mut game, |_, t| match t.kind {
            TurnKind::WolfKill | TurnKind::DoctorProtect => target(prey),
            _ => None,
        });
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GameEvent::Private(_, PrivateEvent::Protected { .. })))
        );
        assert!(!game.players[prey as usize].alive);
    }

    #[test]
    fn vote_tie_eliminates_nobody() {
        let mut game = game(6, 5);
        play_until(&mut game, |_, _| None, |g| g.step == Step::DayVote);
        let events = step(&mut game, |_, t| match t.player {
            0 | 2 => target(1),
            1 | 3 => target(0),
            _ => None,
        });
        assert!(
            public(&events)
                .iter()
                .any(|e| matches!(e, PublicEvent::NoElimination))
        );
        assert!(game.players.iter().all(|p| p.alive));
    }

    #[test]
    fn invalid_answers_default_to_abstain() {
        let mut game = game(5, 6);
        play_until(&mut game, |_, _| None, |g| g.step == Step::DayVote);
        // Voting for yourself and speaking on a vote turn are both invalid.
        let events = step(&mut game, |_, t| match t.player {
            0 => target(0),
            _ => speak("hello"),
        });
        assert!(public(&events).iter().all(|e| !matches!(
            e,
            PublicEvent::Voted {
                target: Some(_),
                ..
            }
        )));
        assert!(
            public(&events)
                .iter()
                .any(|e| matches!(e, PublicEvent::NoElimination))
        );
    }

    #[test]
    fn timeouts_default_to_silence_and_abstain_until_day_ten() {
        let mut game = game(8, 7);
        let events = play(&mut game, |_, _| None);
        let public = public(&events);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GameEvent::Private(_, PrivateEvent::WolfSpoke { .. })))
        );
        assert!(
            !public
                .iter()
                .any(|e| matches!(e, PublicEvent::Spoke { .. }))
        );
        assert!(public.iter().all(|e| !matches!(
            e,
            PublicEvent::Voted {
                target: Some(_),
                ..
            }
        )));
        let peaceful = public
            .iter()
            .filter(|e| matches!(e, PublicEvent::PeacefulNight))
            .count();
        let hung = public
            .iter()
            .filter(|e| matches!(e, PublicEvent::NoElimination))
            .count();
        assert_eq!((peaceful, hung), (LAST_DAY as usize, LAST_DAY as usize));
        assert_eq!(game.winner(), Some(Team::Wolves));
        assert_eq!(game.days_played(), LAST_DAY);
    }

    #[test]
    fn wolf_chat_window_is_for_living_wolves_only() {
        let mut game = game(8, 8);
        let wolves: Vec<PlayerId> = (0..8)
            .filter(|&p| game.roles()[p as usize] == Role::Werewolf)
            .collect();
        let villager = seat_with(&game, Role::Villager);
        assert_eq!(game.chat(), Some(TurnKind::WolfChat));
        let holders: Vec<PlayerId> = game.turns().iter().map(|t| t.player).collect();
        assert_eq!(holders, wolves);

        assert_eq!(game.speak(wolves[1], "eat p0").len(), 1);
        assert!(game.speak(villager, "let me in").is_empty());
        assert!(game.speak(wolves[0], "  ").is_empty());
        let view = game.view(wolves[0]).unwrap();
        assert!(
            matches!(&view.private_log[..], [PrivateEvent::WolfSpoke { text, .. }] if text == "eat p0")
        );

        game.close_chat();
        assert_eq!(game.chat(), None);
        assert!(game.turns().iter().all(|t| t.kind == TurnKind::WolfKill
            || t.kind == TurnKind::SeerInspect
            || t.kind == TurnKind::DoctorProtect));
        assert!(game.speak(wolves[0], "too late").is_empty());
    }

    #[test]
    fn day_discussion_lets_anyone_living_speak_any_time() {
        let mut game = game(5, 9);
        play_until(
            &mut game,
            |_, _| None,
            |g| g.chat() == Some(TurnKind::DaySpeak),
        );
        assert_eq!(game.turns().len(), 5);
        for p in [3, 0, 3] {
            assert_eq!(game.speak(p, "hello").len(), 1);
        }
        let spoken = game
            .public_log
            .iter()
            .filter(|e| matches!(e, PublicEvent::Spoke { .. }))
            .count();
        assert_eq!(spoken, 3);
        game.close_chat();
        assert_eq!(game.step, Step::DayVote);
    }

    #[test]
    fn dead_chat_needs_two_ghosts_and_reaches_only_the_dead() {
        let mut game = game(8, 8);
        let villagers: Vec<PlayerId> = (0..8)
            .filter(|&p| game.roles()[p as usize] == Role::Villager)
            .collect();
        let (first, second, late) = (villagers[0], villagers[1], villagers[2]);
        let policy = |g: &Game, t: &TurnSpec| match t.kind {
            TurnKind::WolfKill if g.day == 1 => target(first),
            TurnKind::DayVote if g.day == 1 => target(second),
            TurnKind::DayVote if g.day == 2 => target(late),
            _ => None,
        };
        let discussion = |day| move |g: &Game| g.day == day && g.chat() == Some(TurnKind::DaySpeak);

        play_until(&mut game, policy, discussion(1));
        assert!(game.ghosts().is_empty(), "one ghost has nobody to talk to");
        assert!(game.speak(first, "anyone?").is_empty());

        play_until(&mut game, policy, discussion(2));
        assert_eq!(game.ghosts(), vec![first, second]);
        assert!(matches!(
            &game.speak(first, "boo")[..],
            [GameEvent::Private(Audience::Dead, PrivateEvent::DeadSpoke { player, .. })] if *player == first
        ));
        assert!(game.speak(second, "  ").is_empty());
        let seer = seat_with(&game, Role::Seer);
        assert!(matches!(
            &game.speak(seer, "hi")[..],
            [GameEvent::Public(PublicEvent::Spoke { .. })]
        ));
        for you in 0..8 {
            let json = serde_json::to_string(&game.view(you).unwrap()).unwrap();
            let ghost = you == first || you == second;
            assert_eq!(json.contains("boo"), ghost, "dead chat for {you}");
        }

        play_until(&mut game, policy, |g| g.chat() == Some(TurnKind::WolfChat));
        assert!(game.ghosts().is_empty(), "no dead chat at night");
        play_until(&mut game, policy, discussion(3));
        assert_eq!(game.ghosts(), vec![first, second, late]);
        game.speak(second, "welcome");
        let view = serde_json::to_string(&game.view(late).unwrap()).unwrap();
        assert!(
            !view.contains("boo"),
            "the dead chat before your death stays hidden"
        );
        assert!(view.contains("welcome"));
    }

    #[test]
    fn same_seed_same_game() {
        let run = |seed| {
            let mut game = game(8, seed);
            // Each wolf votes for a different target to force a random tie-break.
            let events = play(&mut game, |_, t| match t.kind {
                TurnKind::WolfKill => target(t.candidates[t.player as usize % t.candidates.len()]),
                _ => None,
            });
            format!("{events:?}")
        };
        assert_eq!(run(9), run(9));
    }

    #[test]
    fn views_do_not_leak() {
        const SECRET: &str = "wolves-only-secret";
        let mut game = game(8, 10);
        let seer = seat_with(&game, Role::Seer);
        let doctor = seat_with(&game, Role::Doctor);
        let villager = seat_with(&game, Role::Villager);
        let wolves: Vec<PlayerId> = (0..8)
            .filter(|&p| game.roles()[p as usize] == Role::Werewolf)
            .collect();
        play_until(
            &mut game,
            |_, t| match t.kind {
                TurnKind::WolfChat => speak(SECRET),
                TurnKind::WolfKill => target(villager),
                TurnKind::SeerInspect => target(wolves[0]),
                TurnKind::DoctorProtect => target(doctor),
                _ => None,
            },
            |g| g.step == Step::DayVote,
        );
        assert!(
            !game.players[villager as usize].alive,
            "night 1 should have a victim"
        );

        for you in 0..8 {
            let view = game.view(you).unwrap();
            let role = game.roles()[you as usize];
            assert_eq!(view.role, role);
            assert!(view.players[0].human && !view.players[1].human);
            for p in &view.players {
                assert_eq!(
                    p.revealed_role.is_some(),
                    !p.alive,
                    "role of living {} leaked to {you}",
                    p.id
                );
            }
            let json = serde_json::to_string(&view).unwrap();
            match role {
                Role::Werewolf => {
                    assert_eq!(
                        view.wolf_team,
                        wolves
                            .iter()
                            .copied()
                            .filter(|&w| w != you)
                            .collect::<Vec<_>>()
                    );
                    assert!(json.contains(SECRET));
                    assert!(view.private_log.iter().all(|e| matches!(
                        e,
                        PrivateEvent::WolfSpoke { .. } | PrivateEvent::WolfVoted { .. }
                    )));
                }
                _ => {
                    assert!(view.wolf_team.is_empty(), "wolf team leaked to {you}");
                    assert!(!json.contains(SECRET), "wolf chat leaked to {you}");
                }
            }
            match role {
                Role::Seer => {
                    assert_eq!(you, seer);
                    assert!(matches!(
                        &view.private_log[..],
                        [PrivateEvent::Inspected {
                            is_werewolf: true,
                            ..
                        }]
                    ));
                }
                Role::Doctor => {
                    assert!(
                        matches!(&view.private_log[..], [PrivateEvent::Protected { target }] if *target == doctor)
                    );
                }
                Role::Villager | Role::Sheriff | Role::Cupid => {
                    assert!(view.private_log.is_empty())
                }
                Role::Werewolf => {}
            }
        }
    }

    fn cupid_game(n: usize, sheriff: bool, seed: u64) -> Game {
        Game::new(roster(n), setup(None, sheriff, true), seed)
            .unwrap()
            .0
    }

    fn seats_with(game: &Game, role: Role) -> Vec<PlayerId> {
        (0..)
            .zip(game.roles())
            .filter(|&(_, &r)| r == role)
            .map(|(p, _)| p)
            .collect()
    }

    /// Answers Cupid's two turns, linking `a` and `b`.
    fn link(game: &mut Game, a: PlayerId, b: PlayerId) -> Vec<GameEvent> {
        assert!(
            step(game, |_, _| target(a)).is_empty(),
            "the first pick is secret"
        );
        let events = step(game, |_, _| target(b));
        assert_eq!(game.lover_of(a), Some(b));
        events
    }

    fn deaths(events: &[GameEvent]) -> Vec<(PlayerId, DeathCause)> {
        public(events)
            .into_iter()
            .filter_map(|e| match e {
                PublicEvent::Died { player, cause, .. } => Some((*player, *cause)),
                _ => None,
            })
            .collect()
    }

    /// Wolves kill, and everyone votes for, the first candidate who isn't a lover.
    fn spare_lovers(game: &Game, turn: &TurnSpec) -> Option<Action> {
        let target = turn
            .candidates
            .iter()
            .copied()
            .find(|&c| game.lover_of(c).is_none());
        match turn.kind {
            TurnKind::WolfKill | TurnKind::DayVote => Some(Action::Target { target }),
            _ => None,
        }
    }

    #[test]
    fn role_counts_with_cupid() {
        for (n, wolves) in [(6, 1), (8, 2), (12, 3)] {
            let game = cupid_game(n, false, 7);
            let count = |role| game.roles().iter().filter(|&&r| r == role).count();
            assert_eq!(count(Role::Werewolf), wolves, "n={n}");
            assert_eq!(count(Role::Cupid), 1, "n={n}");
            assert_eq!(count(Role::Villager), n - wolves - 3, "n={n}");
        }
        // Off by default: no cupid and no link turn.
        let game = game(8, 7);
        assert!(!game.roles().contains(&Role::Cupid));
        assert_eq!(game.chat(), Some(TurnKind::WolfChat));
    }

    #[test]
    fn players_needed_covers_every_setup() {
        for (wolves, sheriff, cupid, need) in [
            (1, false, false, 5),
            (1, true, false, 5),
            (1, false, true, 5),
            (1, true, true, 6),
            (2, false, false, 5),
            (2, true, false, 6),
            (2, false, true, 6),
            (2, true, true, 7),
            (3, false, false, 7),
            (3, true, false, 7),
            (3, false, true, 7),
            (3, true, true, 8),
        ] {
            let case = format!("{wolves} wolves, sheriff={sheriff}, cupid={cupid}");
            assert_eq!(players_needed(wolves, sheriff, cupid), need, "{case}");
            let setup = setup(Some(wolves), sheriff, cupid);
            assert!(Game::new(roster(need), setup, 0).is_ok(), "{case}");
            assert_eq!(
                Game::new(roster(need - 1), setup, 0).err(),
                Some(EngineError::TooFewForSetup {
                    setup: describe_setup(wolves, sheriff, cupid),
                    need
                }),
                "{case}"
            );
        }
        assert_eq!(
            Game::new(roster(6), setup(Some(2), true, true), 0)
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some("need at least 7 players for 2 wolves + sheriff + cupid")
        );
        // Without a fixed count, five seats get one wolf, which can't fit a sheriff and a cupid.
        assert_eq!(
            Game::new(roster(5), setup(None, true, true), 0)
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some("need at least 6 players for 1 wolves + sheriff + cupid")
        );
    }

    #[test]
    fn cupid_links_two_lovers_before_wolf_chat() {
        let mut game = cupid_game(8, false, 21);
        let cupid = seat_with(&game, Role::Cupid);
        let wolf = seat_with(&game, Role::Werewolf);
        assert_eq!(game.phase, Phase::Night { n: 1 });
        assert_eq!(
            game.turns(),
            [TurnSpec {
                player: cupid,
                kind: TurnKind::CupidLink,
                candidates: (0..8).collect(),
            }],
            "the first pick may be Cupid"
        );
        assert!(step(&mut game, |_, _| target(cupid)).is_empty());
        assert_eq!(
            game.turns(),
            [TurnSpec {
                player: cupid,
                kind: TurnKind::CupidLink,
                candidates: (0..8).filter(|&p| p != cupid).collect(),
            }],
            "the second pick excludes the first"
        );

        let events = step(&mut game, |_, _| target(wolf));
        assert!(
            matches!(
                &events[..],
                [
                    GameEvent::Private(Audience::Cupid, PrivateEvent::Linked { a, b }),
                    GameEvent::Private(
                        Audience::Lover(l1),
                        PrivateEvent::InLove { partner: p1, partner_role: Role::Werewolf }
                    ),
                    GameEvent::Private(
                        Audience::Lover(l2),
                        PrivateEvent::InLove { partner: p2, partner_role: Role::Cupid }
                    ),
                ] if *a == cupid && *b == wolf && *l1 == cupid && *p1 == wolf && *l2 == wolf && *p2 == cupid
            ),
            "{events:?}"
        );
        assert_eq!(
            (game.lover_of(cupid), game.lover_of(wolf)),
            (Some(wolf), Some(cupid))
        );
        assert_eq!(game.chat(), Some(TurnKind::WolfChat), "night 1 goes on");

        for you in 0..8 {
            let log = game.view(you).unwrap().private_log;
            let linked = log
                .iter()
                .filter(|e| matches!(e, PrivateEvent::Linked { .. }))
                .count();
            assert_eq!(linked, usize::from(you == cupid), "Linked for {you}");
            let partners: Vec<PlayerId> = log
                .iter()
                .filter_map(|e| match e {
                    PrivateEvent::InLove { partner, .. } => Some(*partner),
                    _ => None,
                })
                .collect();
            let expected = match you {
                p if p == cupid => vec![wolf],
                p if p == wolf => vec![cupid],
                _ => Vec::new(),
            };
            assert_eq!(partners, expected, "InLove for {you}");
        }

        play_until(&mut game, |_, _| None, |g| g.phase == Phase::Night { n: 2 });
        assert!(
            game.turns().iter().all(|t| t.kind != TurnKind::CupidLink),
            "Cupid acts on night 1 only"
        );
    }

    #[test]
    fn null_invalid_or_timeout_on_either_pick_means_no_lovers() {
        let pass = Some(Action::Target { target: None });
        let pick = target(0);
        // `None` is a timeout; picking seat 0 twice is invalid the second time.
        for (case, first, second) in [
            ("null first", pass.clone(), None),
            ("timeout first", None, None),
            ("null second", pick.clone(), pass.clone()),
            ("timeout second", pick.clone(), None),
            ("repeat second", pick.clone(), pick.clone()),
        ] {
            let mut game = cupid_game(8, false, 31);
            let mut events = step(&mut game, |_, _| first.clone());
            if first.is_some_and(|a| a != Action::Target { target: None }) {
                assert_eq!(game.turns()[0].kind, TurnKind::CupidLink, "{case}");
                events.extend(step(&mut game, |_, _| second.clone()));
            }
            assert!(events.is_empty(), "{case}: {events:?}");
            assert_eq!(game.chat(), Some(TurnKind::WolfChat), "{case}");
            assert!((0..8).all(|p| game.lover_of(p).is_none()), "{case}");
            assert!(
                (0..8).all(|p| game.view(p).unwrap().private_log.is_empty()),
                "{case}"
            );
        }
    }

    #[test]
    fn a_lover_dies_of_heartbreak_after_a_night_kill_or_a_vote() {
        let mut game = cupid_game(8, false, 22);
        let villagers = seats_with(&game, Role::Villager);
        let (a, b) = (villagers[0], villagers[1]);
        link(&mut game, a, b);
        let events = play_until(
            &mut game,
            |_, t| match t.kind {
                TurnKind::WolfKill => target(a),
                _ => None,
            },
            |g| g.chat() == Some(TurnKind::DaySpeak),
        );
        assert_eq!(
            deaths(&events),
            [(a, DeathCause::Night), (b, DeathCause::Heartbreak)]
        );
        assert_eq!(game.died_on(b), Some(Phase::Night { n: 1 }));

        let mut game = cupid_game(8, false, 23);
        let villagers = seats_with(&game, Role::Villager);
        let (a, b) = (villagers[0], villagers[1]);
        link(&mut game, a, b);
        play_until(&mut game, |_, _| None, |g| g.step == Step::DayVote);
        let events = step(&mut game, |_, _| target(a));
        assert_eq!(
            deaths(&events),
            [(a, DeathCause::Vote), (b, DeathCause::Heartbreak)]
        );
        assert_eq!(game.died_on(b), Some(Phase::Day { n: 1 }));
    }

    #[test]
    fn heartbreak_follows_sheriff_shots_and_backfires() {
        use DeathCause::{Backfire, Heartbreak, Shot};

        // A wolf hit: its lover breaks; the sheriff lives.
        let mut game = cupid_game(8, true, 24);
        let wolf = seat_with(&game, Role::Werewolf);
        let villager = seat_with(&game, Role::Villager);
        link(&mut game, wolf, villager);
        let sheriff = to_sheriff_turn(&mut game);
        let events = shoot(&mut game, Some(wolf));
        assert_eq!(deaths(&events), [(wolf, Shot), (villager, Heartbreak)]);
        assert!(game.is_alive(sheriff));

        // A lover missed: the partner breaks, then the sheriff backfires.
        let mut game = cupid_game(8, true, 25);
        let villagers = seats_with(&game, Role::Villager);
        let (a, b) = (villagers[0], villagers[1]);
        link(&mut game, a, b);
        let sheriff = to_sheriff_turn(&mut game);
        let events = shoot(&mut game, Some(a));
        assert_eq!(
            deaths(&events),
            [(a, Shot), (b, Heartbreak), (sheriff, Backfire)]
        );

        // A sheriff in love misfires: the backfire breaks their partner.
        let mut game = cupid_game(8, true, 26);
        let sheriff = seat_with(&game, Role::Sheriff);
        let seer = seat_with(&game, Role::Seer);
        let villager = seat_with(&game, Role::Villager);
        link(&mut game, sheriff, seer);
        to_sheriff_turn(&mut game);
        let events = shoot(&mut game, Some(villager));
        assert_eq!(
            deaths(&events),
            [(villager, Shot), (sheriff, Backfire), (seer, Heartbreak)]
        );

        // A sheriff who shoots their own lover dies of heartbreak, not a second time by backfire.
        let mut game = cupid_game(8, true, 27);
        let sheriff = seat_with(&game, Role::Sheriff);
        let seer = seat_with(&game, Role::Seer);
        link(&mut game, sheriff, seer);
        to_sheriff_turn(&mut game);
        let events = shoot(&mut game, Some(seer));
        assert_eq!(deaths(&events), [(seer, Shot), (sheriff, Heartbreak)]);
        assert_eq!(game.step, Step::DayVote);
    }

    #[test]
    fn cross_team_lovers_win_as_the_last_two_alive() {
        let mut game = cupid_game(6, false, 28);
        let wolf = seat_with(&game, Role::Werewolf);
        let cupid = seat_with(&game, Role::Cupid);
        link(&mut game, wolf, cupid);
        let events = play(&mut game, spare_lovers);
        // Parity alone would hand the last two to the wolves.
        assert_eq!(game.winner(), Some(Team::Lovers));
        assert!(matches!(
            public(&events).last(),
            Some(PublicEvent::GameOver {
                winner: Team::Lovers
            })
        ));
        let living: Vec<PlayerId> = game.living().collect();
        assert_eq!(living.len(), 2);
        assert!(living.contains(&wolf) && living.contains(&cupid));
        for p in 0..6 {
            assert_eq!(game.won(p), p == wolf || p == cupid, "seat {p}");
        }
    }

    #[test]
    fn cross_team_lovers_never_win_with_their_team() {
        let mut game = cupid_game(8, false, 29);
        let wolves = seats_with(&game, Role::Werewolf);
        let villager = seat_with(&game, Role::Villager);
        link(&mut game, wolves[0], villager);
        // Wolves kill a non-lover each night and nobody is ever voted out.
        play(&mut game, |g, t| match t.kind {
            TurnKind::WolfKill => spare_lovers(g, t),
            _ => None,
        });
        assert_eq!(game.winner(), Some(Team::Wolves));
        assert!(game.is_alive(wolves[0]) && game.is_alive(villager));
        assert_eq!(
            game.living().count(),
            4,
            "the wolf lover counts toward parity"
        );
        for p in 0..8 {
            assert_eq!(game.won(p), p == wolves[1], "seat {p}");
        }
    }

    #[test]
    fn same_team_lovers_play_for_their_team() {
        // Two wolves in love: voting one out takes the pack.
        let mut game = cupid_game(8, false, 30);
        let wolves = seats_with(&game, Role::Werewolf);
        link(&mut game, wolves[0], wolves[1]);
        let events = play(&mut game, |_, t| match t.kind {
            TurnKind::DayVote => target(wolves[0]),
            _ => None,
        });
        assert_eq!(
            deaths(&events),
            [
                (wolves[0], DeathCause::Vote),
                (wolves[1], DeathCause::Heartbreak)
            ]
        );
        assert_eq!(game.winner(), Some(Team::Village));
        assert_eq!(game.days_played(), 1);
        for p in 0..8 {
            assert_eq!(game.won(p), !wolves.contains(&p), "seat {p}");
        }

        // Two villagers in love win with the village.
        let mut game = cupid_game(8, false, 31);
        let villagers = seats_with(&game, Role::Villager);
        link(&mut game, villagers[0], villagers[1]);
        play(&mut game, |g, t| match t.kind {
            TurnKind::DayVote => g.living().find(|&p| g.is_wolf(p)).and_then(target),
            _ => None,
        });
        assert_eq!(game.winner(), Some(Team::Village));
        assert!(game.won(villagers[0]) && game.won(villagers[1]));
    }
}
