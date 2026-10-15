//! The signed record of a running staked game: checks acts against the public log and builds the
//! transcript the replay verifies.

use std::collections::BTreeMap;

use tracing::warn;
use werewolf_engine::{Game, GameEvent};
use werewolf_protocol::{Action, PlayerId, Turn, TurnKind};
use werewolf_verify::{
    Transcript, TranscriptBuilder,
    alloy_primitives::{B256, B512, Bytes},
    encode::{self, Act, Domain, LogChain},
    force, keys,
    transcript::{AnswerSource, BuildError, ChatMessage, Header, OpEntry, RoundAnswer},
};

use super::Force;
use crate::error::HttpError;

/// An act whose signature checked out: enough to record it and to sign its receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signed {
    pub turn_id: u64,
    pub log_hash: B256,
    pub signature: B512,
    pub act_hash: B256,
    /// `receiptDigest`, the Merkle leaf.
    pub digest: B256,
}

pub struct Ledger {
    domain: Domain,
    /// Registered session keys, by seat.
    session_keys: Vec<B256>,
    log: LogChain,
    builder: TranscriptBuilder,
}

impl Ledger {
    /// Starts the record with the deal's opening `events`.
    pub fn new(header: Header, events: &[GameEvent]) -> Result<Self, serde_json::Error> {
        let domain = header.domain();
        let mut log = LogChain::new(&domain);
        log.extend(events)?;
        Ok(Self {
            domain,
            session_keys: header.seats.iter().map(|s| s.session_key).collect(),
            log,
            builder: TranscriptBuilder::new(header),
        })
    }

    /// Follows the engine's public log; call with every batch of engine events, in order.
    pub fn extend(&mut self, events: &[GameEvent]) -> Result<(), serde_json::Error> {
        self.log.extend(events)
    }

    pub fn begin_chat(&mut self, kind: TurnKind) -> u32 {
        self.builder.begin_chat(kind)
    }

    pub fn begin_round(&mut self) -> (u32, u32) {
        self.builder.begin_round()
    }

    pub fn record_message(
        &mut self,
        seat: u32,
        text: &str,
        signed: &Signed,
    ) -> Result<(), BuildError> {
        self.builder.record_message(ChatMessage {
            seat,
            turn_id: signed.turn_id,
            text: text.to_owned(),
            log_hash: signed.log_hash,
            signature: signed.signature,
        })
    }

    /// Records round `round`: its forced moves and checkpoint in on-chain order, then one answer
    /// per turn, in turn order. A signed answer wins; else the seat's last force if it decrypts;
    /// else a timeout. Returns the actions for the engine.
    pub fn record_round(
        &mut self,
        round: u32,
        forces: &[Force],
        answers: &[(PlayerId, Option<(Action, Signed)>)],
        enc_secret: &[u8; 32],
    ) -> Result<Vec<Option<Action>>, BuildError> {
        let mut last: BTreeMap<u32, (u32, &Bytes)> = BTreeMap::new();
        for force in forces {
            let op_index = self.builder.record_op(OpEntry::Force {
                seat: force.seat,
                round,
                payload: force.payload.clone(),
            });
            last.insert(force.seat, (op_index, &force.payload));
        }
        self.builder.record_op(OpEntry::Checkpoint { round });
        let mut applied = Vec::with_capacity(answers.len());
        for (seat, answer) in answers {
            let source = match answer {
                Some((action, signed)) => AnswerSource::Signed {
                    turn_id: signed.turn_id,
                    action: action.clone(),
                    log_hash: signed.log_hash,
                    signature: signed.signature,
                },
                None => match last.get(seat) {
                    Some(&(op_index, payload)) => {
                        match force::decrypt(enc_secret, &self.domain, round, *seat, payload) {
                            Ok(action) => AnswerSource::Forced { op_index, action },
                            Err(err) => {
                                warn!(game = %self.domain.game_id, seat, round, %err, "forced move does not decrypt; it counts as a timeout");
                                AnswerSource::Timeout
                            }
                        }
                    }
                    None => {
                        warn!(game = %self.domain.game_id, seat, round, "turn timed out with no forced move; applying default action");
                        AnswerSource::Timeout
                    }
                },
            };
            applied.push(match &source {
                AnswerSource::Signed { action, .. } | AnswerSource::Forced { action, .. } => {
                    Some(action.clone())
                }
                AnswerSource::Timeout => None,
            });
            self.builder.record_answer(RoundAnswer {
                seat: *seat,
                source,
            })?;
        }
        Ok(applied)
    }

