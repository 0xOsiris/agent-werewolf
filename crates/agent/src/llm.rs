//! OpenRouter chat-completions client (OpenAI-compatible).

use crate::{
    backoff::full_jitter,
    http::{HttpError, MAX_ERROR_BODY, send_json},
    reply::truncate,
};
use anyhow::Context;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::Instant;
use tracing::warn;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(4);
const TEMPERATURE: f32 = 0.8;
/// Hidden reasoning tokens count against this on OpenRouter, so leave room beyond the reply.
pub const MAX_TOKENS: u32 = 4000;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

#[derive(Serialize, Clone, Debug)]
pub struct Message {
    pub role: ChatRole,
    pub content: String,
}

impl Message {
    pub const fn new(role: ChatRole, content: String) -> Self {
        Self { role, content }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    temperature: f32,
    max_tokens: u32,
    reasoning: ReasoningConfig,
}

/// OpenRouter's unified reasoning knob; low effort keeps turns fast on stage.
#[derive(Serialize)]
struct ReasoningConfig {
    effort: &'static str,
    exclude: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    /// OpenRouter reports some upstream failures as a 200 with an error object.
    error: Option<UpstreamError>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct UpstreamError {
    code: Option<u16>,
    message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("no time left before the turn deadline")]
    OutOfTime,
    /// Usually a reasoning model that spent the whole token budget thinking.
    #[error("model returned no text (finish_reason: {})", .finish_reason.as_deref().unwrap_or("none"))]
    EmptyReply { finish_reason: Option<String> },
}

impl LlmError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Http(e) => e.class(),
            Self::OutOfTime => "out_of_time",
            Self::EmptyReply { .. } => "empty_reply",
        }
    }

    /// Errors a retry can't fix, such as a bad key or unknown model; the operator must act.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Http(e) if !e.is_retryable())
    }
}

/// Holds the API key, so deliberately not `Debug`.
pub struct OpenRouter {
    http: Client,
    url: String,
    api_key: String,
    model: String,
    max_call_timeout: Duration,
}

impl OpenRouter {
    pub fn new(
        base_url: &str,
        api_key: String,
        model: String,
        max_call_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("failed to build OpenRouter HTTP client")?;
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        Ok(Self {
            http,
            url,
            api_key,
            model,
            max_call_timeout,
        })
    }

    /// Returns the reply text, retrying transient failures while `deadline` allows.
    pub async fn complete(
        &self,
        messages: &[Message],
        deadline: Instant,
    ) -> Result<String, LlmError> {
        let mut attempt = 1;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LlmError::OutOfTime);
            }
            let error = match self
                .call(messages, self.max_call_timeout.min(remaining))
                .await
            {
                Ok(text) => return Ok(text),
                Err(LlmError::Http(error)) => error,
                Err(other) => return Err(other),
            };
            if attempt >= MAX_ATTEMPTS || !error.is_retryable() {
                return Err(error.into());
            }
            // A Retry-After that doesn't fit the turn is honored by giving up, not by ignoring it.
            let delay = error
                .retry_after()
                .unwrap_or_else(|| full_jitter(RETRY_BASE, RETRY_CAP, attempt - 1));
            if Instant::now() + delay >= deadline {
                return Err(error.into());
            }
            warn!(attempt, error_class = error.class(), %error, "LLM call failed; retrying");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    async fn call(&self, messages: &[Message], timeout: Duration) -> Result<String, LlmError> {
        let body = ChatRequest {
            model: &self.model,
            messages,
            temperature: TEMPERATURE,
            max_tokens: MAX_TOKENS,
            reasoning: ReasoningConfig {
                effort: "low",
                exclude: true,
            },
        };
        let request = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .header("X-Title", "Agent Werewolf")
            .timeout(timeout)
            .json(&body);
        let response: ChatResponse = send_json(request).await?;
        if let Some(error) = response.error {
            let status = error.code.and_then(|c| StatusCode::from_u16(c).ok());
            return Err(HttpError::Status {
                status: status.unwrap_or(StatusCode::BAD_GATEWAY),
                retry_after: None,
                body: truncate(&error.message, MAX_ERROR_BODY).into(),
            }
            .into());
        }
        let Some(choice) = response.choices.into_iter().next() else {
            return Err(LlmError::EmptyReply {
                finish_reason: None,
            });
        };
        match choice.message.content {
            Some(text) if !text.trim().is_empty() => Ok(text),
            _ => Err(LlmError::EmptyReply {
                finish_reason: choice.finish_reason,
            }),
        }
    }
}
