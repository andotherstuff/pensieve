//! Relay statistics API endpoints.
//!
//! Provides endpoints for querying relay discovery, connection status,
//! and event throughput metrics from the ingester's SQLite database.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

/// Helper to convert errors to ApiError::Internal.
fn internal_error(msg: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("{}", msg))
}

// ═══════════════════════════════════════════════════════════════════════════
// Response Types
// ═══════════════════════════════════════════════════════════════════════════

/// Summary of relay statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaySummary {
    /// Total number of discovered relays.
    pub total_discovered: u64,
    /// Number of currently active (connected) relays.
    pub total_active: u64,
    /// Number of blocked relays.
    pub total_blocked: u64,
    /// Number of relays currently failing.
    pub total_failing: u64,
    /// Number of idle relays (known but not connected).
    pub total_idle: u64,
    /// Number of pending relays (never connected).
    pub total_pending: u64,
    /// Average events per minute across all relays (last hour).
    pub avg_events_per_minute: f64,
    /// Total events received in the last hour.
    pub events_last_hour: u64,
    /// Total novel events received in the last hour.
    pub novel_events_last_hour: u64,
}

/// Individual relay status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayStatus {
    /// Relay URL.
    pub url: String,
    /// Current status (active, idle, pending, failing, blocked).
    pub status: String,
    /// Relay tier (seed, discovered).
    pub tier: String,
    /// Last connection timestamp (Unix seconds).
    pub last_connected_at: Option<i64>,
    /// Consecutive connection failures.
    pub consecutive_failures: u32,
    /// Reason for blocking (if blocked).
    pub blocked_reason: Option<String>,
    /// Relay score (if available).
    pub score: Option<f64>,
    /// Events received in the last hour.
    pub events_last_hour: u64,
    /// Novel events in the last hour.
    pub novel_events_last_hour: u64,
}

/// Response for relay list endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayListResponse {
    /// List of relays.
    pub relays: Vec<RelayStatus>,
    /// Total count (for pagination).
    pub total: u64,
}

/// Hourly relay throughput.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHourlyThroughput {
    /// Hour start (Unix timestamp).
    pub hour_start: i64,
    /// Total events received this hour.
    pub events_received: u64,
    /// Total novel events this hour.
    pub events_novel: u64,
    /// Active relays during this hour.
    pub active_relays: u64,
}

// ═══════════════════════════════════════════════════════════════════════════
// Query Parameters
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for relay list.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayListQuery {
    /// Filter by status (active, idle, pending, failing, blocked).
    pub status: Option<String>,
    /// Filter by tier (seed, discovered).
    pub tier: Option<String>,
    /// Sort by (score, events, url). Default: score.
    pub sort_by: Option<String>,
    /// Sort order (asc, desc). Default: desc.
    pub order: Option<String>,
    /// Maximum number of results. Default: 100.
    pub limit: Option<u32>,
    /// Offset for pagination. Default: 0.
    pub offset: Option<u32>,
}

