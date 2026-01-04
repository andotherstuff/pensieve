//! Bearer token authentication middleware.

use axum::extract::Request;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::ApiError;
use crate::state::AppState;

/// Middleware that requires a valid Bearer token for all requests.
///
/// The token must be provided in the `Authorization` header as:
/// ```text
/// Authorization: Bearer <token>
/// ```
///
/// Tokens are validated against the list configured in `PENSIEVE_API_TOKENS`.
pub async fn require_auth(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => {
            tracing::debug!("missing or malformed authorization header");
            return Err(ApiError::Unauthorized);
        }
    };

    if !state.config.api_tokens.contains(token) {
        tracing::debug!("invalid api token");
        return Err(ApiError::Unauthorized);
    }

    Ok(next.run(request).await)
}
