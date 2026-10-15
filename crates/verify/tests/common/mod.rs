//! An honest staked game, played through the engine with deterministic keys.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::num::NonZeroUsize;

use werewolf_engine::{Answer, Game, Setup, TurnSpec};
use werewolf_protocol::{Action, PublicPlayer, Team, TurnKind};
use werewolf_verify::{
    Transcript, TranscriptBuilder,
    alloy_primitives::{Address, B256, B512, Bytes, U256, address},
    encode::{self, Act, Domain, LogChain},
    force,
    keys::{self, SigningKey},
    transcript::{AnswerSource, ChatMessage, Header, OpEntry, RoundAnswer, Rules, Seat, Step},
};

pub const SEATS: u32 = 8;
pub const ENC_SECRET: [u8; 32] = [0x33; 32];
const NAMES: [&str; SEATS as usize] = [
    "Sherlock", "osiris", "Ada", "bjørn", "Mallory", "Zoë", "Trent", "Peggy",
];

pub fn domain() -> Domain {
    Domain {
        chain_id: 31337,
        escrow: address!("5fbdb2315678afecb367f032d93f642f64180aa3"),
        game_id: U256::from(1),
    }
}

pub fn key(seat: u32) -> SigningKey {
    SigningKey::from_bytes(&[seat as u8 + 1; 32])
}

pub fn sign(
    domain: &Domain,
    turn_id: u64,
    step: u32,
    seat: u32,
    log_hash: B256,
    action: &Action,
) -> B512 {
    let act = Act {
        turn_id,
        step,
        seat,
        log_hash,
        action,
    };
    keys::sign(&key(seat), &act.message(domain).unwrap())
}

pub fn header() -> Header {
    let domain = domain();
    Header {
        chain_id: domain.chain_id,
        escrow: domain.escrow,
        game_id: domain.game_id,
        rules: Rules {
            wolves: 2,
            sheriff: true,
            cupid: true,
        },
        seats: (0..SEATS)
            .map(|seat| Seat {
                address: Address::with_last_byte(0xa0 + seat as u8),
                session_key: keys::session_key(&key(seat)),
            })
            .collect(),
        names: NAMES.iter().map(|n| n.to_string()).collect(),
        seed: B256::repeat_byte(0x11),
        randomness: B256::repeat_byte(0x22),
        enc_key: force::public_key(&ENC_SECRET),
    }
}

pub struct Honest {
    pub transcript: Transcript,
    pub winner: Team,
    pub won: Vec<bool>,
}

