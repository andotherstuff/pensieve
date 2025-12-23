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

// ═══════════════════════════════════════════════════════════════════════════
// Throughput
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for throughput endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ThroughputQuery {
    /// Number of hours to return (default: 24, max: 168).
    pub limit: Option<u32>,
    /// Filter by event kind.
    pub kind: Option<u16>,
}

/// Throughput time series row.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct ThroughputRow {
    /// Hour as string (YYYY-MM-DD HH:00:00).
    pub hour: String,
    pub event_count: u64,
    pub unique_pubkeys: u64,
}

/// `GET /api/v1/stats/throughput`
///
/// Returns events per hour time series (based on event created_at).
pub async fn throughput(
    State(state): State<AppState>,
    Query(params): Query<ThroughputQuery>,
) -> Result<Json<Vec<ThroughputRow>>, ApiError> {
    let limit = params.limit.unwrap_or(24).min(168);

    let kind_clause = match params.kind {
        Some(kind) => format!("AND kind = {}", kind),
        None => String::new(),
    };

    let rows: Vec<ThroughputRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(toStartOfHour(created_at)) AS hour,
                count() AS event_count,
                uniq(pubkey) AS unique_pubkeys
            FROM events_local
            WHERE created_at >= now() - INTERVAL {} HOUR
            {}
            GROUP BY hour
            ORDER BY hour DESC
            LIMIT {}",
            limit, kind_clause, limit
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

// ═══════════════════════════════════════════════════════════════════════════
// User Retention
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for retention cohorts.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionQuery {
    /// Start date for first cohort (YYYY-MM-DD). Defaults to 12 weeks ago.
    pub cohort_start: Option<NaiveDate>,
    /// Cohort size: "week" (default) or "month".
    pub cohort_size: Option<String>,
    /// Number of cohorts to return (default: 12, max: 52).
    pub limit: Option<u32>,
}

/// A single cohort's retention data.
#[derive(Debug, Clone, Serialize)]
pub struct CohortRetention {
    /// Cohort period start (YYYY-MM-DD).
    pub cohort: String,
    /// Number of users who joined in this cohort.
    pub cohort_size: u64,
    /// Retention for each subsequent period (index 0 = same period, 1 = next period, etc.).
    /// Values are counts of active users from the cohort.
    pub retention: Vec<u64>,
    /// Retention as percentages (0.0 - 100.0).
    pub retention_pct: Vec<f64>,
}

#[derive(Debug, Clone, Deserialize, Row)]
struct CohortActivityRow {
    cohort: String,
    activity_period: String,
    active_count: u64,
}

/// `GET /api/v1/stats/users/retention`
///
/// Returns cohort retention analysis.
/// Requires the pubkey_first_seen materialized view (migration 002).
pub async fn user_retention(
    State(state): State<AppState>,
    Query(params): Query<RetentionQuery>,
) -> Result<Json<Vec<CohortRetention>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(52);
    let cohort_size = params.cohort_size.as_deref().unwrap_or("week");

    let (period_func, interval_unit) = match cohort_size {
        "month" => ("toStartOfMonth", "MONTH"),
        "week" => ("toMonday", "WEEK"),
        other => {
            return Err(ApiError::BadRequest(format!(
                "invalid cohort_size value: '{}'. Valid options: week, month",
                other
            )));
        }
    };

    let cohort_start_clause = match params.cohort_start {
        Some(date) => format!("AND first_seen >= '{}'", date),
        None => format!("AND first_seen >= now() - INTERVAL {} {}", limit, interval_unit),
    };

    // Query: for each cohort (users grouped by first_seen period),
    // count how many were active in each subsequent period
    let rows: Vec<CohortActivityRow> = state
        .clickhouse
        .query(&format!(
            "WITH cohort_users AS (
                SELECT
                    pubkey,
                    {}(first_seen) AS cohort
                FROM pubkey_first_seen
                WHERE 1=1
                {}
            )
            SELECT
                toString(cu.cohort) AS cohort,
                toString({}(e.created_at)) AS activity_period,
                uniq(e.pubkey) AS active_count
            FROM events_local e
            INNER JOIN cohort_users cu ON e.pubkey = cu.pubkey
            WHERE e.kind NOT IN (445, 1059)
                AND e.created_at >= cu.cohort
            GROUP BY cu.cohort, activity_period
            ORDER BY cu.cohort, activity_period",
            period_func, cohort_start_clause, period_func
        ))
        .fetch_all()
        .await?;

    // Process rows into cohort retention structure
    let mut cohorts: std::collections::BTreeMap<String, Vec<(String, u64)>> =
        std::collections::BTreeMap::new();
    for row in rows {
        cohorts
            .entry(row.cohort.clone())
            .or_default()
            .push((row.activity_period, row.active_count));
    }

    let result: Vec<CohortRetention> = cohorts
        .into_iter()
        .take(limit as usize)
        .map(|(cohort, mut periods)| {
            periods.sort_by(|a, b| a.0.cmp(&b.0));
            let cohort_size = periods.first().map(|(_, c)| *c).unwrap_or(0);
            let retention: Vec<u64> = periods.iter().map(|(_, c)| *c).collect();
            let retention_pct: Vec<f64> = retention
                .iter()
                .map(|&c| {
                    if cohort_size > 0 {
                        (c as f64 / cohort_size as f64) * 100.0
                    } else {
                        0.0
                    }
                })
                .collect();
            CohortRetention {
                cohort,
                cohort_size,
                retention,
                retention_pct,
            }
        })
        .collect();

    Ok(Json(result))
}