/// Query parameters for throughput history.
#[derive(Debug, Clone, Deserialize)]
pub struct ThroughputQuery {
    /// Number of hours to include. Default: 24.
    pub hours: Option<u32>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Endpoints
// ═══════════════════════════════════════════════════════════════════════════

/// `GET /api/v1/relays/summary`
///
/// Returns aggregate relay statistics.
pub async fn summary(State(state): State<AppState>) -> Result<Json<RelaySummary>, ApiError> {
    let relay_db = state
        .relay_db
        .as_ref()
        .ok_or_else(|| internal_error("Relay database not configured"))?;

    let conn = relay_db.lock();

    // Get status counts
    let mut stmt = conn
        .prepare("SELECT status, COUNT(*) as cnt FROM relays GROUP BY status")
        .map_err(|e| internal_error(format!("Query failed: {}", e)))?;

    let mut total_discovered: u64 = 0;
    let mut total_active: u64 = 0;
    let mut total_blocked: u64 = 0;
    let mut total_failing: u64 = 0;
    let mut total_idle: u64 = 0;
    let mut total_pending: u64 = 0;

    let mut rows = stmt
        .query([])
        .map_err(|e| internal_error(format!("Query failed: {}", e)))?;

    while let Some(row) = rows
        .next()
        .map_err(|e| internal_error(format!("Row fetch failed: {}", e)))?
    {
        let status: String = row.get(0).unwrap_or_default();
        let count: u64 = row.get(1).unwrap_or(0);
        total_discovered += count;

        match status.as_str() {
            "active" => total_active = count,
            "blocked" => total_blocked = count,
            "failing" => total_failing = count,
            "idle" => total_idle = count,
            "pending" => total_pending = count,
            _ => {}
        }
    }

    // Get events from the previous complete hour only
    // Current time rounded down to hour start gives us the current (incomplete) hour
    // We want the hour before that (the last complete hour)
    let now = chrono::Utc::now().timestamp();
    let current_hour_start = now - (now % 3600);
    let prev_hour_start = current_hour_start - 3600;

    let (events_last_hour, novel_events_last_hour): (u64, u64) = conn
        .query_row(
            "SELECT
                COALESCE(SUM(events_received), 0),
                COALESCE(SUM(events_novel), 0)
            FROM relay_stats_hourly
            WHERE hour_start >= ? AND hour_start < ?",
            [prev_hour_start, current_hour_start],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((0, 0));

    // Calculate events per minute
    let avg_events_per_minute = events_last_hour as f64 / 60.0;

    Ok(Json(RelaySummary {
        total_discovered,
        total_active,
        total_blocked,
        total_failing,
        total_idle,
        total_pending,
        avg_events_per_minute,
        events_last_hour,
        novel_events_last_hour,
    }))
}

/// `GET /api/v1/relays`
///
/// Returns a list of relays with optional filtering and sorting.
pub async fn list(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<RelayListQuery>,
) -> Result<Json<RelayListResponse>, ApiError> {
    let relay_db = state
        .relay_db
        .as_ref()
        .ok_or_else(|| internal_error("Relay database not configured"))?;

    let conn = relay_db.lock();

    let limit = params.limit.unwrap_or(100).min(1000);
    let offset = params.offset.unwrap_or(0);

    // Validate sort_by against allowlist to prevent SQL injection
    let sort_by = match params.sort_by.as_deref() {
        Some("events") => "events",
        Some("url") => "url",
        Some("score") | None => "score",
        Some(_) => "score", // Invalid values default to score
    };

    // Validate order against allowlist to prevent SQL injection
    let order = match params.order.as_deref().map(|s| s.to_lowercase()).as_deref() {
        Some("asc") => "ASC",
        Some("desc") | None => "DESC",
        Some(_) => "DESC", // Invalid values default to DESC
    };

    // Build WHERE clause
    let mut where_clauses = Vec::new();
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref status) = params.status {
        where_clauses.push("r.status = ?");
        bind_values.push(status.clone());
    }
    if let Some(ref tier) = params.tier {
        where_clauses.push("r.tier = ?");
        bind_values.push(tier.clone());
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    // Build ORDER BY clause (sort_by and order are validated above)
    let order_sql = match sort_by {
        "events" => format!("COALESCE(h.events_received, 0) {}", order),
        "url" => format!("r.url {}", order),
        _ => format!("COALESCE(s.score, 0) {}", order),
    };

    // Get the previous complete hour only (same logic as summary)
    let now = chrono::Utc::now().timestamp();
    let current_hour_start = now - (now % 3600);
    let prev_hour_start = current_hour_start - 3600;

    let query = format!(
        "SELECT
            r.url,
            r.status,
            r.tier,
            r.last_connected_at,
            r.consecutive_failures,
            r.blocked_reason,
            s.score,
            COALESCE(h.events_received, 0) as events_last_hour,
            COALESCE(h.events_novel, 0) as novel_events_last_hour
        FROM relays r
        LEFT JOIN relay_scores s ON r.url = s.relay_url
        LEFT JOIN (
            SELECT relay_url, SUM(events_received) as events_received, SUM(events_novel) as events_novel
            FROM relay_stats_hourly
            WHERE hour_start >= {} AND hour_start < {}
            GROUP BY relay_url
        ) h ON r.url = h.relay_url
        {}
        ORDER BY {}
        LIMIT {} OFFSET {}",
        prev_hour_start, current_hour_start, where_sql, order_sql, limit, offset
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| internal_error(format!("Query failed: {}", e)))?;

    // Bind parameters dynamically
    let params_iter: Vec<&dyn rusqlite::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::ToSql)
        .collect();

    let mut rows = stmt
        .query(params_iter.as_slice())
        .map_err(|e| internal_error(format!("Query failed: {}", e)))?;

    let mut relays = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| internal_error(format!("Row fetch failed: {}", e)))?
    {
        relays.push(RelayStatus {
            url: row.get(0).unwrap_or_default(),
            status: row.get(1).unwrap_or_default(),
            tier: row.get(2).unwrap_or_default(),
            last_connected_at: row.get(3).ok(),
            consecutive_failures: row.get(4).unwrap_or(0),
            blocked_reason: row.get(5).ok(),
            score: row.get(6).ok(),
            events_last_hour: row.get(7).unwrap_or(0),
            novel_events_last_hour: row.get(8).unwrap_or(0),
        });
    }

    // Get total count
    let count_query = format!("SELECT COUNT(*) FROM relays r {}", where_sql);
    let total: u64 = conn
        .query_row(&count_query, params_iter.as_slice(), |row| row.get(0))
        .unwrap_or(0);

    Ok(Json(RelayListResponse { relays, total }))
}

/// `GET /api/v1/relays/throughput`
///
/// Returns hourly event throughput for the last N hours.
pub async fn throughput(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ThroughputQuery>,
) -> Result<Json<Vec<RelayHourlyThroughput>>, ApiError> {
    let relay_db = state
        .relay_db
        .as_ref()
        .ok_or_else(|| internal_error("Relay database not configured"))?;

    let conn = relay_db.lock();
    let hours = params.hours.unwrap_or(24).min(168); // Max 1 week

    let hours_ago = chrono::Utc::now().timestamp() - (hours as i64 * 3600);
    let start_hour = hours_ago - (hours_ago % 3600);

    let mut stmt = conn
        .prepare(
            "SELECT
                hour_start,
                SUM(events_received) as events_received,
                SUM(events_novel) as events_novel,
                COUNT(DISTINCT relay_url) as active_relays
            FROM relay_stats_hourly
            WHERE hour_start >= ?
            GROUP BY hour_start
            ORDER BY hour_start ASC",
        )
        .map_err(|e| internal_error(format!("Query failed: {}", e)))?;

    let mut rows = stmt
        .query([start_hour])
        .map_err(|e| internal_error(format!("Query failed: {}", e)))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| internal_error(format!("Row fetch failed: {}", e)))?
    {
        results.push(RelayHourlyThroughput {
            hour_start: row.get(0).unwrap_or(0),
            events_received: row.get(1).unwrap_or(0),
            events_novel: row.get(2).unwrap_or(0),
            active_relays: row.get(3).unwrap_or(0),
        });
    }

    Ok(Json(results))
}
