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

    if !token_is_valid(&state.config.api_tokens, token) {
        tracing::debug!("invalid api token");
        return Err(ApiError::Unauthorized);
    }

    Ok(next.run(request).await)
}

/// Constant-time check that `token` matches one of the configured tokens.
///
/// Iterates the entire set without early exit and compares each candidate with a
/// constant-time byte comparison, so response timing does not reveal how many
/// leading characters of a guessed token were correct (which a naive
/// `HashSet::contains` / `==` would leak).
fn token_is_valid(tokens: &std::collections::HashSet<String>, token: &str) -> bool {
    let mut valid = false;
    for known in tokens {
        // Bitwise `|=` (not `||`) so the comparison runs for every token.
        valid |= constant_time_eq(known.as_bytes(), token.as_bytes());
    }
    valid
}

/// Constant-time equality for two byte slices.
///
/// Returns early only on a length mismatch (token length is not a meaningful
/// secret); for equal lengths the comparison time is independent of the content.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
