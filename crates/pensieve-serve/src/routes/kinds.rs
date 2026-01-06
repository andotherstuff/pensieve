//! Event kinds endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use crate::cache::get_or_compute;
use crate::error::ApiError;
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
/// Reads from pre-aggregated `kinds_stats_mv` (refreshed hourly with exact counts).
/// Cached for 5 minutes.
pub async fn list_kinds(
    State(state): State<AppState>,
    Query(params): Query<KindsQuery>,
) -> Result<Json<Vec<KindSummary>>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);
    let sort = params.sort.clone();

    // Validate sort before caching
    let order_by = match sort.as_deref() {
        Some("kind") => "kind ASC",
        Some("count") | None => "event_count DESC",
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

    let result = get_or_compute(&state.cache, &cache_key, || async {
        // Read from pre-aggregated MV (hourly refresh with uniqExact)
        let rows: Vec<KindSummary> = state
            .clickhouse
            .query(&format!(
                "SELECT
                    kind,
                    event_count,
                    unique_pubkeys,
                    first_seen,
                    last_seen
                FROM kinds_stats_mv
                FINAL
                ORDER BY {}
                LIMIT {}",
                order_by, limit
            ))
            .fetch_all()
            .await?;

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

#[derive(Debug, Clone, Deserialize, Row)]
struct KindBasicStats {
    kind: u16,
    event_count: u64,
    unique_pubkeys: u64,
    first_seen: u32,
    last_seen: u32,
    avg_content_length: f64,
}

#[derive(Debug, Clone, Deserialize, Row)]
struct KindRecentStats {
    events_24h: u64,
    events_7d: u64,
    events_30d: u64,
}

/// `GET /api/v1/kinds/{kind}`
///
/// Returns detailed statistics for a specific event kind.
/// Cached for 5 minutes.
pub async fn get_kind(
    State(state): State<AppState>,
    Path(kind): Path<u16>,
) -> Result<Json<KindDetail>, ApiError> {
    let cache_key = format!("kind_detail:{}", kind);

    let result = get_or_compute(&state.cache, &cache_key, || async {
        // Get basic stats
        let basic: KindBasicStats = state
            .clickhouse
            .query(
                "SELECT
                    kind,
                    count() AS event_count,
                    uniqExact(pubkey) AS unique_pubkeys,
                    toUInt32(min(created_at)) AS first_seen,
                    toUInt32(max(created_at)) AS last_seen,
                    avg(length(content)) AS avg_content_length
                FROM events_local
                WHERE kind = ?
                GROUP BY kind",
            )
            .bind(kind)
            .fetch_optional()
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("kind {} not found", kind)))?;

        // Get recent activity
        let recent: KindRecentStats = state
            .clickhouse
            .query(
                "SELECT
                    countIf(created_at >= now() - INTERVAL 1 DAY) AS events_24h,
                    countIf(created_at >= now() - INTERVAL 7 DAY) AS events_7d,
                    countIf(created_at >= now() - INTERVAL 30 DAY) AS events_30d
                FROM events_local
                WHERE kind = ?",
            )
            .bind(kind)
            .fetch_one()
            .await?;

        Ok(KindDetail {
            kind: basic.kind,
            event_count: basic.event_count,
            unique_pubkeys: basic.unique_pubkeys,
            first_seen: basic.first_seen,
            last_seen: basic.last_seen,
            avg_content_length: basic.avg_content_length,
            events_24h: recent.events_24h,
            events_7d: recent.events_7d,
            events_30d: recent.events_30d,
        })
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
/// Returns activity time series for a specific kind.
/// Cached for 5 minutes.
pub async fn kind_activity(
    State(state): State<AppState>,
    Path(kind): Path<u16>,
    Query(params): Query<KindTimeSeriesQuery>,
) -> Result<Json<Vec<KindTimeSeriesRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365);
    let group_by = params.group_by.clone();

    // Validate group_by before caching
    let (group_expr, max_limit) = match group_by.as_deref() {
        Some("week") => ("toString(toMonday(created_at))", 52u32),
        Some("month") => ("toString(toStartOfMonth(created_at))", 120u32),
        Some("day") | None => ("toString(toDate(created_at))", 365u32),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "invalid group_by value: '{}'. Valid options: day, week, month",
                other
            )));
        }
    };

    let limit = limit.min(max_limit);

    let cache_key = format!(
        "kind_activity:kind={}&group_by={}&limit={}",
        kind,
        group_by.as_deref().unwrap_or("day"),
        limit
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        let rows: Vec<KindTimeSeriesRow> = state
            .clickhouse
            .query(&format!(
                "SELECT
                    {} AS period,
                    count() AS event_count,
                    uniqExact(pubkey) AS unique_pubkeys
                FROM events_local
                WHERE kind = ?
                GROUP BY period
                ORDER BY period DESC
                LIMIT {}",
                group_expr, limit
            ))
            .bind(kind)
            .fetch_all()
            .await?;

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}
