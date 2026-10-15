mod common;

use common::{ENC_SECRET, answers_mut, domain, honest_game, messages_mut, round_step, sign};
use werewolf_protocol::{Action, TurnKind};
use werewolf_verify::{
    Decrypt, PublicValues, Transcript, TranscriptBuilder, VerifyError,
    alloy_primitives::{B256, U256},
    check_result, encode,
    keys::KeyError,
    replay::{EntryError, OpLogError, StepError},
    transcript::{AnswerSource, BuildError, OpEntry, Step},
    verify,
};

fn with_secret(transcript: &Transcript) -> Result<PublicValues, VerifyError> {
    verify(transcript, Decrypt::WithSecret(ENC_SECRET))
}

fn trusting(transcript: &Transcript) -> Result<PublicValues, VerifyError> {
    verify(transcript, Decrypt::Trust)
}

/// The entry error at `step`, or a panic describing what came back instead.
fn entry_error(result: Result<PublicValues, VerifyError>, at: usize) -> EntryError {
    match result {
        Err(VerifyError::Entry { step, reason, .. }) if step == at => reason,
        other => panic!("expected an entry error at step {at}, got {other:?}"),
    }
}

fn first_signed_answer(transcript: &Transcript) -> (usize, usize) {
    transcript
        .steps
        .iter()
        .enumerate()
        .find_map(|(step, s)| match s {
            Step::Round { answers, .. } => answers
                .iter()
                .position(|a| matches!(a.source, AnswerSource::Signed { .. }))
                .map(|entry| (step, entry)),
            Step::Chat { .. } => None,
        })
        .unwrap()
}

fn chat_steps(transcript: &Transcript, kind: TurnKind) -> Vec<usize> {
    (0..transcript.steps.len())
        .filter(|&i| matches!(&transcript.steps[i], Step::Chat { kind: k, .. } if *k == kind))
        .collect()
}

#[test]
fn honest_transcript_replays_to_the_engines_result() {
    let honest = honest_game();
    let transcript = &honest.transcript;
    let answers = || {
        transcript.steps.iter().flat_map(|s| match s {
            Step::Round { answers, .. } => answers.as_slice(),
            Step::Chat { .. } => &[],
        })
    };
    assert!(!chat_steps(transcript, TurnKind::WolfChat).is_empty());
    assert!(chat_steps(transcript, TurnKind::DaySpeak).len() >= 2);
    assert!(answers().any(|a| matches!(a.source, AnswerSource::Forced { .. })));
    assert!(answers().any(|a| matches!(a.source, AnswerSource::Timeout)));

    for values in [trusting(transcript), with_secret(transcript)] {
        let values = values.unwrap();
        check_result(transcript, &values).unwrap();
        assert_eq!(values.winnerTeam, encode::team_id(honest.winner));
        for (seat, &won) in honest.won.iter().enumerate() {
            assert_eq!(values.winners.bit(seat), won, "seat {seat}");
        }
        assert_ne!(values.winners, U256::ZERO);
        assert_eq!(values.opLogHash, encode::op_log_hash(&transcript.op_log));
        assert_eq!(
            values.transcriptRoot,
            encode::merkle_root(&transcript.leaves().unwrap())
        );
    }

    let json = serde_json::to_string(transcript).unwrap();
    assert_eq!(
        &serde_json::from_str::<Transcript>(&json).unwrap(),
        transcript
    );
}

#[test]
fn a_wrong_result_claim_is_caught_by_check_result() {
    let mut transcript = honest_game().transcript;
    transcript.result.winners ^= U256::from(1);
    let values = trusting(&transcript).unwrap();
    let err = check_result(&transcript, &values).unwrap_err();
    assert_eq!(err.field, "winners");

    transcript.result.winners ^= U256::from(1);
    transcript.result.transcript_root = B256::repeat_byte(1);
    let err = check_result(&transcript, &values).unwrap_err();
    assert_eq!(err.field, "transcript_root");
}

#[test]
fn bad_signature_fails() {
    let mut transcript = honest_game().transcript;
    let (step, entry) = first_signed_answer(&transcript);
    if let AnswerSource::Signed { signature, .. } =
        &mut answers_mut(&mut transcript, step)[entry].source
    {
        signature.0[0] ^= 1;
    }
    let err = entry_error(trusting(&transcript), step);
    assert!(
        matches!(err, EntryError::Key(KeyError::BadSignature)),
        "{err}"
    );
}

#[test]
fn a_signed_message_moved_to_another_step_fails() {
    let mut transcript = honest_game().transcript;
    let days = chat_steps(&transcript, TurnKind::DaySpeak);
    let (earlier, later) = (days[0], days[1]);
    let message = messages_mut(&mut transcript, later).remove(0);
    messages_mut(&mut transcript, earlier).push(message);
    let err = entry_error(trusting(&transcript), earlier);
    assert!(
        matches!(err, EntryError::Key(KeyError::BadSignature)),
        "{err}"
    );
}

#[test]
fn a_duplicate_seat_and_turn_fails() {
    let mut transcript = honest_game().transcript;
    let chat = chat_steps(&transcript, TurnKind::WolfChat)[0];
    let messages = messages_mut(&mut transcript, chat);
    messages.push(messages[0].clone());
    let err = entry_error(trusting(&transcript), chat);
    assert!(matches!(err, EntryError::DuplicateTurn(_)), "{err}");
}

