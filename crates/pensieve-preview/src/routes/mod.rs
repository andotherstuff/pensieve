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
mod og;
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
        .route("/favicon.svg", get(favicon_svg))
        .route("/favicon.ico", get(favicon_svg))
        .route("/llms.txt", get(llms::llms_txt))
        .route("/llms-full.txt", get(llms::llms_full_txt))
        .route("/og/{identifier}", get(og::og_image_handler))
        .route("/{identifier}", get(preview::preview_handler))
        .with_state(state)
}

/// Serve robots.txt allowing all crawlers.
async fn robots_txt() -> impl IntoResponse {
    (
        [("content-type", "text/plain; charset=utf-8")],
        "User-agent: *\nAllow: /\n",
    )
}

/// SVG favicon — bold lowercase "n", adapts to light/dark mode.
const FAVICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><style>text{font-family:Inter,-apple-system,sans-serif}@media(prefers-color-scheme:light){rect{fill:#fff}text{fill:#111}}@media(prefers-color-scheme:dark){rect{fill:#0a0a0f}text{fill:#e5e5e5}}</style><rect width="32" height="32" rx="6" fill="#fff"/><text x="16" y="25" text-anchor="middle" font-size="26" font-weight="800" fill="#111">n</text></svg>"##;

/// Serve the SVG favicon.
async fn favicon_svg() -> impl IntoResponse {
    (
        [
            ("content-type", "image/svg+xml"),
            ("cache-control", "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
}
