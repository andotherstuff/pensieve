//! API route definitions.

mod health;
mod kinds;
mod stats;

use axum::extract::Request;
use axum::http::header;
use axum::middleware::{self, Next};
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
/// ### Health
/// - `GET /api/v1/ping` - Authenticated ping
///
/// ### Stats Overview
/// - `GET /api/v1/stats` - High-level overview (combined metrics)
///
/// ### Granular Stats
/// - `GET /api/v1/stats/events/total` - Approximate total event count (TTL: 5min)
/// - `GET /api/v1/stats/pubkeys/total` - Total unique pubkeys (TTL: 5min)
/// - `GET /api/v1/stats/kinds/total` - Distinct event kinds in last 30 days (TTL: 1hr)
/// - `GET /api/v1/stats/events/earliest` - Earliest event timestamp (TTL: 1hr)
/// - `GET /api/v1/stats/events/latest` - Latest event timestamp (TTL: 10s)
///
/// ### Event Stats
/// - `GET /api/v1/stats/events` - Event counts with filters
/// - `GET /api/v1/stats/throughput` - Events per hour (7-day rolling avg)
///
/// ### Active Users
/// - `GET /api/v1/stats/users/active` - DAU/WAU/MAU summary
/// - `GET /api/v1/stats/users/active/daily` - Daily active users time series
/// - `GET /api/v1/stats/users/active/weekly` - Weekly active users time series
/// - `GET /api/v1/stats/users/active/monthly` - Monthly active users time series
///
/// ### User Analytics
/// - `GET /api/v1/stats/users/new` - New users per period
/// - `GET /api/v1/stats/users/retention` - Cohort retention analysis
///
/// ### Activity Patterns
/// - `GET /api/v1/stats/activity/hourly` - Hourly activity pattern (0-23 UTC)
///
/// ### Zaps
/// - `GET /api/v1/stats/zaps` - Zap statistics
/// - `GET /api/v1/stats/zaps/histogram` - Zap amount distribution
///
/// ### Engagement
/// - `GET /api/v1/stats/engagement` - Reply/reaction engagement stats
///
/// ### Content
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
        // Stats overview (combined)
        .route("/stats", get(stats::overview))
        // Granular stats (for dashboards with independent caching)
        .route("/stats/events/total", get(stats::total_events))
        .route("/stats/pubkeys/total", get(stats::total_pubkeys))
        .route("/stats/kinds/total", get(stats::total_kinds))
        .route("/stats/events/earliest", get(stats::earliest_event))
        .route("/stats/events/latest", get(stats::latest_event))
        // Event stats
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
        .layer(middleware::from_fn(add_cache_headers));

    Router::new()
        .merge(public)
        .nest("/api/v1", api_v1)
        .with_state(state)
}

/// Add cache headers to API responses based on the endpoint.
///
/// TTLs are set per-endpoint based on how frequently the data changes:
/// - `/stats/events/latest`: 10 seconds (changes frequently)
/// - `/stats/events/total`, `/stats/pubkeys/total`: 5 minutes
/// - `/stats/kinds/total`, `/stats/events/earliest`: 1 hour (stable data)
/// - Other endpoints: 60 seconds default
async fn add_cache_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let response = next.run(request).await;

    // Only cache successful responses
    if !response.status().is_success() {
        return response;
    }

    // Determine TTL based on endpoint path (paths are relative to /api/v1 nest)
    let (max_age, stale_while_revalidate) = match path.as_str() {
        // Latest event changes frequently - short TTL
        "/api/v1/stats/events/latest" => (10, 30),
        // Total counts - moderate TTL (5 minutes)
        "/api/v1/stats/events/total" | "/api/v1/stats/pubkeys/total" => (300, 600),
        // Stable data - long TTL (1 hour)
        "/api/v1/stats/kinds/total" | "/api/v1/stats/events/earliest" => (3600, 7200),
        // Default for all other endpoints (1 minute)
        _ => (60, 300),
    };

    let cache_value = format!(
        "public, max-age={max_age}, stale-while-revalidate={stale_while_revalidate}"
    );

    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        header::CACHE_CONTROL,
        cache_value.parse().expect("valid header value"),
    );
    Response::from_parts(parts, body)
}
