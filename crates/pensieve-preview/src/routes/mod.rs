//! Route definitions for the preview service.
//!
//! ## Routes
//!
//! - `GET /` - Home page
//! - `GET /health` - Health check (JSON)
//! - `GET /robots.txt` - Crawler instructions
//! - `GET /llms.txt` - LLM agent description
//! - `GET /llms-full.txt` - LLM agent full documentation
//! - `GET /{identifier}` - Preview page (or .json for API)

mod health;
mod home;
pub mod json;
mod llms;
mod preview;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::state::AppState;

/// Build the complete preview service router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home::home_page))
        .route("/health", get(health::health_check))
        .route("/robots.txt", get(robots_txt))
        .route("/llms.txt", get(llms::llms_txt))
        .route("/llms-full.txt", get(llms::llms_full_txt))
        .route("/{identifier}", get(preview::preview_handler))
        .with_state(state)
}

/// Serve robots.txt allowing all crawlers.
///
/// We want crawlers to fetch these pages for link previews.
async fn robots_txt() -> impl IntoResponse {
    (
        [("content-type", "text/plain; charset=utf-8")],
        "User-agent: *\nAllow: /\n",
    )
}
