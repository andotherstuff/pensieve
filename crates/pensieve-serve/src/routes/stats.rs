//! Stats endpoints for aggregate analytics.

use axum::extract::{Query, State};
use axum::Json;
use chrono::NaiveDate;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

// ═══════════════════════════════════════════════════════════════════════════
// Overview
// ═══════════════════════════════════════════════════════════════════════════

/// High-level stats overview.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct OverviewResponse {
    pub total_events: u64,
    pub total_pubkeys: u64,
    pub total_kinds: u64,
    /// Earliest event timestamp (Unix seconds, 0 if no events).
    pub earliest_event: u32,
    /// Latest event timestamp (Unix seconds, 0 if no events).
    pub latest_event: u32,
}

/// `GET /api/v1/stats`
///
/// Returns high-level overview statistics.
pub async fn overview(State(state): State<AppState>) -> Result<Json<OverviewResponse>, ApiError> {
    let stats: OverviewResponse = state
        .clickhouse
        .query(
            "SELECT
                count() AS total_events,
                uniq(pubkey) AS total_pubkeys,
                uniq(kind) AS total_kinds,
                toUInt32(min(created_at)) AS earliest_event,
                toUInt32(max(created_at)) AS latest_event
            FROM events_local",
        )
        .fetch_one()
        .await?;

    Ok(Json(stats))
}

// ═══════════════════════════════════════════════════════════════════════════
// Events
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for event stats.
#[derive(Debug, Clone, Deserialize)]
pub struct EventStatsQuery {
    /// Filter by event kind.
    pub kind: Option<u16>,
    /// Filter events created on or after this date (YYYY-MM-DD).
    pub since: Option<NaiveDate>,
    /// Filter events created before this date (YYYY-MM-DD).
    pub until: Option<NaiveDate>,
    /// Shorthand: events from the last N days.
    pub days: Option<u32>,
    /// Group results by time period: "day", "week", "month".
    pub group_by: Option<String>,
    /// Limit number of results (default: 100, max: 1000).
    pub limit: Option<u32>,
}

/// Event count response (single aggregate).
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct EventCountResponse {
    pub count: u64,
    pub unique_pubkeys: u64,
}

/// Event count grouped by time period.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct EventCountByPeriod {
    /// Period as string (YYYY-MM-DD).
    pub period: String,
    pub count: u64,
    pub unique_pubkeys: u64,
}

