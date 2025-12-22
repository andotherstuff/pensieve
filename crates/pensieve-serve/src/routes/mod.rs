//! API route definitions.

mod health;
mod kinds;
mod stats;

use axum::middleware;
use axum::routing::get;
use axum::Router;

use crate::auth::require_auth;
use crate::state::AppState;

/// Build the complete API router.
///
/// # Route Structure
///
/// ## Public (no auth)
/// - `GET /health` - Health check
///
/// ## Protected (auth required)
///
/// ### Stats
/// - `GET /api/v1/stats` - High-level overview
/// - `GET /api/v1/stats/events` - Event counts with filters
/// - `GET /api/v1/stats/users/active` - DAU/WAU/MAU summary
/// - `GET /api/v1/stats/users/active/daily` - Daily active users time series
/// - `GET /api/v1/stats/users/active/weekly` - Weekly active users time series
/// - `GET /api/v1/stats/users/active/monthly` - Monthly active users time series
///
/// ### Kinds
/// - `GET /api/v1/kinds` - List all kinds with counts
/// - `GET /api/v1/kinds/{kind}` - Detailed stats for a kind
/// - `GET /api/v1/kinds/{kind}/activity` - Activity time series for a kind
pub fn router(state: AppState) -> Router {
    // Public routes (no authentication)
    let public = Router::new().route("/health", get(health::health_check));

    // Protected API routes
    let api_v1 = Router::new()
        // Health/auth check
        .route("/ping", get(health::authenticated_ping))
        // Stats overview
        .route("/stats", get(stats::overview))
        .route("/stats/events", get(stats::events))
        // Active users
        .route("/stats/users/active", get(stats::active_users_summary))
        .route("/stats/users/active/daily", get(stats::active_users_daily))
        .route("/stats/users/active/weekly", get(stats::active_users_weekly))
        .route(
            "/stats/users/active/monthly",
            get(stats::active_users_monthly),
        )
        // Kinds
        .route("/kinds", get(kinds::list_kinds))
        .route("/kinds/{kind}", get(kinds::get_kind))
        .route("/kinds/{kind}/activity", get(kinds::kind_activity))
        // Auth middleware
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .merge(public)
        .nest("/api/v1", api_v1)
        .with_state(state)
}
