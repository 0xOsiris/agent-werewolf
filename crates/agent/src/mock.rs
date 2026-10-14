//! Random valid actions with no LLM, for exercising the server.

use crate::reply::{Decision, MAX_REASONING_LEN, is_speak, truncate};
use rand::{RngExt, SeedableRng, rngs::StdRng, seq::IndexedRandom};
use werewolf_protocol::{Action, MAX_TEXT_LEN, Turn};

/// What the mock says, paired with the thought behind it, in the style the LLM prompt asks for.
const LINES: &[(&str, &str)] = &[
    (
        "I don't trust {name}.",
        "I don't trust {name}, they dodged my question.",
    ),
    (
        "{name} has been very quiet.",
        "{name} is too quiet. I want them talking.",
    ),
    (
        "{name}, where were you last night?",
        "I want to see if {name} slips up.",
    ),
    (
        "My gut says {name} is hiding something.",
        "{name} seems nervous. I think they're hiding something.",
    ),
    (
        "I think {name} is telling the truth.",
        "{name} sounds honest to me, so I'll back them.",
    ),
    (
        "Let's hear from {name} before we vote.",
        "I can't read {name} yet. I need to hear more.",
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
        if is_speak(turn.kind) {
            let others: Vec<&str> = view
                .players
                .iter()
                .filter(|p| p.alive && p.id != view.you)
                .map(|p| p.name.as_str())
                .collect();
            let name = others.choose(&mut self.rng).copied().unwrap_or("everyone");
            let (line, thought) = LINES
                .choose(&mut self.rng)
                .copied()
                .unwrap_or(("{name}?", "{name} worries me."));
            return Decision {
                action: Action::Speak {
                    text: truncate(&line.replace("{name}", name), MAX_TEXT_LEN).into(),
                },
                reasoning: Some(
                    truncate(&thought.replace("{name}", name), MAX_REASONING_LEN).into(),
                ),
            };
        }
        let target = if self.rng.random_bool(0.1) {
            None
        } else {
            turn.candidates.choose(&mut self.rng).copied()
        };
        let thought = match target.and_then(|id| view.players.iter().find(|p| p.id == id)) {
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
    use werewolf_protocol::{Phase, PlayerView, PublicPlayer, Role, TurnKind};

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
            candidates: if kind == TurnKind::DaySpeak {
                vec![]
            } else {
                vec![2, 3]
            },
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
}
