//! API error types and response formatting.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// API error type that converts to appropriate HTTP responses.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Authentication failed (missing or invalid token).
    #[error("unauthorized")]
    Unauthorized,

    /// Resource not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Invalid request parameters.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Internal server error (database, etc.).
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// ClickHouse query error.
    #[error("database error: {0}")]
    Database(#[from] clickhouse::error::Error),

    /// JSON serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// JSON error response body.
#[derive(Debug, Clone, Serialize)]
struct ErrorResponse {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error, message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", None),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", Some(msg.clone())),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", Some(msg.clone())),
            Self::Internal(err) => {
                tracing::error!(error = %err, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    Some("An internal error occurred".to_string()),
                )
            }
            Self::Database(err) => {
                tracing::error!(error = %err, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "database_error",
                    Some("A database error occurred".to_string()),
                )
            }
            Self::Serialization(err) => {
                tracing::error!(error = %err, "serialization error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "serialization_error",
                    Some("A serialization error occurred".to_string()),
                )
            }
        };

        let body = ErrorResponse {
            error: error.to_string(),
            message,
        };

        (status, Json(body)).into_response()
    }
}

