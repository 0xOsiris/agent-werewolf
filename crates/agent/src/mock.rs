//! Random valid actions with no LLM, for exercising the server.

use crate::{
    chat,
    reply::{Decision, MAX_REASONING_LEN, MAX_SAY_LEN, is_speak, shorten, truncate},
};
use rand::{RngExt, SeedableRng, rngs::StdRng, seq::IndexedRandom};
use werewolf_protocol::{Action, Turn, TurnKind};

/// Chance a mock sheriff fires on each shoot turn; low, since a miss kills the sheriff too.
const SHERIFF_FIRE_CHANCE: f64 = 0.25;

/// Mocks speak this often per chat window and then pass, so mock games finish fast.
const CHAT_MESSAGES_PER_WINDOW: usize = 2;

/// Chance a mock ghost speaks on each dead chat turn.
const GHOST_CHANCE: f64 = 0.5;

/// What the mock says, paired with the thought behind it, in the style the LLM prompt asks for.
const LINES: &[(&str, &str)] = &[
    (
        "{name}, why so quiet?",
        "{name} is too quiet. I want them talking.",
    ),
    (
        "Not buying it, {name}.",
        "I don't trust {name}, they dodged my question.",
    ),
    (
        "{name}, where were you last night?",
        "I want to see if {name} slips up.",
    ),
    (
        "{name} is hiding something.",
        "{name} seems nervous. I think they're hiding something.",
    ),
    (
        "I believe {name}.",
        "{name} sounds honest to me, so I'll back them.",
    ),
    (
        "Your move, {name}.",
        "I can't read {name} yet. I need to hear more.",
    ),
];

/// What a mock ghost says in the dead chat, with the thought behind it.
const GHOST_LINES: &[(&str, &str)] = &[
    (
        "Watch your back, {name}.",
        "I'm dead, but I still think {name} is in danger.",
    ),
    (
        "I never trusted {name}.",
        "From down here {name} looks even shadier.",
    ),
    (
        "It's cold in the graveyard.",
        "I'm out of the game. All that's left is gossip.",
    ),
];

pub struct MockBrain {
    rng: StdRng,
}

impl MockBrain {
    pub fn new(seed: Option<u64>) -> Self {
        Self {
            rng: seed.map_or_else(rand::make_rng, StdRng::seed_from_u64),
        }
    }

