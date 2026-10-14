//! Turns a model's free-form reply into a validated action.

use serde_json::{Map, Value};
use werewolf_protocol::{Action, MAX_TEXT_LEN, PlayerId, Turn, TurnKind};

/// Backstop for the prompt's 25-word limit, so spectators get a glanceable thought.
pub const MAX_REASONING_LEN: usize = 200;

/// What the agent sends for one turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    /// Shown to spectators only.
    pub reasoning: Option<String>,
}

impl Decision {
    /// Silence or abstain: the same thing the server does when a turn times out.
    pub fn fallback(kind: TurnKind) -> Self {
        let action = if is_speak(kind) {
            Action::Speak {
                text: String::new(),
            }
        } else {
            Action::Target { target: None }
        };
        Self {
            action,
            reasoning: None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReplyError {
    #[error("no JSON object with a \"{0}\" field was found")]
    NoJson(&'static str),
    #[error("\"say\" must be a string")]
    BadSay,
    #[error("\"target\" must be a player id number or null, got {0}")]
    BadTarget(String),
    #[error("{0} is not one of the candidate ids")]
    NotCandidate(PlayerId),
}

pub const fn is_speak(kind: TurnKind) -> bool {
    matches!(kind, TurnKind::DaySpeak | TurnKind::WolfChat)
}

/// Parses `text` as the reply to `turn`, rejecting targets outside `turn.candidates`.
pub fn parse(text: &str, turn: &Turn) -> Result<Decision, ReplyError> {
    let key = if is_speak(turn.kind) { "say" } else { "target" };
    let object = extract_object(text, key).ok_or(ReplyError::NoJson(key))?;
    let reasoning = object
        .get("reasoning")
        .and_then(Value::as_str)
        .and_then(|r| clean_text(r, MAX_REASONING_LEN));

    let action = if is_speak(turn.kind) {
        let say = object
            .get("say")
            .and_then(Value::as_str)
            .ok_or(ReplyError::BadSay)?;
        Action::Speak {
            text: clean_text(say, MAX_TEXT_LEN).unwrap_or_default(),
        }
    } else {
        let target = parse_target(object.get(key).ok_or(ReplyError::NoJson(key))?)?;
        if let Some(id) = target
            && !turn.candidates.contains(&id)
        {
            return Err(ReplyError::NotCandidate(id));
        }
        Action::Target { target }
    };
    Ok(Decision { action, reasoning })
}

/// Finds the first balanced `{...}` in `text` that parses as a JSON object containing `key`.
///
/// Tolerates code fences, prose around the object, and stray braces in that prose.
pub fn extract_object(text: &str, key: &str) -> Option<Map<String, Value>> {
    text.match_indices('{').find_map(|(start, _)| {
        let candidate = &text[start..];
        let end = balanced_len(candidate)?;
        match serde_json::from_str(&candidate[..end]) {
            Ok(Value::Object(object)) if object.contains_key(key) => Some(object),
            _ => None,
        }
    })
}

/// Byte length of the balanced object at the start of `s`, skipping braces inside JSON strings.
fn balanced_len(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    // Scanning bytes is safe: the delimiters are ASCII and never appear inside multi-byte chars.
    for (i, byte) in s.bytes().enumerate() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Accepts `3`, `"3"` or `null`; models sometimes quote numbers.
fn parse_target(value: &Value) -> Result<Option<PlayerId>, ReplyError> {
    let bad = || ReplyError::BadTarget(truncate(&value.to_string(), 40).into());
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| PlayerId::try_from(n).ok())
            .map(Some)
            .ok_or_else(bad),
        Value::String(s) => s.trim().parse().map(Some).map_err(|_| bad()),
        _ => Err(bad()),
    }
}

/// Trims and caps text at `max_chars`; `None` when nothing is left.
fn clean_text(text: &str, max_chars: usize) -> Option<String> {
    let text = truncate(text.trim(), max_chars).trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

/// First `max_chars` chars of `s`; the server counts text limits in chars.
pub fn truncate(s: &str, max_chars: usize) -> &str {
    s.char_indices()
        .nth(max_chars)
        .map_or(s, |(end, _)| &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use werewolf_protocol::{Phase, PlayerView, Role};

    fn turn(kind: TurnKind, candidates: Vec<PlayerId>) -> Turn {
        Turn {
            turn_id: 1,
            kind,
            candidates,
            deadline_ms: 0,
            view: PlayerView {
                you: 1,
                role: Role::Villager,
                phase: Phase::Day { n: 1 },
                players: vec![],
                wolf_team: vec![],
                public_log: vec![],
                private_log: vec![],
            },
        }
    }

    fn speak(text: &str) -> Action {
        Action::Speak { text: text.into() }
    }

    #[test]
    fn extracts_fenced_json() {
        let text = "```json\n{\"reasoning\": \"r\", \"say\": \"hello\"}\n```";
        let decision = parse(text, &turn(TurnKind::DaySpeak, vec![])).unwrap();
        assert_eq!(decision.action, speak("hello"));
        assert_eq!(decision.reasoning.as_deref(), Some("r"));
    }

    #[test]
    fn extracts_json_surrounded_by_prose_with_stray_braces() {
        let text = "Sure {thinking}. Here it is: {\"target\": 3} hope that helps }";
        let decision = parse(text, &turn(TurnKind::DayVote, vec![2, 3])).unwrap();
        assert_eq!(decision.action, Action::Target { target: Some(3) });
        assert_eq!(decision.reasoning, None);
    }

    #[test]
    fn braces_and_escaped_quotes_inside_strings_do_not_end_the_object() {
        let text = r#"{"reasoning": "a } b { c", "say": "I {really} mean \"it\" }"} trailing"#;
        let decision = parse(text, &turn(TurnKind::WolfChat, vec![])).unwrap();
        assert_eq!(decision.action, speak(r#"I {really} mean "it" }"#));
    }

    #[test]
    fn finds_object_nested_in_wrapper() {
        let text = r#"{"response": {"reasoning": "r", "say": "hi"}}"#;
        let decision = parse(text, &turn(TurnKind::DaySpeak, vec![])).unwrap();
        assert_eq!(decision.action, speak("hi"));
    }

    #[test]
    fn invalid_replies_are_errors() {
        let speak_turn = turn(TurnKind::DaySpeak, vec![]);
        assert_eq!(
            parse("I vote for Bob", &speak_turn),
            Err(ReplyError::NoJson("say"))
        );
        assert_eq!(
            parse("{\"say\": \"unterminated", &speak_turn),
            Err(ReplyError::NoJson("say"))
        );
        assert_eq!(parse("{\"say\": 5}", &speak_turn), Err(ReplyError::BadSay));
        assert_eq!(parse("", &speak_turn), Err(ReplyError::NoJson("say")));
    }

    #[test]
    fn validates_targets_against_candidates() {
        let vote = turn(TurnKind::DayVote, vec![2, 3]);
        assert_eq!(
            parse(r#"{"target": 9}"#, &vote),
            Err(ReplyError::NotCandidate(9))
        );
        assert!(matches!(
            parse(r#"{"target": -1}"#, &vote),
            Err(ReplyError::BadTarget(_))
        ));
        assert!(matches!(
            parse(r#"{"target": "Bob"}"#, &vote),
            Err(ReplyError::BadTarget(_))
        ));
        assert_eq!(
            parse(r#"{"target": "2"}"#, &vote).unwrap().action,
            Action::Target { target: Some(2) }
        );
        assert_eq!(
            parse(r#"{"target": null}"#, &vote).unwrap().action,
            Action::Target { target: None }
        );
    }

    #[test]
    fn fallback_is_silence_or_abstain() {
        assert_eq!(Decision::fallback(TurnKind::DaySpeak).action, speak(""));
        assert_eq!(Decision::fallback(TurnKind::WolfChat).action, speak(""));
        for kind in [
            TurnKind::DayVote,
            TurnKind::WolfKill,
            TurnKind::SeerInspect,
            TurnKind::DoctorProtect,
        ] {
            assert_eq!(
                Decision::fallback(kind).action,
                Action::Target { target: None }
            );
        }
    }

    #[test]
    fn long_text_is_truncated_by_chars() {
        let say = "é".repeat(MAX_TEXT_LEN + 10);
        let text =
            serde_json::json!({ "say": say, "reasoning": "é".repeat(MAX_REASONING_LEN + 5) })
                .to_string();
        let decision = parse(&text, &turn(TurnKind::DaySpeak, vec![])).unwrap();
        let Action::Speak { text } = decision.action else {
            panic!("expected speak")
        };
        assert_eq!(text, "é".repeat(MAX_TEXT_LEN));
        assert_eq!(decision.reasoning, Some("é".repeat(MAX_REASONING_LEN)));
    }
}