    pub fn finish(self, game: &Game) -> Result<Transcript, BuildError> {
        self.builder.finish(game)
    }

    /// Checks `seat`'s signature over `action` for `turn`, and that it signed a public log prefix.
    pub fn check(
        &self,
        turn: &Turn,
        seat: u32,
        action: &Action,
        log_hash: Option<&str>,
        signature: Option<&str>,
    ) -> Result<Signed, HttpError> {
        let (Some(log_hash), Some(signature)) = (log_hash, signature) else {
            return Err(HttpError::invalid(
                "staked games need `log_hash` and `signature` on every act",
            ));
        };
        let log_hash: B256 = log_hash
            .parse()
            .map_err(|_| HttpError::invalid("log_hash must be 32 bytes of 0x-hex"))?;
        let signature: B512 = signature
            .parse()
            .map_err(|_| HttpError::invalid("signature must be 64 bytes of 0x-hex"))?;
        let act = Act {
            turn_id: turn.turn_id,
            step: turn.step,
            seat,
            log_hash,
            action,
        };
        let message = act.message(&self.domain).map_err(|_| HttpError::Internal)?;
        let session_key = self
            .session_keys
            .get(seat as usize)
            .ok_or(HttpError::Internal)?;
        keys::verify(session_key, &message, &signature).map_err(|_| {
            HttpError::invalid(
                "signature does not verify under your session key for this turn_id, step, seat, \
                 log_hash and action",
            )
        })?;
        if !self.log.contains(&log_hash) {
            return Err(HttpError::invalid(
                "log_hash is not a prefix hash of the public log",
            ));
        }
        let act_hash = encode::act_hash(&message);
        Ok(Signed {
            turn_id: turn.turn_id,
            log_hash,
            signature,
            act_hash,
            digest: encode::receipt_digest(&self.domain, turn.turn_id, seat, act_hash),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use werewolf_protocol::{Phase, PlayerView, PublicEvent, Role};
    use werewolf_verify::{
        alloy_primitives::{Address, U256},
        keys::SigningKey,
        transcript::{Rules, Seat},
    };

    use super::*;

    pub fn key(seat: u32) -> SigningKey {
        SigningKey::from_bytes(&[seat as u8 + 1; 32])
    }

    pub fn ledger() -> Ledger {
        let header = Header {
            chain_id: 31337,
            escrow: Address::with_last_byte(9),
            game_id: U256::from(1),
            rules: Rules {
                wolves: 1,
                sheriff: false,
                cupid: false,
            },
            seats: (0..5)
                .map(|seat| Seat {
                    address: Address::with_last_byte(seat as u8),
                    session_key: keys::session_key(&key(seat)),
                })
                .collect(),
            names: (0..5).map(|seat| format!("Seat {seat}")).collect(),
            seed: B256::ZERO,
            randomness: B256::ZERO,
            enc_key: B256::ZERO,
        };
        let opening = GameEvent::Public(PublicEvent::PhaseStarted {
            phase: Phase::Night { n: 1 },
        });
        Ledger::new(header, &[opening]).unwrap()
    }

    pub fn turn(turn_id: u64, step: u32, round: Option<u32>, kind: TurnKind) -> Turn {
        Turn {
            turn_id,
            step,
            round,
            kind,
            candidates: vec![0, 1, 2, 3],
            deadline_ms: 1_000,
            view: PlayerView {
                you: 4,
                role: Role::Villager,
                phase: Phase::Night { n: 1 },
                players: Vec::new(),
                wolf_team: Vec::new(),
                public_log: Vec::new(),
                private_log: Vec::new(),
            },
        }
    }

    /// `seat`'s hex signature and the log head it covers.
    pub fn sign(
        ledger: &Ledger,
        turn: &Turn,
        step: u32,
        seat: u32,
        action: &Action,
    ) -> (String, String) {
        let log_hash = ledger.log.head();
        let act = Act {
            turn_id: turn.turn_id,
            step,
            seat,
            log_hash,
            action,
        };
        let signature = keys::sign(&key(seat), &act.message(&ledger.domain).unwrap());
        (log_hash.to_string(), signature.to_string())
    }

    fn rejection(result: Result<Signed, HttpError>) -> String {
        match result {
            Err(HttpError::Invalid(message)) => message,
            other => panic!("expected a 422, got {other:?}"),
        }
    }

    #[test]
    fn signed_answers_beat_forces_and_bad_forces_time_out() {
        let mut ledger = ledger();
        let secret = [5; 32];
        let enc_key = force::public_key(&secret);
        let domain = ledger.domain;
        let vote = |target| Action::Target {
            target: Some(target),
        };
        let seal = |seat, action: &Action, ephemeral| Force {
            seat,
            payload: force::encrypt(&enc_key, [ephemeral; 32], &domain, 0, seat, action)
                .unwrap()
                .into(),
        };
        let turn = turn(1, 0, Some(0), TurnKind::DayVote);
        let (step, round) = ledger.begin_round();
        assert_eq!((step, round), (0, 0));
        let (log_hash, signature) = sign(&ledger, &turn, step, 0, &vote(1));
        let signed = ledger
            .check(&turn, 0, &vote(1), Some(&log_hash), Some(&signature))
            .unwrap();
        let forces = [
            seal(0, &vote(2), 1),
            seal(1, &vote(0), 2),
            seal(1, &vote(3), 3),
            Force {
                seat: 2,
                payload: Bytes::from(vec![0xde; 64]),
            },
        ];
        let answers = [
            (0, Some((vote(1), signed))),
            (1, None),
            (2, None),
            (3, None),
        ];
        let applied = ledger.record_round(0, &forces, &answers, &secret).unwrap();
        assert_eq!(applied, [Some(vote(1)), Some(vote(3)), None, None]);
    }

    #[test]
    fn checks_signature_step_and_log_hash() {
        let ledger = ledger();
        let turn = turn(7, 2, Some(1), TurnKind::DayVote);
        let action = Action::Target { target: Some(1) };
        let (log_hash, signature) = sign(&ledger, &turn, 2, 4, &action);
        let signed = ledger
            .check(&turn, 4, &action, Some(&log_hash), Some(&signature))
            .unwrap();
        assert_eq!(signed.turn_id, 7);
        let message = Act {
            turn_id: 7,
            step: 2,
            seat: 4,
            log_hash: signed.log_hash,
            action: &action,
        }
        .message(&ledger.domain)
        .unwrap();
        assert_eq!(signed.act_hash, encode::act_hash(&message));

        let check = |seat, action: &Action, log_hash: &str, signature: &str| {
            rejection(ledger.check(&turn, seat, action, Some(log_hash), Some(signature)))
        };
        let other = Action::Target { target: Some(2) };
        assert!(check(4, &other, &log_hash, &signature).contains("does not verify"));
        assert!(check(3, &action, &log_hash, &signature).contains("does not verify"));
        let (_, wrong_step) = sign(&ledger, &turn, 3, 4, &action);
        assert!(check(4, &action, &log_hash, &wrong_step).contains("does not verify"));

        // A validly signed hash that is not on the log.
        let unknown = B256::repeat_byte(0x42);
        let act = Act {
            turn_id: 7,
            step: 2,
            seat: 4,
            log_hash: unknown,
            action: &action,
        };
        let signature = keys::sign(&key(4), &act.message(&ledger.domain).unwrap());
        assert!(
            check(4, &action, &unknown.to_string(), &signature.to_string())
                .contains("not a prefix")
        );

        assert!(
            rejection(ledger.check(&turn, 4, &action, None, Some("0x00"))).contains("log_hash")
        );
        assert!(check(4, &action, "0x1234", "0x00").contains("32 bytes"));
    }
}