/// `GET /api/v1/stats/events`
///
/// Returns event counts with optional filters and grouping.
pub async fn events(
    State(state): State<AppState>,
    Query(params): Query<EventStatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);

    // Build WHERE clause
    let mut conditions = Vec::new();
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(kind) = params.kind {
        conditions.push(format!("kind = {}", kind));
    }

    if let Some(days) = params.days {
        conditions.push(format!("created_at >= now() - INTERVAL {} DAY", days));
    } else {
        if let Some(since) = params.since {
            conditions.push("created_at >= ?".to_string());
            bind_values.push(since.to_string());
        }
        if let Some(until) = params.until {
            conditions.push("created_at < ?".to_string());
            bind_values.push(until.to_string());
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    // Determine if we're grouping by time period
    match params.group_by.as_deref() {
        Some("day") => {
            let query = format!(
                "SELECT
                    toString(toDate(created_at)) AS period,
                    count() AS count,
                    uniq(pubkey) AS unique_pubkeys
                FROM events_local
                {}
                GROUP BY period
                ORDER BY period DESC
                LIMIT {}",
                where_clause, limit
            );

            let mut q = state.clickhouse.query(&query);
            for val in &bind_values {
                q = q.bind(val);
            }
            let rows: Vec<EventCountByPeriod> = q.fetch_all().await?;
            Ok(Json(serde_json::to_value(rows)?))
        }
        Some("week") => {
            let query = format!(
                "SELECT
                    toString(toMonday(created_at)) AS period,
                    count() AS count,
                    uniq(pubkey) AS unique_pubkeys
                FROM events_local
                {}
                GROUP BY period
                ORDER BY period DESC
                LIMIT {}",
                where_clause, limit
            );

            let mut q = state.clickhouse.query(&query);
            for val in &bind_values {
                q = q.bind(val);
            }
            let rows: Vec<EventCountByPeriod> = q.fetch_all().await?;
            Ok(Json(serde_json::to_value(rows)?))
        }
        Some("month") => {
            let query = format!(
                "SELECT
                    toString(toStartOfMonth(created_at)) AS period,
                    count() AS count,
                    uniq(pubkey) AS unique_pubkeys
                FROM events_local
                {}
                GROUP BY period
                ORDER BY period DESC
                LIMIT {}",
                where_clause, limit
            );

            let mut q = state.clickhouse.query(&query);
            for val in &bind_values {
                q = q.bind(val);
            }
            let rows: Vec<EventCountByPeriod> = q.fetch_all().await?;
            Ok(Json(serde_json::to_value(rows)?))
        }
        Some(other) => Err(ApiError::BadRequest(format!(
            "invalid group_by value: '{}'. Valid options: day, week, month",
            other
        ))),
        None => {
            // No grouping - return aggregate
            let query = format!(
                "SELECT
                    count() AS count,
                    uniq(pubkey) AS unique_pubkeys
                FROM events_local
                {}",
                where_clause
            );

            let mut q = state.clickhouse.query(&query);
            for val in &bind_values {
                q = q.bind(val);
            }
            let stats: EventCountResponse = q.fetch_one().await?;
            Ok(Json(serde_json::to_value(stats)?))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Active Users
// ═══════════════════════════════════════════════════════════════════════════

/// Active users summary (current DAU/WAU/MAU).
#[derive(Debug, Clone, Serialize)]
pub struct ActiveUsersSummary {
    pub daily: ActiveUsersCount,
    pub weekly: ActiveUsersCount,
    pub monthly: ActiveUsersCount,
}

#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct ActiveUsersCount {
    pub active_users: u64,
    pub has_profile: u64,
    pub has_follows_list: u64,
    pub has_profile_and_follows_list: u64,
    pub total_events: u64,
}

/// `GET /api/v1/stats/users/active`
///
/// Returns current DAU/WAU/MAU summary.
pub async fn active_users_summary(
    State(state): State<AppState>,
) -> Result<Json<ActiveUsersSummary>, ApiError> {
    // Daily (last 24 hours)
    let daily: ActiveUsersCount = state
        .clickhouse
        .query(
            "SELECT
                uniq(pubkey) AS active_users,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
                uniqIf(pubkey,
                    pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
                    AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
                ) AS has_profile_and_follows_list,
                count() AS total_events
            FROM events_local
            WHERE kind NOT IN (445, 1059)
                AND created_at >= now() - INTERVAL 1 DAY",
        )
        .fetch_one()
        .await?;

    // Weekly (last 7 days)
    let weekly: ActiveUsersCount = state
        .clickhouse
        .query(
            "SELECT
                uniq(pubkey) AS active_users,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
                uniqIf(pubkey,
                    pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
                    AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
                ) AS has_profile_and_follows_list,
                count() AS total_events
            FROM events_local
            WHERE kind NOT IN (445, 1059)
                AND created_at >= now() - INTERVAL 7 DAY",
        )
        .fetch_one()
        .await?;

    // Monthly (last 30 days)
    let monthly: ActiveUsersCount = state
        .clickhouse
        .query(
            "SELECT
                uniq(pubkey) AS active_users,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
                uniqIf(pubkey,
                    pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
                    AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
                ) AS has_profile_and_follows_list,
                count() AS total_events
            FROM events_local
            WHERE kind NOT IN (445, 1059)
                AND created_at >= now() - INTERVAL 30 DAY",
        )
        .fetch_one()
        .await?;

    Ok(Json(ActiveUsersSummary {
        daily,
        weekly,
        monthly,
    }))
}

/// Query parameters for active users time series.
#[derive(Debug, Clone, Deserialize)]
pub struct ActiveUsersQuery {
    /// Number of periods to return (default: 30 for daily, 12 for weekly/monthly).
    pub limit: Option<u32>,
    /// Only include data from this date onwards (YYYY-MM-DD).
    pub since: Option<NaiveDate>,
}

/// Active users time series row.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct ActiveUsersRow {
    /// Period as string (YYYY-MM-DD).
    pub period: String,
    pub active_users: u64,
    pub has_profile: u64,
    pub has_follows_list: u64,
    pub has_profile_and_follows_list: u64,
    pub total_events: u64,
}

/// `GET /api/v1/stats/users/active/daily`
///
/// Returns daily active users time series.
pub async fn active_users_daily(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365);

    let since_clause = match params.since {
        Some(date) => format!("AND created_at >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<ActiveUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(toDate(created_at)) AS period,
                uniq(pubkey) AS active_users,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
                uniqIf(pubkey,
                    pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
                    AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
                ) AS has_profile_and_follows_list,
                count() AS total_events
            FROM events_local
            WHERE kind NOT IN (445, 1059)
            {}
            GROUP BY period
            ORDER BY period DESC
            LIMIT {}",
            since_clause, limit
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

/// `GET /api/v1/stats/users/active/weekly`
///
/// Returns weekly active users time series.
pub async fn active_users_weekly(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(52);

    let since_clause = match params.since {
        Some(date) => format!("AND created_at >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<ActiveUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(toMonday(created_at)) AS period,
                uniq(pubkey) AS active_users,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
                uniqIf(pubkey,
                    pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
                    AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
                ) AS has_profile_and_follows_list,
                count() AS total_events
            FROM events_local
            WHERE kind NOT IN (445, 1059)
            {}
            GROUP BY period
            ORDER BY period DESC
            LIMIT {}",
            since_clause, limit
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

/// `GET /api/v1/stats/users/active/monthly`
///
/// Returns monthly active users time series.
pub async fn active_users_monthly(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(120);

    let since_clause = match params.since {
        Some(date) => format!("AND created_at >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<ActiveUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(toStartOfMonth(created_at)) AS period,
                uniq(pubkey) AS active_users,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
                uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
                uniqIf(pubkey,
                    pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
                    AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
                ) AS has_profile_and_follows_list,
                count() AS total_events
            FROM events_local
            WHERE kind NOT IN (445, 1059)
            {}
            GROUP BY period
            ORDER BY period DESC
            LIMIT {}",
            since_clause, limit
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

