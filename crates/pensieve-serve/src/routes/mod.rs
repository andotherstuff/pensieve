//! API route definitions.

mod health;
mod kinds;
mod stats;

use axum::http::header;
use axum::middleware;
use axum::response::Response;
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
/// - `GET /api/v1/stats/throughput` - Events per hour time series
/// - `GET /api/v1/stats/users/active` - DAU/WAU/MAU summary
/// - `GET /api/v1/stats/users/active/daily` - Daily active users time series
/// - `GET /api/v1/stats/users/active/weekly` - Weekly active users time series
/// - `GET /api/v1/stats/users/active/monthly` - Monthly active users time series
/// - `GET /api/v1/stats/users/retention` - User retention cohort analysis
/// - `GET /api/v1/stats/users/new` - New users time series
/// - `GET /api/v1/stats/activity/hourly` - Hourly activity pattern
/// - `GET /api/v1/stats/zaps` - Zap statistics
/// - `GET /api/v1/stats/zaps/histogram` - Zap amount distribution histogram
/// - `GET /api/v1/stats/engagement` - Reply/reaction engagement stats
/// - `GET /api/v1/stats/longform` - Long-form content (kind 30023) stats
/// - `GET /api/v1/stats/publishers` - Top publishers by event count
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
        .route("/stats/throughput", get(stats::throughput))
        // Active users
        .route("/stats/users/active", get(stats::active_users_summary))
        .route("/stats/users/active/daily", get(stats::active_users_daily))
        .route("/stats/users/active/weekly", get(stats::active_users_weekly))
        .route(
            "/stats/users/active/monthly",
            get(stats::active_users_monthly),
        )
        // User analytics
        .route("/stats/users/retention", get(stats::user_retention))
        .route("/stats/users/new", get(stats::new_users))
        // Activity patterns
        .route("/stats/activity/hourly", get(stats::hourly_activity))
        // Zaps
        .route("/stats/zaps", get(stats::zap_stats))
        .route("/stats/zaps/histogram", get(stats::zap_histogram))
        // Engagement
        .route("/stats/engagement", get(stats::engagement))
        // Long-form content
        .route("/stats/longform", get(stats::longform))
        // Publishers
        .route("/stats/publishers", get(stats::publishers))
        // Kinds
        .route("/kinds", get(kinds::list_kinds))
        .route("/kinds/{kind}", get(kinds::get_kind))
        .route("/kinds/{kind}/activity", get(kinds::kind_activity))
        // Auth middleware
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        // Cache headers middleware
        .layer(middleware::map_response(add_cache_headers));

    Router::new()
        .merge(public)
        .nest("/api/v1", api_v1)
        .with_state(state)
}

/// Add cache headers to API responses.
///
/// Sets a default cache duration of 60 seconds for successful responses.
/// This allows CDNs and clients to cache analytics data appropriately.
async fn add_cache_headers(response: Response) -> Response {
    // Only cache successful responses
    if response.status().is_success() {
        let (mut parts, body) = response.into_parts();
        // Cache for 60 seconds, allow stale responses for up to 5 minutes during revalidation
        parts.headers.insert(
            header::CACHE_CONTROL,
            "public, max-age=60, stale-while-revalidate=300"
                .parse()
                .expect("valid header value"),
        );
        Response::from_parts(parts, body)
    } else {
        response
    }
}
