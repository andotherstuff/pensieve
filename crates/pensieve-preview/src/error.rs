//! Error types for the preview service.
//!
//! Errors are rendered as simple HTML error pages rather than JSON,
//! since this is a user-facing HTML service.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, html};

/// Preview service error type.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    /// The NIP-19 identifier could not be decoded.
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),

    /// The requested event or profile was not found in the database.
    #[error("not found: {0}")]
    NotFound(String),

    /// Internal server error (database, rendering, etc.).
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// ClickHouse query error.
    #[error("database error: {0}")]
    Database(#[from] clickhouse::error::Error),
}

impl IntoResponse for PreviewError {
    fn into_response(self) -> Response {
        let (status, title, message) = match &self {
            Self::InvalidIdentifier(msg) => (
                StatusCode::BAD_REQUEST,
                "Invalid Identifier",
                format!(
                    "The provided identifier could not be decoded as a valid Nostr entity: {msg}"
                ),
            ),
            Self::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                "Not Found",
                format!("The requested Nostr event or profile was not found: {msg}"),
            ),
            Self::Internal(err) => {
                tracing::error!(error = %err, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Error",
                    "An internal error occurred. Please try again later.".to_string(),
                )
            }
            Self::Database(err) => {
                tracing::error!(error = %err, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Service Unavailable",
                    "The database is temporarily unavailable. Please try again later.".to_string(),
                )
            }
        };

        let markup = html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    title { (title) " — nstr.to" }
                    meta name="robots" content="noindex";
                    link rel="icon" type="image/svg+xml" href="/favicon.svg";
                    style { (maud::PreEscaped(crate::render::components::ERROR_CSS)) }
                }
                body {
                    main class="error-page" {
                        h1 { (title) }
                        p { (message) }
                        a href="/" { "Back to nstr.to" }
                    }
                }
            }
        };

        (status, markup).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_invalid_identifier() {
        let err = PreviewError::InvalidIdentifier("bad input".to_string());
        assert_eq!(err.to_string(), "invalid identifier: bad input");
    }

    #[test]
    fn error_display_not_found() {
        let err = PreviewError::NotFound("event abc".to_string());
        assert_eq!(err.to_string(), "not found: event abc");
    }

    #[test]
    fn error_display_internal() {
        let err = PreviewError::Internal(anyhow::anyhow!("something broke"));
        assert_eq!(err.to_string(), "internal error: something broke");
    }

    #[test]
    fn error_into_response_invalid_identifier() {
        let err = PreviewError::InvalidIdentifier("test".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn error_into_response_not_found() {
        let err = PreviewError::NotFound("event xyz".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn error_into_response_internal() {
        let err = PreviewError::Internal(anyhow::anyhow!("boom"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
