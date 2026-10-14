//! JSON-over-HTTP helpers shared by the game server and OpenRouter clients.

use crate::reply::truncate;
use reqwest::{RequestBuilder, Response, StatusCode, header::RETRY_AFTER};
use serde::de::DeserializeOwned;
use std::time::Duration;

/// Upper bound on any response body; a full game view is a few hundred KB at most.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Error bodies end up in logs, so keep them short.
pub const MAX_ERROR_BODY: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Status {
        status: StatusCode,
        retry_after: Option<Duration>,
        body: String,
    },
    #[error("response body exceeds {MAX_BODY_BYTES} bytes")]
    TooLarge,
    #[error("unexpected response body ({source}): {body}")]
    Decode {
        source: serde_json::Error,
        body: String,
    },
}

impl HttpError {
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Network failures, timeouts, 408, 429 and 5xx may succeed on retry; other errors won't.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, .. } => {
                status.is_server_error()
                    || *status == StatusCode::TOO_MANY_REQUESTS
                    || *status == StatusCode::REQUEST_TIMEOUT
            }
            Self::TooLarge | Self::Decode { .. } => false,
        }
    }

    /// Coarse failure class for structured logs.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Transport(e) if e.is_timeout() => "timeout",
            Self::Transport(e) if e.is_connect() => "connect",
            Self::Transport(_) => "transport",
            Self::Status { status, .. } if status.is_server_error() => "http_5xx",
            Self::Status { status, .. } if *status == StatusCode::TOO_MANY_REQUESTS => {
                "rate_limited"
            }
            Self::Status { .. } => "http_4xx",
            Self::TooLarge => "too_large",
            Self::Decode { .. } => "decode",
        }
    }
}

/// Sends `request` and returns the body of a 2xx response.
pub async fn send(request: RequestBuilder) -> Result<String, HttpError> {
    let response = request.send().await?;
    let status = response.status();
    let retry_after = retry_after(&response);
    let body = read_body(response).await?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(HttpError::Status {
            status,
            retry_after,
            body: truncate(&body, MAX_ERROR_BODY).into(),
        })
    }
}

/// Sends `request` and decodes a 2xx JSON body.
pub async fn send_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, HttpError> {
    let body = send(request).await?;
    serde_json::from_str(&body).map_err(|source| HttpError::Decode {
        source,
        body: truncate(&body, MAX_ERROR_BODY).into(),
    })
}

async fn read_body(mut response: Response) -> Result<String, HttpError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Only the delay-seconds form; HTTP-date values are rare enough to ignore.
fn retry_after(response: &Response) -> Option<Duration> {
    let secs = response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}