#[test]
fn a_forged_log_hash_fails_even_when_signed() {
    let mut transcript = honest_game().transcript;
    let domain = domain();
    let (step, entry) = first_signed_answer(&transcript);
    let answer = &mut answers_mut(&mut transcript, step)[entry];
    let seat = answer.seat;
    if let AnswerSource::Signed {
        turn_id,
        action,
        log_hash,
        signature,
    } = &mut answer.source
    {
        *log_hash = B256::repeat_byte(0x99);
        *signature = sign(&domain, *turn_id, step as u32, seat, *log_hash, action);
    }
    let err = entry_error(trusting(&transcript), step);
    assert!(matches!(err, EntryError::UnknownLogHash(_)), "{err}");
}

#[test]
fn a_timeout_despite_a_valid_force_fails_with_the_secret() {
    let mut transcript = honest_game().transcript;
    let step = round_step(&transcript, 1);
    answers_mut(&mut transcript, step)[0].source = AnswerSource::Timeout;
    let err = entry_error(with_secret(&transcript), step);
    assert!(matches!(err, EntryError::IgnoredForce(_)), "{err}");
}

#[test]
fn a_forced_answer_must_match_the_last_decrypted_force() {
    let honest = honest_game().transcript;
    let step = round_step(&honest, 1);

    let mut transcript = honest.clone();
    if let AnswerSource::Forced { action, .. } = &mut answers_mut(&mut transcript, step)[0].source {
        *action = Action::Target { target: None };
    }
    let err = entry_error(with_secret(&transcript), step);
    assert!(matches!(err, EntryError::ForcedActionMismatch), "{err}");

    let mut transcript = honest;
    if let AnswerSource::Forced { op_index, .. } = &mut answers_mut(&mut transcript, step)[0].source
    {
        *op_index -= 1;
    }
    let err = entry_error(trusting(&transcript), step);
    assert!(matches!(err, EntryError::NotLastForce { .. }), "{err}");
}

#[test]
fn op_log_with_a_missing_or_misplaced_checkpoint_fails() {
    let honest = honest_game().transcript;
    let checkpoints: Vec<usize> = (0..honest.op_log.len())
        .filter(|&i| matches!(honest.op_log[i], OpEntry::Checkpoint { .. }))
        .collect();

    let mut transcript = honest.clone();
    transcript.op_log.remove(checkpoints[0]);
    let err = trusting(&transcript).unwrap_err();
    assert!(
        matches!(
            err,
            VerifyError::OpLog {
                reason: OpLogError::ForceRound { .. } | OpLogError::CheckpointOrder { .. },
                ..
            }
        ),
        "{err}"
    );

    let mut transcript = honest.clone();
    transcript.op_log.swap(checkpoints[0], checkpoints[1]);
    let err = trusting(&transcript).unwrap_err();
    assert!(matches!(err, VerifyError::OpLog { .. }), "{err}");

    // Dropping the last checkpoint, and the late force after it, leaves the last round unbacked.
    let mut transcript = honest;
    let last = *checkpoints.last().unwrap();
    transcript.op_log.truncate(last);
    let err = trusting(&transcript).unwrap_err();
    assert!(
        matches!(
            err,
            VerifyError::Step {
                reason: StepError::NoCheckpoint(_),
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn a_step_after_game_over_fails() {
    let mut transcript = honest_game().transcript;
    let extra = transcript.steps[0].clone();
    transcript.steps.push(extra);
    let err = trusting(&transcript).unwrap_err();
    assert!(
        matches!(err, VerifyError::ExtraStep(i) if i == transcript.steps.len() - 1),
        "{err}"
    );

    transcript.steps.truncate(transcript.steps.len() - 2);
    let err = trusting(&transcript).unwrap_err();
    assert!(matches!(err, VerifyError::Truncated(_)), "{err}");
}

#[test]
fn answers_out_of_turn_order_fail() {
    let mut transcript = honest_game().transcript;
    let step = round_step(&transcript, 2);
    answers_mut(&mut transcript, step).swap(0, 1);
    let err = entry_error(trusting(&transcript), step);
    assert!(matches!(err, EntryError::WrongSeat { .. }), "{err}");
}

#[test]
fn a_secret_that_does_not_match_enc_key_is_rejected() {
    let transcript = honest_game().transcript;
    let err = verify(&transcript, Decrypt::WithSecret([0x44; 32])).unwrap_err();
    assert!(matches!(err, VerifyError::EncKeyMismatch), "{err}");
}

#[test]
fn builder_rejects_entries_outside_their_step() {
    let honest = honest_game().transcript;
    let mut builder = TranscriptBuilder::new(honest.header.clone());
    let Step::Chat { messages, .. } = &honest.steps[chat_steps(&honest, TurnKind::WolfChat)[0]]
    else {
        unreachable!()
    };
    assert!(matches!(
        builder.record_message(messages[0].clone()),
        Err(BuildError::NoChat)
    ));
    builder.begin_chat(TurnKind::WolfChat);
    let answer = answers_mut(&mut honest.clone(), round_step(&honest, 0))[0].clone();
    assert!(matches!(
        builder.record_answer(answer),
        Err(BuildError::NoRound)
    ));
}
