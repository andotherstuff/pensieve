//! Event kinds endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use crate::cache::{get_or_compute, get_or_compute_with_ttl, ttl};
use crate::error::ApiError;
use crate::rollups::derived::KindsSort;
use crate::rollups::kind_activity::SeriesGrain;
use crate::routes::stats::{fetch_kind_detail, fetch_kind_series, fetch_kind_totals};
use crate::state::AppState;

/// Query parameters for kinds list.
#[derive(Debug, Clone, Deserialize)]
pub struct KindsQuery {
    /// Limit number of results (default: 100, max: 1000).
    pub limit: Option<u32>,
    /// Sort by: "count" (default) or "kind".
    pub sort: Option<String>,
}

/// Kind summary in the list.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct KindSummary {
    pub kind: u16,
    pub event_count: u64,
    pub unique_pubkeys: u64,
    /// First seen timestamp (Unix seconds).
    pub first_seen: u32,
    /// Last seen timestamp (Unix seconds).
    pub last_seen: u32,
}

/// `GET /api/v1/kinds`
///
/// Returns a list of event kinds with counts.
/// Reads the insert-fed `kind_totals` rollup (migration 020): event counts and
/// first/last seen are exact and current; `unique_pubkeys` is approximate
/// (`uniqCombined64`, ~0.3% standard error). Ties in `sort=count` are broken
/// by kind. Cached for 5 minutes.
pub async fn list_kinds(
    State(state): State<AppState>,
    Query(params): Query<KindsQuery>,
) -> Result<Json<Vec<KindSummary>>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);
    let sort = params.sort.clone();

    // Validate sort before caching
    let order_by = match sort.as_deref() {
        Some("kind") => KindsSort::Kind,
        Some("count") | None => KindsSort::Count,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "invalid sort value: '{}'. Valid options: count, kind",
                other
            )));
        }
    };

    let cache_key = format!(
        "kinds:limit={}&sort={}",
        limit,
        sort.as_deref().unwrap_or("count")
    );

    let cache = state.cache.clone();
    let result = get_or_compute(&cache, &cache_key, move || async move {
        let rows = fetch_kind_totals(&state, order_by, limit).await?;
        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}

/// Detailed stats for a single kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindDetail {
    pub kind: u16,
    pub event_count: u64,
    pub unique_pubkeys: u64,
    /// First seen timestamp (Unix seconds).
    pub first_seen: u32,
    /// Last seen timestamp (Unix seconds).
    pub last_seen: u32,
    pub avg_content_length: f64,
    /// Events in the last 24 hours.
    pub events_24h: u64,
    /// Events in the last 7 days.
    pub events_7d: u64,
    /// Events in the last 30 days.
    pub events_30d: u64,
}

/// `GET /api/v1/kinds/{kind}`
///
/// Returns detailed statistics for a specific event kind, from the
/// insert-fed rollups (no scan of `events_local`): `event_count`,
/// `first_seen`, `last_seen` and the approximate `unique_pubkeys` from
/// `kind_totals` (migration 020); `avg_content_length` and the 24 h / 7 d /
/// 30 d counts from `kind_activity` (migration 021). The windows start at the
/// UTC hour containing `now() - N days` and have no upper bound. `404` when
/// the kind has no events, `503 metric_unavailable` when a rollup is missing.
/// Cached for 5 minutes.
pub async fn get_kind(
    State(state): State<AppState>,
    Path(kind): Path<u16>,
) -> Result<Json<KindDetail>, ApiError> {
    let cache_key = format!("kind_detail:{}", kind);

    let cache = state.cache.clone();
    let result = get_or_compute(&cache, &cache_key, move || async move {
        fetch_kind_detail(&state, kind)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("kind {} not found", kind)))
    })
    .await?;

    Ok(Json(result))
}

/// Query parameters for kind time series.
#[derive(Debug, Clone, Deserialize)]
pub struct KindTimeSeriesQuery {
    /// Group by: "day" (default), "week", "month".
    pub group_by: Option<String>,
    /// Number of periods to return (default: 30).
    pub limit: Option<u32>,
}

/// Kind time series row.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct KindTimeSeriesRow {
    /// Period as string (YYYY-MM-DD).
    pub period: String,
    pub event_count: u64,
    pub unique_pubkeys: u64,
}

/// `GET /api/v1/kinds/{kind}/activity`
///
/// Returns activity time series for a specific kind from the `kind_activity`
/// rollup (migration 021): UTC days, Monday weeks or UTC months of
/// `created_at`, the current period included, future periods excluded.
/// `unique_pubkeys` is approximate (`uniqCombined64`). `503
/// metric_unavailable` when the rollup is missing. Cached for 10 minutes.
pub async fn kind_activity(
    State(state): State<AppState>,
    Path(kind): Path<u16>,
    Query(params): Query<KindTimeSeriesQuery>,
) -> Result<Json<Vec<KindTimeSeriesRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365);

    // Validate group_by before caching
    let Some(grain) = SeriesGrain::parse(params.group_by.as_deref()) else {
        return Err(ApiError::BadRequest(format!(
            "invalid group_by value: '{}'. Valid options: day, week, month",
            params.group_by.as_deref().unwrap_or_default()
        )));
    };
    let limit = limit.min(grain.max_limit());

    let cache_key = format!(
        "kind_activity:kind={}&group_by={}&limit={}",
        kind,
        grain.name(),
        limit
    );

    let cache = state.cache.clone();
    let result =
        get_or_compute_with_ttl(&cache, &cache_key, ttl::TIME_SERIES, move || async move {
            fetch_kind_series(&state, kind, grain, limit).await
        })
        .await?;

    Ok(Json(result))
}
