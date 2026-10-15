//! The published transcript of a staked game, and the builder the server fills while it runs.

use alloy_primitives::{Address, B256, B512, Bytes, U256};
use serde::{Deserialize, Serialize};
use werewolf_engine::Game;
use werewolf_protocol::{Action, TurnKind};

use crate::encode::{self, Act, Domain};

/// The only transcript format so far.
pub const TRANSCRIPT_VERSION: u32 = 1;

/// A finished staked game, as served by `GET /v1/transcript`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub version: u32,
    #[serde(flatten)]
    pub header: Header,
    pub op_log: Vec<OpEntry>,
    pub steps: Vec<Step>,
    /// The operator's claim; [`crate::verify`] recomputes it and never reads this.
    pub result: GameResult,
}

/// Everything fixed before play starts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub chain_id: u64,
    pub escrow: Address,
    #[serde(with = "decimal")]
    pub game_id: U256,
    pub rules: Rules,
    /// Indexed by seat, in on-chain join order.
    pub seats: Vec<Seat>,
    /// Display names by seat; they don't affect play.
    pub names: Vec<String>,
    pub seed: B256,
    pub randomness: B256,
    /// Operator's X25519 public key for forced moves.
    pub enc_key: B256,
}

impl Header {
    pub fn domain(&self) -> Domain {
        Domain {
            chain_id: self.chain_id,
            escrow: self.escrow,
            game_id: self.game_id,
        }
    }
}

/// The `rulesHash` preimage.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    pub wolves: u8,
    pub sheriff: bool,
    pub cupid: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seat {
    pub address: Address,
    /// Ed25519 public key registered in `join`.
    pub session_key: B256,
}

/// One entry of the contract's op log, in on-chain order.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpEntry {
    Force {
        seat: u32,
        round: u32,
        payload: Bytes,
    },
    Checkpoint {
        round: u32,
    },
}

/// A chat window or a target round; its index in [`Transcript::steps`] is the signed `step`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    Chat {
        kind: TurnKind,
        messages: Vec<ChatMessage>,
    },
    Round {
        round: u32,
        /// One per [`Game::turns`], in that order.
        answers: Vec<RoundAnswer>,
    },
}

/// A posted chat message; its action is `speak` with `text`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub seat: u32,
    pub turn_id: u64,
    pub text: String,
    pub log_hash: B256,
    pub signature: B512,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoundAnswer {
    pub seat: u32,
    #[serde(flatten)]
    pub source: AnswerSource,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum AnswerSource {
    Signed {
        turn_id: u64,
        action: Action,
        log_hash: B256,
        signature: B512,
    },
    /// Taken from the [`OpEntry::Force`] at `op_index`.
    Forced {
        op_index: u32,
        action: Action,
    },
    Timeout,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameResult {
    pub winner_team: u8,
    /// Bit `i` is set when seat `i` won.
    pub winners: U256,
    pub transcript_root: B256,
}

impl Transcript {
    /// Merkle leaves: the receipt digest of every signed entry, in transcript order.
    pub fn leaves(&self) -> Result<Vec<B256>, serde_json::Error> {
        let domain = self.header.domain();
        let mut leaves = Vec::new();
        for (step, entry) in (0..).zip(&self.steps) {
            match entry {
                Step::Chat { messages, .. } => {
                    for message in messages {
                        let action = Action::Speak {
                            text: message.text.clone(),
                        };
                        let act = Act {
                            turn_id: message.turn_id,
                            step,
                            seat: message.seat,
                            log_hash: message.log_hash,
                            action: &action,
                        };
                        leaves.push(act.receipt_digest(&domain)?);
                    }
                }
                Step::Round { answers, .. } => {
                    for answer in answers {
                        if let AnswerSource::Signed {
                            turn_id,
                            action,
                            log_hash,
                            ..
                        } = &answer.source
                        {
                            let act = Act {
                                turn_id: *turn_id,
                                step,
                                seat: answer.seat,
                                log_hash: *log_hash,
                                action,
                            };
                            leaves.push(act.receipt_digest(&domain)?);
                        }
                    }
                }
            }
        }
        Ok(leaves)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("no chat step is open")]
    NoChat,
    #[error("no round is open")]
    NoRound,
    #[error("the game is not over")]
    NotOver,
    #[error("encoding an action: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Records a staked game as it runs. Steps must follow the engine: [`Self::begin_chat`] when
/// [`Game::chat`] is some, else [`Self::begin_round`] with answers in [`Game::turns`] order.
#[derive(Clone, Debug)]
pub struct TranscriptBuilder {
    header: Header,
    op_log: Vec<OpEntry>,
    steps: Vec<Step>,
    next_step: u32,
    next_round: u32,
}

impl TranscriptBuilder {
    pub fn new(header: Header) -> Self {
        Self {
            header,
            op_log: Vec::new(),
            steps: Vec::new(),
            next_step: 0,
            next_round: 0,
        }
    }

    /// Opens a chat window and returns its step.
    pub fn begin_chat(&mut self, kind: TurnKind) -> u32 {
        self.steps.push(Step::Chat {
            kind,
            messages: Vec::new(),
        });
        self.advance_step()
    }

    /// Appends a posted message to the open chat window.
    pub fn record_message(&mut self, message: ChatMessage) -> Result<(), BuildError> {
        match self.steps.last_mut() {
            Some(Step::Chat { messages, .. }) => {
                messages.push(message);
                Ok(())
            }
            _ => Err(BuildError::NoChat),
        }
    }

    /// Opens the next target round and returns its `(step, round)`.
    pub fn begin_round(&mut self) -> (u32, u32) {
        let round = self.next_round;
        self.next_round += 1;
        self.steps.push(Step::Round {
            round,
            answers: Vec::new(),
        });
        (self.advance_step(), round)
    }

    /// Appends the next answer, in [`Game::turns`] order, to the open round.
    pub fn record_answer(&mut self, answer: RoundAnswer) -> Result<(), BuildError> {
        match self.steps.last_mut() {
            Some(Step::Round { answers, .. }) => {
                answers.push(answer);
                Ok(())
            }
            _ => Err(BuildError::NoRound),
        }
    }

    /// Appends an op log entry as the contract logged it; returns its `op_index`.
    pub fn record_op(&mut self, entry: OpEntry) -> u32 {
        self.op_log.push(entry);
        // The contract's op log grows by one transaction per entry, far below `u32::MAX`.
        (self.op_log.len() - 1) as u32
    }

    /// Seals the transcript with the result of the finished `game`.
    pub fn finish(self, game: &Game) -> Result<Transcript, BuildError> {
        let winner = game.winner().ok_or(BuildError::NotOver)?;
        let winners = encode::winners(game, self.header.seats.len());
        let mut transcript = Transcript {
            version: TRANSCRIPT_VERSION,
            header: self.header,
            op_log: self.op_log,
            steps: self.steps,
            result: GameResult {
                winner_team: encode::team_id(winner),
                winners,
                transcript_root: B256::ZERO,
            },
        };
        transcript.result.transcript_root = encode::merkle_root(&transcript.leaves()?);
        Ok(transcript)
    }

    fn advance_step(&mut self) -> u32 {
        let step = self.next_step;
        self.next_step += 1;
        step
    }
}

/// `U256` as a decimal string, the way the spec writes `game_id`.
mod decimal {
    use alloy_primitives::U256;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &U256, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<U256, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(D::Error::custom("expected a decimal string"));
        }
        U256::from_str_radix(&text, 10).map_err(D::Error::custom)
    }
}
