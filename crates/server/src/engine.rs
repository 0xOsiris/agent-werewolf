//! Pure, deterministic game rules. No I/O: the driver hands out [`Game::turns`] and feeds the
//! answers back through [`Game::submit`].

use std::collections::BTreeMap;

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
}

/// Which role may see a private event.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Wolves,
    Seer,
    Doctor,
}

impl Audience {
    pub(crate) fn of(role: Role) -> Option<Self> {
        match role {
            Role::Werewolf => Some(Self::Wolves),
            Role::Seer => Some(Self::Seer),
            Role::Doctor => Some(Self::Doctor),
            Role::Villager => None,
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
    WolfChat { speaker: PlayerId },
    NightActions,
    DaySpeak { round: u32, speaker: PlayerId },
    DayVote,
    Over,
}

pub struct Game {
    rng: StdRng,
    players: Vec<PublicPlayer>,
    roles: Vec<Role>,
    died_on: Vec<Option<Phase>>,
    phase: Phase,
    step: Step,
    /// Number of the current night/day; night n precedes day n.
    day: u32,
    speak_rounds: u32,
    last_protected: Option<PlayerId>,
    winner: Option<Team>,
    public_log: Vec<PublicEvent>,
    private_log: Vec<(Audience, PrivateEvent)>,
    outbox: Vec<GameEvent>,
}

impl Game {
    /// Seats `roster` in order (ids become seat numbers), deals roles, and starts night 1.
    pub fn new(
        roster: Vec<PublicPlayer>,
        speak_rounds: u32,
        seed: u64,
    ) -> Result<(Self, Vec<GameEvent>), EngineError> {
        let n = roster.len();
        if n < MIN_PLAYERS {
            return Err(EngineError::TooFewPlayers(n));
        }
        let mut rng = StdRng::seed_from_u64(seed);
        let mut roles = vec![Role::Werewolf; (n / 4).max(1)];
        roles.extend([Role::Seer, Role::Doctor]);
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
            phase: Phase::Lobby,
            step: Step::Over,
            day: 0,
            speak_rounds,
            last_protected: None,
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

    /// Turns to hand out now, in seat order. Empty only once the game is over.
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
            Step::WolfChat { speaker } => vec![spec(speaker, TurnKind::WolfChat, Vec::new())],
            Step::DaySpeak { speaker, .. } => vec![spec(speaker, TurnKind::DaySpeak, Vec::new())],
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
                    Role::Villager => None,
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
    /// get the default action (silence / abstain).
    pub fn submit(&mut self, answers: &[Answer]) -> Vec<GameEvent> {
        let turns = self.turns();
        match self.step {
            Step::WolfChat { speaker } => {
                if let Some(text) = spoken(speaker, answers) {
                    self.emit_private(
                        Audience::Wolves,
                        PrivateEvent::WolfSpoke {
                            player: speaker,
                            text,
                        },
                    );
                }
                self.step = match self.next_living_after(speaker, true) {
                    Some(speaker) => Step::WolfChat { speaker },
                    None => Step::NightActions,
                };
            }
            Step::NightActions => self.resolve_night(&turns, answers),
            Step::DaySpeak { round, speaker } => {
                if let Some(text) = spoken(speaker, answers) {
                    self.emit_public(PublicEvent::Spoke {
                        player: speaker,
                        text,
                    });
                }
                self.step = match self.next_living_after(speaker, false) {
                    Some(speaker) => Step::DaySpeak { round, speaker },
                    None => self.discussion_step(round + 1),
                };
            }
            Step::DayVote => self.resolve_vote(&turns, answers),
            Step::Over => {}
        }
        self.take_events()
    }

    /// Everything `you` may know. `None` for an unknown seat.
    pub fn view(&self, you: PlayerId) -> Option<PlayerView> {
        let role = *self.roles.get(you as usize)?;
        let audience = Audience::of(role);
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
                .filter(|(seen_by, _)| Some(*seen_by) == audience)
                .map(|(_, event)| event.clone())
                .collect(),
        })
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
                TurnKind::DaySpeak | TurnKind::DayVote | TurnKind::WolfChat => {}
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
            self.step = self.discussion_step(0);
        }
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
        let first_two = {
            let mut wolves = self.living().filter(|&p| self.is_wolf(p));
            (wolves.next(), wolves.next())
        };
        self.step = match first_two {
            (Some(speaker), Some(_)) => Step::WolfChat { speaker },
            _ => Step::NightActions,
        };
    }

    /// First speaker of discussion `round`, or the vote once discussion is done.
    fn discussion_step(&self, round: u32) -> Step {
        match self.living().next() {
            Some(speaker) if round < self.speak_rounds => Step::DaySpeak { round, speaker },
            _ => Step::DayVote,
        }
    }

    fn kill(&mut self, player: PlayerId, cause: DeathCause, when: Phase) {
        let role = self.role(player);
        if let Some(seat) = self.players.get_mut(player as usize) {
            seat.alive = false;
            seat.revealed_role = Some(role);
        }
        if let Some(slot) = self.died_on.get_mut(player as usize) {
            *slot = Some(when);
        }
        self.emit_public(PublicEvent::Died {
            player,
            role,
            cause,
        });
        if let Some(winner) = self.check_win() {
            self.finish(winner);
        }
    }

    fn check_win(&self) -> Option<Team> {
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

    fn next_living_after(&self, player: PlayerId, wolves_only: bool) -> Option<PlayerId> {
        self.living()
            .find(|&p| p > player && (!wolves_only || self.is_wolf(p)))
    }

    fn seats(&self) -> impl Iterator<Item = PlayerId> + '_ {
        self.players.iter().map(|p| p.id)
    }

    fn living(&self) -> impl Iterator<Item = PlayerId> + '_ {
        self.players.iter().filter(|p| p.alive).map(|p| p.id)
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

/// The player's speech, or `None` for silence.
fn spoken(player: PlayerId, answers: &[Answer]) -> Option<String> {
    match &answers.iter().find(|a| a.player == player)?.action {
        Some(Action::Speak { text }) if !text.trim().is_empty() => Some(text.clone()),
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
            })
            .collect()
    }

    fn game(n: usize, seed: u64) -> Game {
        Game::new(roster(n), 2, seed).unwrap().0
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
            Game::new(roster(4), 2, 0).err(),
            Some(EngineError::TooFewPlayers(4))
        );
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
    fn wolf_chat_runs_in_seat_order_and_sees_prior_messages() {
        let mut game = game(8, 8);
        let wolves: Vec<PlayerId> = (0..8)
            .filter(|&p| game.roles()[p as usize] == Role::Werewolf)
            .collect();
        let first = game.turns();
        assert_eq!(
            first,
            vec![TurnSpec {
                player: wolves[0],
                kind: TurnKind::WolfChat,
                candidates: vec![]
            }]
        );
        step(&mut game, |_, _| speak("eat p0"));
        let second = game.turns();
        assert_eq!(second[0].player, wolves[1]);
        let view = game.view(wolves[1]).unwrap();
        assert!(
            matches!(&view.private_log[..], [PrivateEvent::WolfSpoke { text, .. }] if text == "eat p0")
        );
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
                Role::Villager => assert!(view.private_log.is_empty()),
                Role::Werewolf => {}
            }
        }
    }
}
