//! Chooses an action for each turn, from an LLM or at random.

use crate::{
    llm::{ChatRole, LlmError, Message, OpenRouter},
    mock::MockBrain,
    prompt,
    reply::{self, Decision, ReplyError, is_speak},
};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, error, warn};
use werewolf_protocol::{Action, Turn};

/// Held back from the LLM so the action still reaches the server before the deadline.
const ACT_MARGIN: Duration = Duration::from_secs(3);
/// With less left in a chat window, a reply would land after it closes, so pass instead.
const MIN_CHAT_BUDGET: Duration = Duration::from_secs(4);

pub enum Brain {
    Llm(LlmBrain),
    Mock(MockBrain),
}

impl Brain {
    /// Always returns an action; on failure it is the server's own default, sent early.
    pub async fn decide(&mut self, turn: &Turn, deadline: Instant) -> Decision {
        if !is_speak(turn.kind) && turn.candidates.is_empty() {
            return Decision {
                action: Action::Target { target: None },
                reasoning: None,
            };
        }
        match self {
            Self::Llm(brain) => brain.decide(turn, deadline).await,
            Self::Mock(brain) => brain.decide(turn),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum DecideError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("reply still invalid after a correction: {0}")]
    Reply(#[from] ReplyError),
}

pub struct LlmBrain {
    llm: OpenRouter,
    persona: Option<String>,
}

impl LlmBrain {
    pub const fn new(llm: OpenRouter, persona: Option<String>) -> Self {
        Self { llm, persona }
    }

    async fn decide(&self, turn: &Turn, deadline: Instant) -> Decision {
        if is_speak(turn.kind)
            && deadline.saturating_duration_since(Instant::now()) < MIN_CHAT_BUDGET
        {
            debug!("chat window nearly closed; passing");
            return Decision::fallback(turn.kind);
        }
        let deadline = deadline
            .checked_sub(ACT_MARGIN)
            .unwrap_or_else(Instant::now);
        match self.ask(turn, deadline).await {
            Ok(decision) => decision,
            Err(err) => {
                let class = match &err {
                    DecideError::Llm(e) => e.class(),
                    DecideError::Reply(_) => "invalid_reply",
                };
                if matches!(&err, DecideError::Llm(e) if e.is_permanent()) {
                    error!(error_class = class, error = %err, "LLM request rejected; sending fallback action");
                } else {
                    warn!(error_class = class, error = %err, "no usable LLM decision; sending fallback action");
                }
                Decision::fallback(turn.kind)
            }
        }
    }

    /// One request, plus one correction round if the reply can't be used.
    async fn ask(&self, turn: &Turn, deadline: Instant) -> Result<Decision, DecideError> {
        let mut messages = vec![
            Message::new(
                ChatRole::System,
                prompt::system(turn.view.role, self.persona.as_deref()),
            ),
            Message::new(ChatRole::User, prompt::user(turn)),
        ];
        let reply = self.llm.complete(&messages, deadline).await?;
        let error = match reply::parse(&reply, turn) {
            Ok(decision) => return Ok(decision),
            Err(error) => error,
        };
        warn!(error_class = "invalid_reply", %error, "unusable LLM reply; asking again");

        messages.push(Message::new(ChatRole::Assistant, reply));
        messages.push(Message::new(
            ChatRole::User,
            prompt::correction(turn, &error),
        ));
        let reply = self.llm.complete(&messages, deadline).await?;
        Ok(reply::parse(&reply, turn)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::post,
    };
    use serde_json::{Value, json};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use werewolf_protocol::{Phase, PlayerView, Role, TurnKind};

    fn vote_turn() -> Turn {
        Turn {
            turn_id: 5,
            step: 0,
            round: None,
            kind: TurnKind::DayVote,
            candidates: vec![2, 3],
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

    fn completion(content: &str) -> Response {
        Json(json!({ "choices": [{ "message": { "role": "assistant", "content": content } }] }))
            .into_response()
    }

    /// Fake OpenRouter that answers the n-th request with `respond(n)` and counts requests.
    async fn fake_openrouter<F, Fut>(respond: F) -> (String, Arc<AtomicUsize>)
    where
        F: Fn(usize, Value) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send,
    {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |Json(body): Json<Value>| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                respond(n, body)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, calls)
    }

    fn brain(url: &str, call_timeout: Duration) -> Brain {
        let llm =
            OpenRouter::new(url, "test-key".into(), "test/model".into(), call_timeout).unwrap();
        Brain::Llm(LlmBrain::new(llm, None))
    }

    #[tokio::test]
    async fn retries_server_error_then_uses_reply() {
        let (url, calls) = fake_openrouter(|n, body| async move {
            assert_eq!(body["model"], "test/model");
            assert_eq!(body["max_tokens"], crate::llm::MAX_TOKENS);
            if n == 0 {
                (StatusCode::INTERNAL_SERVER_ERROR, "upstream exploded").into_response()
            } else {
                completion("```json\n{\"reasoning\": \"Bob dodged.\", \"target\": 2}\n```")
            }
        })
        .await;

        let deadline = Instant::now() + Duration::from_secs(20);
        let decision = brain(&url, Duration::from_secs(5))
            .decide(&vote_turn(), deadline)
            .await;
        assert_eq!(decision.action, Action::Target { target: Some(2) });
        assert_eq!(decision.reasoning.as_deref(), Some("Bob dodged."));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalid_target_is_corrected_once() {
        let (url, calls) = fake_openrouter(|n, body| async move {
            if n == 0 {
                completion(r#"{"reasoning": "hmm", "target": 99}"#)
            } else {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 4);
                assert!(
                    messages[3]["content"]
                        .as_str()
                        .unwrap()
                        .contains("99 is not one of the candidate ids")
                );
                completion(r#"{"reasoning": "fine", "target": 3}"#)
            }
        })
        .await;

        let deadline = Instant::now() + Duration::from_secs(20);
        let decision = brain(&url, Duration::from_secs(5))
            .decide(&vote_turn(), deadline)
            .await;
        assert_eq!(decision.action, Action::Target { target: Some(3) });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn slow_model_falls_back_before_deadline() {
        let (url, _calls) = fake_openrouter(|_, _| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            completion(r#"{"target": 2}"#)
        })
        .await;

        let started = Instant::now();
        let deadline = started + ACT_MARGIN + Duration::from_secs(1);
        let decision = brain(&url, Duration::from_secs(60))
            .decide(&vote_turn(), deadline)
            .await;
        assert_eq!(decision, Decision::fallback(TurnKind::DayVote));
        assert!(Instant::now() < deadline, "fallback must leave time to act");
    }

    #[tokio::test]
    async fn empty_reply_falls_back_without_reasking() {
        let (url, calls) = fake_openrouter(|_, _| async {
            Json(json!({ "choices": [{ "message": { "content": null }, "finish_reason": "length" }] }))
                .into_response()
        })
        .await;

        let deadline = Instant::now() + Duration::from_secs(20);
        let decision = brain(&url, Duration::from_secs(5))
            .decide(&vote_turn(), deadline)
            .await;
        assert_eq!(decision, Decision::fallback(TurnKind::DayVote));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nearly_closed_chat_window_passes_without_calling_the_llm() {
        let (url, calls) = fake_openrouter(|_, _| async { completion(r#"{"say": "hi"}"#) }).await;
        let mut turn = vote_turn();
        turn.kind = TurnKind::DaySpeak;
        turn.candidates.clear();

        let deadline = Instant::now() + MIN_CHAT_BUDGET - Duration::from_millis(500);
        let decision = brain(&url, Duration::from_secs(5))
            .decide(&turn, deadline)
            .await;
        assert_eq!(decision, Decision::fallback(TurnKind::DaySpeak));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bad_request_is_not_retried() {
        let (url, calls) = fake_openrouter(|_, _| async {
            (StatusCode::BAD_REQUEST, "unknown model").into_response()
        })
        .await;

        let deadline = Instant::now() + Duration::from_secs(20);
        let decision = brain(&url, Duration::from_secs(5))
            .decide(&vote_turn(), deadline)
            .await;
        assert_eq!(decision, Decision::fallback(TurnKind::DayVote));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
