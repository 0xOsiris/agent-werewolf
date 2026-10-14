//! HTTP error type; every failure renders as an `ApiError` JSON body.

use axum::{
    Json,
    extract::rejection::{BytesRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use werewolf_protocol::ApiError;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("missing or unknown bearer token")]
    Unauthorized,
    #[error("missing or wrong spectator key")]
    BadKey,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{message}")]
    Rejected { status: StatusCode, message: String },
    #[error("not found")]
    NotFound,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("internal server error")]
    Internal,
}

impl HttpError {
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized | Self::BadKey => StatusCode::UNAUTHORIZED,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Rejected { status, .. } => *status,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (
            self.status(),
            Json(ApiError {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

impl From<BytesRejection> for HttpError {
    fn from(rejection: BytesRejection) -> Self {
        Self::Rejected {
            status: rejection.status(),
            message: rejection.body_text(),
        }
    }
}

impl From<QueryRejection> for HttpError {
    fn from(rejection: QueryRejection) -> Self {
        Self::Rejected {
            status: rejection.status(),
            message: rejection.body_text(),
        }
    }
}

impl From<serde_json::Error> for HttpError {
    fn from(err: serde_json::Error) -> Self {
        let status = match err.classify() {
            serde_json::error::Category::Data => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::BAD_REQUEST,
        };
        Self::Rejected {
            status,
            message: format!("invalid JSON body: {err}"),
        }
    }
}
