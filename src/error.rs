use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use mongodb::error::{Error as MongoError, ErrorKind, WriteFailure};
use serde_json::json;

use crate::retry::is_transient;

const DUPLICATE_KEY: i32 = 11000;

/// Application error, rendered as `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Validation(String),
    /// The database is unreachable or overloaded; the client may retry later.
    Unavailable(String),
    /// The operation did not finish within the configured timeout.
    Timeout,
    /// The client exceeded the rate limit; holds the seconds until it may retry.
    RateLimited(u64),
    /// The detail is only logged (through `Debug`), never sent to the client.
    Internal(#[allow(dead_code)] String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", m.clone()),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, "NOT_FOUND", m.clone()),
            Self::Conflict(m) => (StatusCode::CONFLICT, "CONFLICT", m.clone()),
            Self::Validation(m) => (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR", m.clone()),
            Self::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE", m.clone()),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "TIMEOUT",
                "the database did not respond in time".to_string(),
            ),
            Self::RateLimited(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "too many requests, slow down".to_string(),
            ),
            // Internal details are logged, never leaked to the client.
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "an unexpected error occurred".to_string(),
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        if status.is_server_error() {
            tracing::error!(?self, "request failed");
        }
        let mut res = (status, Json(json!({ "error": { "code": code, "message": message } })))
            .into_response();
        match self {
            Self::Unavailable(_) | Self::Timeout => {
                res.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            }
            Self::RateLimited(secs) => {
                res.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(secs));
            }
            _ => {}
        }
        res
    }
}

pub fn is_duplicate_key(err: &MongoError) -> bool {
    match err.kind.as_ref() {
        ErrorKind::Write(WriteFailure::WriteError(e)) => e.code == DUPLICATE_KEY,
        ErrorKind::Command(e) => e.code == DUPLICATE_KEY,
        _ => false,
    }
}

impl From<MongoError> for AppError {
    fn from(err: MongoError) -> Self {
        if is_duplicate_key(&err) {
            return Self::Conflict("a resource with the same unique key already exists".into());
        }
        if is_transient(&err) {
            tracing::warn!(error = %err, "database unavailable");
            return Self::Unavailable("the database is temporarily unavailable".into());
        }
        match err.kind.as_ref() {
            ErrorKind::Authentication { .. } => {
                tracing::error!(error = %err, "database authentication failed");
                Self::Unavailable("the database is temporarily unavailable".into())
            }
            _ => Self::Internal(err.to_string()),
        }
    }
}

impl From<JsonRejection> for AppError {
    fn from(rej: JsonRejection) -> Self {
        match rej {
            JsonRejection::JsonDataError(e) => Self::Validation(e.body_text()),
            other => Self::BadRequest(other.body_text()),
        }
    }
}

impl From<QueryRejection> for AppError {
    fn from(rej: QueryRejection) -> Self {
        Self::BadRequest(rej.body_text())
    }
}

impl From<PathRejection> for AppError {
    fn from(rej: PathRejection) -> Self {
        Self::BadRequest(rej.body_text())
    }
}