// ═══════════════════════════════════════════════════════════════════════════
// New Users
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for new users endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct NewUsersQuery {
    /// Group by: "day" (default), "week", "month".
    pub group_by: Option<String>,
    /// Number of periods to return (default: 30).
    pub limit: Option<u32>,
    /// Only include data from this date onwards (YYYY-MM-DD).
    pub since: Option<NaiveDate>,
}

/// New users time series row.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct NewUsersRow {
    /// Period as string (YYYY-MM-DD).
    pub period: String,
    /// Number of pubkeys first seen in this period.
    pub new_users: u64,
}

/// `GET /api/v1/stats/users/new`
///
/// Returns count of new pubkeys (first seen) per period.
/// Requires the pubkey_first_seen materialized view (migration 002).
pub async fn new_users(
    State(state): State<AppState>,
    Query(params): Query<NewUsersQuery>,
) -> Result<Json<Vec<NewUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365);

    let (group_expr, max_limit) = match params.group_by.as_deref() {
        Some("week") => ("toMonday(first_seen)", 52u32),
        Some("month") => ("toStartOfMonth(first_seen)", 120u32),
        Some("day") | None => ("toDate(first_seen)", 365u32),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "invalid group_by value: '{}'. Valid options: day, week, month",
                other
            )));
        }
    };

    let limit = limit.min(max_limit);

    let since_clause = match params.since {
        Some(date) => format!("WHERE first_seen >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<NewUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString({}) AS period,
                count() AS new_users
            FROM pubkey_first_seen
            {}
            GROUP BY period
            ORDER BY period DESC
            LIMIT {}",
            group_expr, since_clause, limit
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

// ═══════════════════════════════════════════════════════════════════════════
// Hourly Activity Pattern
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for hourly activity.
#[derive(Debug, Clone, Deserialize)]
pub struct HourlyActivityQuery {
    /// Number of days to aggregate over (default: 7, max: 90).
    pub days: Option<u32>,
    /// Filter by event kind.
    pub kind: Option<u16>,
}

/// Hourly activity row (hour of day pattern).
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct HourlyActivityRow {
    /// Hour of day (0-23).
    pub hour: u8,
    pub event_count: u64,
    pub unique_pubkeys: u64,
    /// Average events per day for this hour.
    pub avg_per_day: f64,
}

/// `GET /api/v1/stats/activity/hourly`
///
/// Returns event activity grouped by hour of day (0-23) to show usage patterns.
pub async fn hourly_activity(
    State(state): State<AppState>,
    Query(params): Query<HourlyActivityQuery>,
) -> Result<Json<Vec<HourlyActivityRow>>, ApiError> {
    let days = params.days.unwrap_or(7).min(90);

    let kind_clause = match params.kind {
        Some(kind) => format!("AND kind = {}", kind),
        None => String::new(),
    };

    let rows: Vec<HourlyActivityRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toHour(created_at) AS hour,
                count() AS event_count,
                uniq(pubkey) AS unique_pubkeys,
                toFloat64(count()) / {} AS avg_per_day
            FROM events_local
            WHERE created_at >= now() - INTERVAL {} DAY
            {}
            GROUP BY hour
            ORDER BY hour ASC",
            days, days, kind_clause
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

// ═══════════════════════════════════════════════════════════════════════════
// Zap Statistics
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for zap stats.
#[derive(Debug, Clone, Deserialize)]
pub struct ZapStatsQuery {
    /// Number of days to include (default: 30).
    pub days: Option<u32>,
    /// Group by: "day", "week", "month". If omitted, returns aggregate.
    pub group_by: Option<String>,
    /// Number of periods to return when grouping (default: 30).
    pub limit: Option<u32>,
}

/// Aggregate zap statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct ZapStatsAggregate {
    pub total_zaps: u64,
    pub total_sats: u64,
    pub unique_senders: u64,
    pub unique_recipients: u64,
    pub avg_zap_sats: f64,
}

/// Zap statistics grouped by time period.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct ZapStatsByPeriod {
    pub period: String,
    pub total_zaps: u64,
    pub total_sats: u64,
    pub unique_senders: u64,
    pub unique_recipients: u64,
    pub avg_zap_sats: f64,
}

/// `GET /api/v1/stats/zaps`
///
/// Returns zap statistics (requires migration 003_zap_amounts).
pub async fn zap_stats(
    State(state): State<AppState>,
    Query(params): Query<ZapStatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let days = params.days.unwrap_or(30);
    let limit = params.limit.unwrap_or(30).min(365);

    match params.group_by.as_deref() {
        Some("day") => {
            let rows: Vec<ZapStatsByPeriod> = state
                .clickhouse
                .query(&format!(
                    "SELECT
                        toString(toDate(created_at)) AS period,
                        count() AS total_zaps,
                        toUInt64(sum(amount_msats) / 1000) AS total_sats,
                        uniq(sender_pubkey) AS unique_senders,
                        uniq(recipient_pubkey) AS unique_recipients,
                        avg(amount_msats) / 1000 AS avg_zap_sats
                    FROM zap_amounts_data
                    WHERE created_at >= now() - INTERVAL {} DAY
                        AND amount_msats > 0
                    GROUP BY period
                    ORDER BY period DESC
                    LIMIT {}",
                    days, limit
                ))
                .fetch_all()
                .await?;
            Ok(Json(serde_json::to_value(rows)?))
        }
        Some("week") => {
            let rows: Vec<ZapStatsByPeriod> = state
                .clickhouse
                .query(&format!(
                    "SELECT
                        toString(toMonday(created_at)) AS period,
                        count() AS total_zaps,
                        toUInt64(sum(amount_msats) / 1000) AS total_sats,
                        uniq(sender_pubkey) AS unique_senders,
                        uniq(recipient_pubkey) AS unique_recipients,
                        avg(amount_msats) / 1000 AS avg_zap_sats
                    FROM zap_amounts_data
                    WHERE created_at >= now() - INTERVAL {} DAY
                        AND amount_msats > 0
                    GROUP BY period
                    ORDER BY period DESC
                    LIMIT {}",
                    days, limit
                ))
                .fetch_all()
                .await?;
            Ok(Json(serde_json::to_value(rows)?))
        }
        Some("month") => {
            let rows: Vec<ZapStatsByPeriod> = state
                .clickhouse
                .query(&format!(
                    "SELECT
                        toString(toStartOfMonth(created_at)) AS period,
                        count() AS total_zaps,
                        toUInt64(sum(amount_msats) / 1000) AS total_sats,
                        uniq(sender_pubkey) AS unique_senders,
                        uniq(recipient_pubkey) AS unique_recipients,
                        avg(amount_msats) / 1000 AS avg_zap_sats
                    FROM zap_amounts_data
                    WHERE created_at >= now() - INTERVAL {} DAY
                        AND amount_msats > 0
                    GROUP BY period
                    ORDER BY period DESC
                    LIMIT {}",
                    days, limit
                ))
                .fetch_all()
                .await?;
            Ok(Json(serde_json::to_value(rows)?))
        }
        Some(other) => Err(ApiError::BadRequest(format!(
            "invalid group_by value: '{}'. Valid options: day, week, month",
            other
        ))),
        None => {
            // Return aggregate stats
            let stats: ZapStatsAggregate = state
                .clickhouse
                .query(&format!(
                    "SELECT
                        count() AS total_zaps,
                        toUInt64(sum(amount_msats) / 1000) AS total_sats,
                        uniq(sender_pubkey) AS unique_senders,
                        uniq(recipient_pubkey) AS unique_recipients,
                        avg(amount_msats) / 1000 AS avg_zap_sats
                    FROM zap_amounts_data
                    WHERE created_at >= now() - INTERVAL {} DAY
                        AND amount_msats > 0",
                    days
                ))
                .fetch_one()
                .await?;
            Ok(Json(serde_json::to_value(stats)?))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Engagement Stats
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for engagement stats.
#[derive(Debug, Clone, Deserialize)]
pub struct EngagementQuery {
    /// Number of days to include (default: 30).
    pub days: Option<u32>,
}

/// Engagement statistics (replies and reactions relative to original notes).
#[derive(Debug, Clone, Serialize)]
pub struct EngagementStats {
    /// Total kind 1 events (notes and replies).
    pub total_notes: u64,
    /// Kind 1 events that are replies (have e-tag).
    pub total_replies: u64,
    /// Kind 7 events (reactions).
    pub total_reactions: u64,
    /// Replies per 100 notes.
    pub replies_per_100_notes: f64,
    /// Reactions per 100 notes.
    pub reactions_per_100_notes: f64,
    /// Combined engagement per 100 notes.
    pub engagement_per_100_notes: f64,
}

#[derive(Debug, Clone, Deserialize, Row)]
struct EngagementRow {
    total_notes: u64,
    total_replies: u64,
    total_reactions: u64,
}

/// `GET /api/v1/stats/engagement`
///
/// Returns reply and reaction ratios relative to original notes.
pub async fn engagement(
    State(state): State<AppState>,
    Query(params): Query<EngagementQuery>,
) -> Result<Json<EngagementStats>, ApiError> {
    let days = params.days.unwrap_or(30);

    let row: EngagementRow = state
        .clickhouse
        .query(&format!(
            "SELECT
                countIf(kind = 1) AS total_notes,
                -- Replies are kind 1 events with an e-tag, created within the time window
                -- Use uniq(event_id) since one event may have multiple e-tags
                (SELECT uniq(event_id) FROM event_tags_flat
                 WHERE kind = 1
                   AND tag_name = 'e'
                   AND created_at >= now() - INTERVAL {} DAY
                ) AS total_replies,
                countIf(kind = 7) AS total_reactions
            FROM events_local
            WHERE created_at >= now() - INTERVAL {} DAY",
            days, days
        ))
        .fetch_one()
        .await?;

    let original_notes = row.total_notes.saturating_sub(row.total_replies);
    let base = if original_notes > 0 {
        original_notes as f64
    } else {
        1.0
    };

    Ok(Json(EngagementStats {
        total_notes: row.total_notes,
        total_replies: row.total_replies,
        total_reactions: row.total_reactions,
        replies_per_100_notes: (row.total_replies as f64 / base) * 100.0,
        reactions_per_100_notes: (row.total_reactions as f64 / base) * 100.0,
        engagement_per_100_notes: ((row.total_replies + row.total_reactions) as f64 / base) * 100.0,
    }))
}

// ═══════════════════════════════════════════════════════════════════════════
// Long-form Content Stats
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for long-form stats.
#[derive(Debug, Clone, Deserialize)]
pub struct LongformQuery {
    /// Number of days to include (default: all time if omitted).
    pub days: Option<u32>,
}

/// Long-form content statistics (kind 30023).
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct LongformStats {
    pub articles_count: u64,
    pub unique_authors: u64,
    pub avg_content_length: f64,
    pub total_content_length: u64,
    /// Estimated word count (content_length / 5).
    pub estimated_total_words: u64,
}

/// `GET /api/v1/stats/longform`
///
/// Returns statistics for long-form content (kind 30023).
pub async fn longform(
    State(state): State<AppState>,
    Query(params): Query<LongformQuery>,
) -> Result<Json<LongformStats>, ApiError> {
    let days_clause = match params.days {
        Some(days) => format!("AND created_at >= now() - INTERVAL {} DAY", days),
        None => String::new(),
    };

    let stats: LongformStats = state
        .clickhouse
        .query(&format!(
            "SELECT
                count() AS articles_count,
                uniq(pubkey) AS unique_authors,
                avg(length(content)) AS avg_content_length,
                sum(length(content)) AS total_content_length,
                toUInt64(sum(length(content)) / 5) AS estimated_total_words
            FROM events_local
            WHERE kind = 30023
            {}",
            days_clause
        ))
        .fetch_one()
        .await?;

    Ok(Json(stats))
}

// ═══════════════════════════════════════════════════════════════════════════
// Top Publishers
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for top publishers.
#[derive(Debug, Clone, Deserialize)]
pub struct PublishersQuery {
    /// Filter by event kind.
    pub kind: Option<u16>,
    /// Number of days to include (default: 30).
    pub days: Option<u32>,
    /// Number of publishers to return (default: 100, max: 1000).
    pub limit: Option<u32>,
}

/// Publisher statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct PublisherRow {
    pub pubkey: String,
    pub event_count: u64,
    pub kinds_count: u64,
    pub first_event: u32,
    pub last_event: u32,
}

/// `GET /api/v1/stats/publishers`
///
/// Returns top publishers by event count.
pub async fn publishers(
    State(state): State<AppState>,
    Query(params): Query<PublishersQuery>,
) -> Result<Json<Vec<PublisherRow>>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);
    let days = params.days.unwrap_or(30);

    let kind_clause = match params.kind {
        Some(kind) => format!("AND kind = {}", kind),
        None => String::new(),
    };

    let rows: Vec<PublisherRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                pubkey,
                count() AS event_count,
                uniq(kind) AS kinds_count,
                toUInt32(min(created_at)) AS first_event,
                toUInt32(max(created_at)) AS last_event
            FROM events_local
            WHERE created_at >= now() - INTERVAL {} DAY
            {}
            GROUP BY pubkey
            ORDER BY event_count DESC
            LIMIT {}",
            days, kind_clause, limit
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
}