    pub fn decide(&mut self, turn: &Turn) -> Decision {
        let view = &turn.view;
        let ghost = turn.kind == TurnKind::DeadChat;
        if ghost && !self.rng.random_bool(GHOST_CHANCE) {
            return Decision::fallback(turn.kind);
        }
        if is_speak(turn.kind) && chat::own_messages(view, turn.kind) >= CHAT_MESSAGES_PER_WINDOW {
            return Decision {
                action: Action::Speak {
                    text: String::new(),
                },
                reasoning: Some("I've said my piece. I'll listen for now.".into()),
            };
        }
        if is_speak(turn.kind) {
            let others: Vec<&str> = view
                .players
                .iter()
                .filter(|p| p.alive && p.id != view.you)
                .map(|p| p.name.as_str())
                .collect();
            let name = others.choose(&mut self.rng).copied().unwrap_or("everyone");
            let lines = if ghost { GHOST_LINES } else { LINES };
            let (line, thought) = lines
                .choose(&mut self.rng)
                .copied()
                .unwrap_or(("{name}?", "{name} worries me."));
            return Decision {
                action: Action::Speak {
                    text: shorten(&line.replace("{name}", name), MAX_SAY_LEN),
                },
                reasoning: Some(
                    truncate(&thought.replace("{name}", name), MAX_REASONING_LEN).into(),
                ),
            };
        }
        let hold = match turn.kind {
            TurnKind::SheriffShoot => 1.0 - SHERIFF_FIRE_CHANCE,
            // Always link, so mock games exercise the Lovers.
            TurnKind::CupidLink => 0.0,
            _ => 0.1,
        };
        let target = if self.rng.random_bool(hold) {
            None
        } else {
            turn.candidates.choose(&mut self.rng).copied()
        };
        let thought = match target.and_then(|id| view.players.iter().find(|p| p.id == id)) {
            Some(player) if turn.kind == TurnKind::CupidLink => {
                format!("{} looks lonely. I'll make them a Lover.", player.name)
            }
            Some(player) => format!(
                "Something about {} feels off. I'm going with my gut.",
                player.name
            ),
            None => "I can't decide yet, so I'll pass this time.".to_owned(),
        };
        Decision {
            action: Action::Target { target },
            reasoning: Some(truncate(&thought, MAX_REASONING_LEN).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use werewolf_protocol::{
        MAX_NAME_LEN, Phase, PlayerView, PrivateEvent, PublicEvent, PublicPlayer, Role,
    };

    fn turn(kind: TurnKind) -> Turn {
        let player = |id, name: &str, alive| PublicPlayer {
            id,
            name: name.into(),
            model: None,
            alive,
            revealed_role: None,
            human: false,
        };
        Turn {
            turn_id: 1,
            kind,
            candidates: if is_speak(kind) { vec![] } else { vec![2, 3] },
            deadline_ms: 0,
            view: PlayerView {
                you: 1,
                role: Role::Villager,
                phase: Phase::Day { n: 1 },
                players: vec![
                    player(1, "Me", true),
                    player(2, "Bob", true),
                    player(3, "Carol", false),
                ],
                wolf_team: vec![],
                public_log: vec![],
                private_log: vec![],
            },
        }
    }

    #[test]
    fn mock_actions_are_valid_and_seeded() {
        let mut brain = MockBrain::new(Some(7));
        for _ in 0..100 {
            let speak = brain.decide(&turn(TurnKind::DaySpeak));
            let Action::Speak { text } = speak.action else {
                panic!("expected speak");
            };
            assert!(text.contains("Bob"), "only living others are named: {text}");
            let thought = speak.reasoning.unwrap();
            assert!(thought.contains("Bob") && thought.chars().count() <= MAX_REASONING_LEN);

            let vote = brain.decide(&turn(TurnKind::DayVote));
            let Action::Target { target } = vote.action else {
                panic!("expected target");
            };
            assert!(target.is_none_or(|t| [2, 3].contains(&t)));
            let thought = vote.reasoning.unwrap();
            match target {
                Some(2) => assert!(thought.contains("Bob"), "{thought}"),
                Some(_) => assert!(thought.contains("Carol"), "{thought}"),
                None => assert!(thought.contains("pass"), "{thought}"),
            }
        }

        let mut a = MockBrain::new(Some(42));
        let mut b = MockBrain::new(Some(42));
        for _ in 0..10 {
            assert_eq!(
                a.decide(&turn(TurnKind::DayVote)),
                b.decide(&turn(TurnKind::DayVote))
            );
        }
    }

    fn spoke(decision: &Decision) -> bool {
        matches!(&decision.action, Action::Speak { text } if !text.is_empty())
    }

    #[test]
    fn mock_speaks_twice_per_chat_window_then_passes() {
        let mut brain = MockBrain::new(Some(3));
        let said = |text: &str| PublicEvent::Spoke {
            player: 1,
            text: text.into(),
        };
        let mut day = turn(TurnKind::DaySpeak);
        day.view.public_log = vec![
            said("yesterday"),
            said("yesterday again"),
            PublicEvent::PhaseStarted {
                phase: Phase::Day { n: 2 },
            },
            said("first"),
        ];
        assert!(spoke(&brain.decide(&day)));
        day.view.public_log.push(said("second"));
        let pass = brain.decide(&day);
        assert_eq!(
            pass.action,
            Action::Speak {
                text: String::new()
            }
        );
        assert!(pass.reasoning.is_some());

        let mut wolves = turn(TurnKind::WolfChat);
        let wolf_said = || PrivateEvent::WolfSpoke {
            player: 1,
            text: "x".into(),
        };
        wolves.view.private_log = vec![wolf_said()];
        assert!(spoke(&brain.decide(&wolves)));
        wolves.view.private_log.push(wolf_said());
        assert!(!spoke(&brain.decide(&wolves)));
    }

    #[test]
    fn mock_lines_fit_the_speech_limits() {
        let longest_name = "N".repeat(MAX_NAME_LEN);
        for (line, thought) in LINES.iter().chain(GHOST_LINES) {
            let line = line.replace("{name}", &longest_name);
            assert!(line.split_whitespace().count() <= 15, "{line}");
            assert!(line.chars().count() <= MAX_SAY_LEN, "{line}");
            assert!(thought.split_whitespace().count() <= 25, "{thought}");
        }
    }

    #[test]
    fn mock_ghosts_speak_ghost_lines_or_nothing() {
        let mut brain = MockBrain::new(Some(9));
        let mut ghost = turn(TurnKind::DeadChat);
        ghost.view.players[0].alive = false;
        let mut spoke = 0;
        for _ in 0..300 {
            let Action::Speak { text } = brain.decide(&ghost).action else {
                panic!("expected speak");
            };
            if !text.is_empty() {
                let ghost_line = |(line, _): &(&str, &str)| text == line.replace("{name}", "Bob");
                assert!(GHOST_LINES.iter().any(ghost_line), "{text}");
                spoke += 1;
            }
        }
        assert!((110..=190).contains(&spoke), "spoke {spoke} of 300");
    }

    #[test]
    fn mock_sheriff_fires_about_a_quarter_of_the_time() {
        let mut brain = MockBrain::new(Some(11));
        let shoot = turn(TurnKind::SheriffShoot);
        let mut fired = 0;
        for _ in 0..1000 {
            let Action::Target { target } = brain.decide(&shoot).action else {
                panic!("expected target");
            };
            if let Some(t) = target {
                assert!(shoot.candidates.contains(&t));
                fired += 1;
            }
        }
        assert!((180..=320).contains(&fired), "fired {fired} of 1000");
    }

    #[test]
    fn mock_cupid_always_picks_a_candidate() {
        let mut brain = MockBrain::new(Some(5));
        let link = turn(TurnKind::CupidLink);
        let mut picked = Vec::new();
        for _ in 0..200 {
            let decision = brain.decide(&link);
            let Action::Target { target: Some(t) } = decision.action else {
                panic!("mock cupid passed: {decision:?}");
            };
            assert!(link.candidates.contains(&t));
            assert!(decision.reasoning.unwrap().contains("Lover"));
            picked.push(t);
        }
        assert!(
            picked.contains(&2) && picked.contains(&3),
            "picks are random"
        );
    }
}