/// Plays to the end. Two speakers per chat, the second signing a lagging log hash. Round 1's
/// first seat forces twice; round 2's first seat forces but signs anyway, and its last seat posts
/// an undecryptable force and times out. A force for a round never played follows the game.
pub fn honest_game() -> Honest {
    let header = header();
    let domain = header.domain();
    let rules = header.rules;
    let enc_key = header.enc_key;
    let roster_hash = encode::roster_hash(&header.seats);
    let seed = encode::deal_seed(&header.seed, &header.randomness, &roster_hash);
    let roster = (0..)
        .zip(NAMES)
        .map(|(id, name)| PublicPlayer {
            id,
            name: name.to_owned(),
            model: None,
            alive: true,
            revealed_role: None,
            human: false,
        })
        .collect();
    let setup = Setup {
        wolves: NonZeroUsize::new(rules.wolves.into()),
        sheriff: rules.sheriff,
        cupid: rules.cupid,
    };
    let (mut game, events) = Game::new(roster, setup, seed).unwrap();
    let mut log = LogChain::new(&domain);
    log.extend(&events).unwrap();
    let mut builder = TranscriptBuilder::new(header);
    let mut turn_id = 0;
    let mut rounds = 0;

    while !game.is_over() {
        if let Some(kind) = game.chat() {
            let step = builder.begin_chat(kind);
            let members: Vec<u32> = game.turns().iter().map(|t| t.player).collect();
            for (i, &seat) in members.iter().take(2).enumerate() {
                turn_id += 1;
                let hashes = log.hashes();
                let (text, log_hash) = if i == 0 {
                    (format!("Seat {seat} here, step {step}."), log.head())
                } else {
                    (
                        format!("Grüße from {seat} 🐺\nstep {step}"),
                        hashes[hashes.len().saturating_sub(2)],
                    )
                };
                let action = Action::Speak { text: text.clone() };
                let signature = sign(&domain, turn_id, step, seat, log_hash, &action);
                builder
                    .record_message(ChatMessage {
                        seat,
                        turn_id,
                        text: text.clone(),
                        log_hash,
                        signature,
                    })
                    .unwrap();
                log.extend(&game.speak(seat, &text)).unwrap();
            }
            log.extend(&game.close_chat()).unwrap();
            continue;
        }

        let (step, round) = builder.begin_round();
        rounds += 1;
        let turns = game.turns();
        let mut answers = Vec::new();
        for (i, turn) in turns.iter().enumerate() {
            turn_id += 1;
            let seat = turn.player;
            let action = policy(turn);
            let abstain = Action::Target { target: None };
            let mut force = |action: &Action, ephemeral: u8| {
                let payload =
                    force::encrypt(&enc_key, [ephemeral; 32], &domain, round, seat, action)
                        .unwrap();
                builder.record_op(OpEntry::Force {
                    seat,
                    round,
                    payload: Bytes::from(payload),
                })
            };
            let source = match (round, i) {
                (1, 0) => {
                    force(&abstain, 1);
                    let op_index = force(&action, 2);
                    AnswerSource::Forced {
                        op_index,
                        action: action.clone(),
                    }
                }
                (2, i) if i == turns.len() - 1 => {
                    builder.record_op(OpEntry::Force {
                        seat,
                        round,
                        payload: Bytes::from(vec![0xde; 64]),
                    });
                    AnswerSource::Timeout
                }
                (2, 0) => {
                    force(&abstain, 3);
                    signed(&domain, &log, turn_id, step, seat, &action)
                }
                _ => signed(&domain, &log, turn_id, step, seat, &action),
            };
            answers.push(RoundAnswer { seat, source });
        }
        builder.record_op(OpEntry::Checkpoint { round });
        let applied: Vec<Answer> = answers
            .iter()
            .map(|answer| Answer {
                player: answer.seat,
                action: match &answer.source {
                    AnswerSource::Signed { action, .. } | AnswerSource::Forced { action, .. } => {
                        Some(action.clone())
                    }
                    AnswerSource::Timeout => None,
                },
            })
            .collect();
        for answer in answers {
            builder.record_answer(answer).unwrap();
        }
        log.extend(&game.submit(&applied)).unwrap();
    }
    builder.record_op(OpEntry::Force {
        seat: 0,
        round: rounds,
        payload: Bytes::from(vec![0xab; 80]),
    });

    let transcript = builder.finish(&game).unwrap();
    Honest {
        transcript,
        winner: game.winner().unwrap(),
        won: (0..SEATS).map(|seat| game.won(seat)).collect(),
    }
}

fn signed(
    domain: &Domain,
    log: &LogChain,
    turn_id: u64,
    step: u32,
    seat: u32,
    action: &Action,
) -> AnswerSource {
    let log_hash = log.head();
    AnswerSource::Signed {
        turn_id,
        action: action.clone(),
        log_hash,
        signature: sign(domain, turn_id, step, seat, log_hash, action),
    }
}

/// Wolves kill and everyone votes for the lowest candidate; the sheriff holds fire.
fn policy(turn: &TurnSpec) -> Action {
    let target = match turn.kind {
        TurnKind::SheriffShoot => None,
        TurnKind::SeerInspect | TurnKind::DoctorProtect => turn.candidates.last().copied(),
        _ => turn.candidates.first().copied(),
    };
    Action::Target { target }
}

/// The index of the round step numbered `round`.
pub fn round_step(transcript: &Transcript, round: u32) -> usize {
    transcript
        .steps
        .iter()
        .position(|s| matches!(s, Step::Round { round: r, .. } if *r == round))
        .unwrap()
}

pub fn answers_mut(transcript: &mut Transcript, step: usize) -> &mut Vec<RoundAnswer> {
    match &mut transcript.steps[step] {
        Step::Round { answers, .. } => answers,
        Step::Chat { .. } => panic!("step {step} is a chat"),
    }
}

pub fn messages_mut(transcript: &mut Transcript, step: usize) -> &mut Vec<ChatMessage> {
    match &mut transcript.steps[step] {
        Step::Chat { messages, .. } => messages,
        Step::Round { .. } => panic!("step {step} is a round"),
    }
}
