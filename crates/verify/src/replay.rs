//! Replays a transcript through the engine and derives the `PublicValues` the contract checks.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
};

use alloy_primitives::{B256, B512, keccak256};
use ed25519_dalek::VerifyingKey;
use werewolf_engine::{Answer, EngineError, Game, Setup};
use werewolf_protocol::{Action, MAX_TEXT_LEN, PlayerId, PublicPlayer, TurnKind};

use crate::{
    encode::{self, Act, Domain, LogChain, PublicValues},
    force::{self, ForceError},
    keys::{self, KeyError},
    transcript::{AnswerSource, ChatMessage, OpEntry, Step, TRANSCRIPT_VERSION, Transcript},
};

/// The contract's `maxSeats` is a `uint8`.
pub const MAX_SEATS: usize = u8::MAX as usize;

/// How to treat forced moves, which only the operator can decrypt.
pub enum Decrypt {
    /// Accept the transcript's forced actions and timeouts as decrypted (players).
    Trust,
    /// Decrypt every force with the operator's X25519 secret and compare (operator, prover).
    WithSecret([u8; 32]),
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("unsupported transcript version {0}, expected {TRANSCRIPT_VERSION}")]
    Version(u32),
    #[error("{names} names for {seats} seats")]
    Names { seats: usize, names: usize },
    #[error("{0} seats, more than the contract's {MAX_SEATS}")]
    TooManySeats(usize),
    #[error("rules fix no wolves; staked games need a fixed count")]
    NoWolves,
    #[error("enc_key is not the public key of the given secret")]
    EncKeyMismatch,
    #[error("cannot deal: {0}")]
    Deal(#[from] EngineError),
    #[error("op_log[{index}]: {reason}")]
    OpLog { index: usize, reason: OpLogError },
    #[error("step {step}: {reason}")]
    Step { step: usize, reason: StepError },
    #[error("step {step}, entry {entry} (seat {seat}): {reason}")]
    Entry {
        step: usize,
        entry: usize,
        seat: u32,
        reason: EntryError,
    },
    #[error("transcript ends after {0} steps, before the game is over")]
    Truncated(usize),
    #[error("step {0} comes after the game is over")]
    ExtraStep(usize),
    #[error("op_log checkpoints {checkpoints} rounds, but {played} were played")]
    Checkpoints { checkpoints: u32, played: u32 },
    #[error("encoding: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpLogError {
    #[error("checkpoint for round {found}, expected round {expected}")]
    CheckpointOrder { expected: u32, found: u32 },
    #[error("force for round {found} while round {expected} is open")]
    ForceRound { expected: u32, found: u32 },
    #[error("force from seat {0}, which does not exist")]
    UnknownSeat(u32),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StepError {
    #[error("expected a {0:?} chat step")]
    ExpectedChat(TurnKind),
    #[error("expected a round step, found a chat step")]
    ExpectedRound,
    #[error("round {found}, expected round {expected}")]
    RoundNumber { expected: u32, found: u32 },
    #[error("op_log has no checkpoint for round {0}")]
    NoCheckpoint(u32),
    #[error("{found} answers for {expected} turns")]
    AnswerCount { expected: usize, found: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum EntryError {
    #[error("answer belongs to seat {expected}'s turn")]
    WrongSeat { expected: PlayerId },
    #[error("seat may not speak in this chat")]
    NotInChat,
    #[error("text {0}")]
    BadText(&'static str),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("log_hash {0} is not a prefix of the public log")]
    UnknownLogHash(B256),
    #[error("turn {0} already has a signed entry from this seat")]
    DuplicateTurn(u64),
    #[error("op_index {0}, but the seat has no force for this round")]
    NoForce(u32),
    #[error("op_index {found} is not the seat's last force this round, {last} is")]
    NotLastForce { found: u32, last: usize },
    #[error("forced payload does not decrypt: {0}")]
    Undecryptable(ForceError),
    #[error("forced action differs from the decrypted payload")]
    ForcedActionMismatch,
    #[error("timed out despite a valid force at op_log[{0}]")]
    IgnoredForce(usize),
    #[error("encoding the action: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Applies every replay rule in `docs/SETTLEMENT.md` and returns the values `prove` checks.
/// Ignores `transcript.result`; see [`check_result`].
pub fn verify(transcript: &Transcript, decrypt: Decrypt) -> Result<PublicValues, VerifyError> {
    if transcript.version != TRANSCRIPT_VERSION {
        return Err(VerifyError::Version(transcript.version));
    }
    let header = &transcript.header;
    let seats = header.seats.len();
    if header.names.len() != seats {
        return Err(VerifyError::Names {
            seats,
            names: header.names.len(),
        });
    }
    if seats > MAX_SEATS {
        return Err(VerifyError::TooManySeats(seats));
    }
    let wolves = NonZeroUsize::new(header.rules.wolves.into()).ok_or(VerifyError::NoWolves)?;
    let secret = match decrypt {
        Decrypt::Trust => None,
        Decrypt::WithSecret(secret) if force::public_key(&secret) == header.enc_key => Some(secret),
        Decrypt::WithSecret(_) => return Err(VerifyError::EncKeyMismatch),
    };
    let ops = OpLog::check(&transcript.op_log, seats)?;

    let domain = header.domain();
    let roster_hash = encode::roster_hash(&header.seats);
    let roster = (0..)
        .zip(&header.names)
        .map(|(id, name)| PublicPlayer {
            id,
            name: name.clone(),
            model: None,
            alive: true,
            revealed_role: None,
            human: false,
        })
        .collect();
    let setup = Setup {
        wolves: Some(wolves),
        sheriff: header.rules.sheriff,
        cupid: header.rules.cupid,
    };
    let seed = encode::deal_seed(&header.seed, &header.randomness, &roster_hash);
    let (game, events) = Game::new(roster, setup, seed)?;
    let mut log = LogChain::new(&domain);
    log.extend(&events)?;

    let mut replay = Replay {
        domain,
        game,
        log,
        // A seat that never joined over HTTP may have registered junk; only its signatures fail.
        keys: header
            .seats
            .iter()
            .map(|s| keys::parse(&s.session_key))
            .collect(),
        seen: BTreeSet::new(),
        leaves: Vec::new(),
        ops: &ops,
        secret,
        rounds: 0,
    };
    for (index, step) in transcript.steps.iter().enumerate() {
        if replay.game.winner().is_some() {
            return Err(VerifyError::ExtraStep(index));
        }
        replay.step(index, step)?;
    }
    let Some(winner) = replay.game.winner() else {
        return Err(VerifyError::Truncated(transcript.steps.len()));
    };
    if ops.checkpoints != replay.rounds {
        return Err(VerifyError::Checkpoints {
            checkpoints: ops.checkpoints,
            played: replay.rounds,
        });
    }

    Ok(PublicValues {
        chainId: domain.chain_id,
        escrow: domain.escrow,
        gameId: domain.game_id,
        rulesHash: encode::rules_hash(&header.rules),
        rosterHash: roster_hash,
        seedCommit: keccak256(header.seed),
        randomness: header.randomness,
        encKey: header.enc_key,
        opLogHash: ops.hash,
        winnerTeam: encode::team_id(winner),
        winners: encode::winners(&replay.game, seats),
        transcriptRoot: encode::merkle_root(&replay.leaves),
    })
}

#[derive(Debug, thiserror::Error)]
#[error("transcript claims {field} {claimed}, replay gives {computed}")]
pub struct ResultMismatch {
    pub field: &'static str,
    pub claimed: String,
    pub computed: String,
}

/// Checks the transcript's claimed `result` against values from [`verify`].
pub fn check_result(transcript: &Transcript, values: &PublicValues) -> Result<(), ResultMismatch> {
    let claimed = &transcript.result;
    let fields = [
        (
            "winner_team",
            claimed.winner_team.to_string(),
            values.winnerTeam.to_string(),
        ),
        (
            "winners",
            format!("{:#x}", claimed.winners),
            format!("{:#x}", values.winners),
        ),
        (
            "transcript_root",
            claimed.transcript_root.to_string(),
            values.transcriptRoot.to_string(),
        ),
    ];
    match fields
        .into_iter()
        .find(|(_, claimed, computed)| claimed != computed)
    {
        Some((field, claimed, computed)) => Err(ResultMismatch {
            field,
            claimed,
            computed,
        }),
        None => Ok(()),
    }
}

/// The op log's hash and the facts replay needs from it, after structural checks.
struct OpLog<'a> {
    hash: B256,
    checkpoints: u32,
    /// Each seat's last force by `(seat, round)`: its index and payload.
    last_force: BTreeMap<(u32, u32), (usize, &'a [u8])>,
}

impl<'a> OpLog<'a> {
    /// Checks the order the contract enforces: checkpoints go 0, 1, 2, and a force targets the
    /// round not yet checkpointed.
    fn check(entries: &'a [OpEntry], seats: usize) -> Result<Self, VerifyError> {
        let mut checkpoints = 0;
        let mut last_force = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            let fail = |reason| VerifyError::OpLog { index, reason };
            match entry {
                &OpEntry::Checkpoint { round } if round != checkpoints => {
                    return Err(fail(OpLogError::CheckpointOrder {
                        expected: checkpoints,
                        found: round,
                    }));
                }
                OpEntry::Checkpoint { .. } => checkpoints += 1,
                &OpEntry::Force { round, .. } if round != checkpoints => {
                    return Err(fail(OpLogError::ForceRound {
                        expected: checkpoints,
                        found: round,
                    }));
                }
                &OpEntry::Force { seat, .. } if seat as usize >= seats => {
                    return Err(fail(OpLogError::UnknownSeat(seat)));
                }
                OpEntry::Force {
                    seat,
                    round,
                    payload,
                } => {
                    last_force.insert((*seat, *round), (index, &payload[..]));
                }
            }
        }
        Ok(Self {
            hash: encode::op_log_hash(entries),
            checkpoints,
            last_force,
        })
    }
}

struct Replay<'a> {
    domain: Domain,
    game: Game,
    log: LogChain,
    keys: Vec<Result<VerifyingKey, KeyError>>,
    /// `(seat, turn_id)` of every signed entry so far.
    seen: BTreeSet<(u32, u64)>,
    leaves: Vec<B256>,
    ops: &'a OpLog<'a>,
    secret: Option<[u8; 32]>,
    rounds: u32,
}

impl Replay<'_> {
    fn step(&mut self, index: usize, step: &Step) -> Result<(), VerifyError> {
        let fail = |reason| VerifyError::Step {
            step: index,
            reason,
        };
        // Games end within a few dozen steps; anything past the end fails as `ExtraStep`.
        let signed_step = index as u32;
        match (self.game.chat(), step) {
            (
                Some(kind),
                Step::Chat {
                    kind: found,
                    messages,
                },
            ) if *found == kind => self.chat(index, signed_step, messages),
            (Some(kind), _) => Err(fail(StepError::ExpectedChat(kind))),
            (None, Step::Chat { .. }) => Err(fail(StepError::ExpectedRound)),
            (None, Step::Round { round, answers }) => {
                if *round != self.rounds {
                    return Err(fail(StepError::RoundNumber {
                        expected: self.rounds,
                        found: *round,
                    }));
                }
                if *round >= self.ops.checkpoints {
                    return Err(fail(StepError::NoCheckpoint(*round)));
                }
                let turns = self.game.turns();
                if answers.len() != turns.len() {
                    return Err(fail(StepError::AnswerCount {
                        expected: turns.len(),
                        found: answers.len(),
                    }));
                }
                let mut applied = Vec::with_capacity(turns.len());
                for (entry, (answer, turn)) in answers.iter().zip(&turns).enumerate() {
                    let seat = answer.seat;
                    let action = self
                        .answer(signed_step, *round, turn.player, seat, &answer.source)
                        .map_err(|reason| VerifyError::Entry {
                            step: index,
                            entry,
                            seat,
                            reason,
                        })?;
                    applied.push(Answer {
                        player: seat,
                        action,
                    });
                }
                let events = self.game.submit(&applied);
                self.log.extend(&events)?;
                self.rounds += 1;
                Ok(())
            }
        }
    }

    fn chat(
        &mut self,
        index: usize,
        step: u32,
        messages: &[ChatMessage],
    ) -> Result<(), VerifyError> {
        let members: Vec<PlayerId> = self.game.turns().iter().map(|t| t.player).collect();
        for (entry, message) in messages.iter().enumerate() {
            let action = Action::Speak {
                text: message.text.clone(),
            };
            let checked = if members.contains(&message.seat) {
                check_text(&message.text).and_then(|()| {
                    self.signed(
                        step,
                        message.seat,
                        message.turn_id,
                        message.log_hash,
                        &message.signature,
                        &action,
                    )
                })
            } else {
                Err(EntryError::NotInChat)
            };
            checked.map_err(|reason| VerifyError::Entry {
                step: index,
                entry,
                seat: message.seat,
                reason,
            })?;
            let events = self.game.speak(message.seat, &message.text);
            self.log.extend(&events)?;
        }
        let events = self.game.close_chat();
        self.log.extend(&events)?;
        Ok(())
    }

    /// Checks one round answer and returns the action the engine applies.
    fn answer(
        &mut self,
        step: u32,
        round: u32,
        expected: PlayerId,
        seat: u32,
        source: &AnswerSource,
    ) -> Result<Option<Action>, EntryError> {
        if seat != expected {
            return Err(EntryError::WrongSeat { expected });
        }
        match source {
            AnswerSource::Signed {
                turn_id,
                action,
                log_hash,
                signature,
            } => {
                self.signed(step, seat, *turn_id, *log_hash, signature, action)?;
                Ok(Some(action.clone()))
            }
            AnswerSource::Forced { op_index, action } => {
                self.forced(round, seat, *op_index, action)?;
                Ok(Some(action.clone()))
            }
            AnswerSource::Timeout => {
                self.timeout(round, seat)?;
                Ok(None)
            }
        }
    }

    /// Checks a signed entry and records its Merkle leaf.
    fn signed(
        &mut self,
        step: u32,
        seat: u32,
        turn_id: u64,
        log_hash: B256,
        signature: &B512,
        action: &Action,
    ) -> Result<(), EntryError> {
        let act = Act {
            turn_id,
            step,
            seat,
            log_hash,
            action,
        };
        let message = act.message(&self.domain)?;
        let key = self
            .keys
            .get(seat as usize)
            .ok_or(KeyError::InvalidKey)?
            .as_ref()
            .map_err(|err| *err)?;
        keys::verify_with(key, &message, signature)?;
        if !self.log.contains(&log_hash) {
            return Err(EntryError::UnknownLogHash(log_hash));
        }
        if !self.seen.insert((seat, turn_id)) {
            return Err(EntryError::DuplicateTurn(turn_id));
        }
        let act_hash = encode::act_hash(&message);
        self.leaves.push(encode::receipt_digest(
            &self.domain,
            turn_id,
            seat,
            act_hash,
        ));
        Ok(())
    }

    fn forced(
        &self,
        round: u32,
        seat: u32,
        op_index: u32,
        action: &Action,
    ) -> Result<(), EntryError> {
        match self.ops.last_force.get(&(seat, round)) {
            Some(&(last, payload)) if last == op_index as usize => {
                if let Some(secret) = &self.secret {
                    let decrypted = force::decrypt(secret, &self.domain, round, seat, payload)
                        .map_err(EntryError::Undecryptable)?;
                    if decrypted != *action {
                        return Err(EntryError::ForcedActionMismatch);
                    }
                }
                Ok(())
            }
            Some(&(last, _)) => Err(EntryError::NotLastForce {
                found: op_index,
                last,
            }),
            None => Err(EntryError::NoForce(op_index)),
        }
    }

    /// A timeout is only valid if the seat's last force, if any, does not decrypt. Without the
    /// secret that can't be checked, so it is trusted like a forced action.
    fn timeout(&self, round: u32, seat: u32) -> Result<(), EntryError> {
        let (Some(secret), Some(&(index, payload))) =
            (&self.secret, self.ops.last_force.get(&(seat, round)))
        else {
            return Ok(());
        };
        match force::decrypt(secret, &self.domain, round, seat, payload) {
            Ok(_) => Err(EntryError::IgnoredForce(index)),
            // An undecryptable force is ignored, so the turn really did time out.
            Err(_) => Ok(()),
        }
    }
}

fn check_text(text: &str) -> Result<(), EntryError> {
    if text.trim().is_empty() {
        return Err(EntryError::BadText("is blank"));
    }
    if text.chars().count() > MAX_TEXT_LEN {
        return Err(EntryError::BadText("is too long"));
    }
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(EntryError::BadText("has control characters other than \\n"));
    }
    Ok(())
}
