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
///
/// Uses system tables and pre-aggregated data for performance:
/// - total_events: from system.parts (instant, approximate)
/// - total_pubkeys: from pubkey_first_seen_data (pre-aggregated)
/// - total_kinds: from a small recent sample (kinds don't change much)
/// - earliest/latest: from pubkey_first_seen view and recent events
pub async fn overview(State(state): State<AppState>) -> Result<Json<OverviewResponse>, ApiError> {
    let stats: OverviewResponse = state
        .clickhouse
        .query(
            "SELECT
                -- Approximate total events from system.parts (instant)
                toUInt64((SELECT sum(rows) FROM system.parts
                 WHERE database = currentDatabase() AND table = 'events_local' AND active)) AS total_events,
                -- Total unique pubkeys from pre-aggregated table
                toUInt64((SELECT count() FROM pubkey_first_seen_data)) AS total_pubkeys,
                -- Distinct kinds (scan last 7 days - kinds are stable)
                toUInt64((SELECT uniq(kind) FROM events_local
                 WHERE created_at >= now() - INTERVAL 7 DAY)) AS total_kinds,
                -- Earliest event from aggregated first-seen data (via helper view)
                -- Floor at 2020-11-01 (Nostr genesis) to exclude bogus backdated events
                toUInt32((SELECT min(first_seen) FROM pubkey_first_seen
                          WHERE first_seen >= toDateTime('2020-11-01 00:00:00'))) AS earliest_event,
                -- Latest event from recent data (exclude future timestamps)
                toUInt32((SELECT max(created_at) FROM events_local
                          WHERE created_at >= now() - INTERVAL 1 HOUR
                            AND created_at <= now())) AS latest_event",
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
/// Queries pre-aggregated summary tables for instant performance.
pub async fn active_users_summary(
    State(state): State<AppState>,
) -> Result<Json<ActiveUsersSummary>, ApiError> {
    // Daily - get most recent day from pre-aggregated summary
    let daily: ActiveUsersCount = state
        .clickhouse
        .query(
            "SELECT
                active_users,
                has_profile,
                has_follows_list,
                has_profile_and_follows_list,
                total_events
            FROM daily_active_users
            ORDER BY date DESC
            LIMIT 1",
        )
        .fetch_one()
        .await?;

    // Weekly - get most recent week from pre-aggregated summary
    let weekly: ActiveUsersCount = state
        .clickhouse
        .query(
            "SELECT
                active_users,
                has_profile,
                has_follows_list,
                has_profile_and_follows_list,
                total_events
            FROM weekly_active_users
            ORDER BY week DESC
            LIMIT 1",
        )
        .fetch_one()
        .await?;

    // Monthly - get most recent month from pre-aggregated summary
    let monthly: ActiveUsersCount = state
        .clickhouse
        .query(
            "SELECT
                active_users,
                has_profile,
                has_follows_list,
                has_profile_and_follows_list,
                total_events
            FROM monthly_active_users
            ORDER BY month DESC
            LIMIT 1",
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
/// Queries pre-aggregated summary tables for instant performance.
pub async fn active_users_daily(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365);

    let since_clause = match params.since {
        Some(date) => format!("AND date >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<ActiveUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(date) AS period,
                active_users,
                has_profile,
                has_follows_list,
                has_profile_and_follows_list,
                total_events
            FROM daily_active_users
            WHERE 1=1
            {}
            ORDER BY date DESC
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
/// Queries pre-aggregated summary tables for instant performance.
pub async fn active_users_weekly(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(52);

    let since_clause = match params.since {
        Some(date) => format!("AND week >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<ActiveUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(week) AS period,
                active_users,
                has_profile,
                has_follows_list,
                has_profile_and_follows_list,
                total_events
            FROM weekly_active_users
            WHERE 1=1
            {}
            ORDER BY week DESC
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
/// Queries pre-aggregated summary tables for instant performance.
pub async fn active_users_monthly(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(120);

    let since_clause = match params.since {
        Some(date) => format!("AND month >= '{}'", date),
        None => String::new(),
    };

    let rows: Vec<ActiveUsersRow> = state
        .clickhouse
        .query(&format!(
            "SELECT
                toString(month) AS period,
                active_users,
                has_profile,
                has_follows_list,
                has_profile_and_follows_list,
                total_events
            FROM monthly_active_users
            WHERE 1=1
            {}
            ORDER BY month DESC
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
    /// Filter by event kind.
    pub kind: Option<u16>,
}

/// Row type for throughput query.
#[derive(Debug, Clone, Deserialize, Row)]
struct ThroughputRow {
    events_per_hour: f64,
    total_events_7d: u64,
}

/// Throughput response: 7-day rolling average of events per hour.
#[derive(Debug, Clone, Serialize)]
pub struct ThroughputResponse {
    /// Average events per hour over the last 7 days.
    pub events_per_hour: f64,
    /// Total events in the 7-day window.
    pub total_events_7d: u64,
}

/// `GET /api/v1/stats/throughput`
///
/// Returns 7-day rolling average of events created per hour.
pub async fn throughput(
    State(state): State<AppState>,
    Query(params): Query<ThroughputQuery>,
) -> Result<Json<ThroughputResponse>, ApiError> {
    let kind_clause = match params.kind {
        Some(kind) => format!("AND kind = {}", kind),
        None => String::new(),
    };

    let row: ThroughputRow = state
        .clickhouse
        .query(&format!(
            "SELECT
                toFloat64(count()) / 168.0 AS events_per_hour,
                count() AS total_events_7d
            FROM events_local
            WHERE created_at >= now() - INTERVAL 7 DAY
            {}",
            kind_clause
        ))
        .fetch_one()
        .await?;

    Ok(Json(ThroughputResponse {
        events_per_hour: row.events_per_hour,
        total_events_7d: row.total_events_7d,
    }))
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

/// Query parameters for zap histogram.
#[derive(Debug, Clone, Deserialize)]
pub struct ZapHistogramQuery {
    /// Number of days to include (default: 30).
    pub days: Option<u32>,
}

/// A bucket in the zap amount histogram.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct ZapHistogramBucket {
    /// Human-readable label for this bucket (e.g., "21-100 sats").
    pub bucket: String,
    /// Minimum sats value (inclusive) for this bucket.
    pub min_sats: u64,
    /// Maximum sats value (inclusive) for this bucket.
    pub max_sats: u64,
    /// Number of zaps in this bucket.
    pub count: u64,
    /// Total sats in this bucket.
    pub total_sats: u64,
    /// Percentage of total zaps.
    pub pct_count: f64,
    /// Percentage of total sats.
    pub pct_sats: f64,
}

/// `GET /api/v1/stats/zaps/histogram`
///
/// Returns a histogram of zap amounts grouped into meaningful buckets.
/// Useful for understanding the distribution of zap sizes.
///
/// Buckets (17 total):
/// - 1-10 sats, 11-21 sats (micro zaps)
/// - 22-50 sats, 51-100 sats (small tips)
/// - 101-250 sats, 251-500 sats (medium zaps)
/// - 501-750 sats, 751-1K sats (larger tips)
/// - 1K-2.5K sats, 2.5K-5K sats (generous zaps)
/// - 5K-7.5K sats, 7.5K-10K sats (big zaps)
/// - 10K-25K sats, 25K-50K sats (very large)
/// - 50K-75K sats, 75K-100K sats (whale zaps)
/// - 100K+ sats (mega zaps)
pub async fn zap_histogram(
    State(state): State<AppState>,
    Query(params): Query<ZapHistogramQuery>,
) -> Result<Json<Vec<ZapHistogramBucket>>, ApiError> {
    let days = params.days.unwrap_or(30);

    // Use ClickHouse's multiIf to bucket amounts, then aggregate
    // 17 buckets for granular distribution analysis
    let rows: Vec<ZapHistogramBucket> = state
        .clickhouse
        .query(&format!(
            "WITH
                total_zaps AS (SELECT count() AS cnt FROM zap_amounts_data WHERE created_at >= now() - INTERVAL {days} DAY AND amount_msats > 0),
                total_amount AS (SELECT sum(amount_msats) / 1000 AS sats FROM zap_amounts_data WHERE created_at >= now() - INTERVAL {days} DAY AND amount_msats > 0)
            SELECT
                multiIf(
                    amount_msats / 1000 <= 10, '1-10 sats',
                    amount_msats / 1000 <= 21, '11-21 sats',
                    amount_msats / 1000 <= 50, '22-50 sats',
                    amount_msats / 1000 <= 100, '51-100 sats',
                    amount_msats / 1000 <= 250, '101-250 sats',
                    amount_msats / 1000 <= 500, '251-500 sats',
                    amount_msats / 1000 <= 750, '501-750 sats',
                    amount_msats / 1000 <= 1000, '751-1K sats',
                    amount_msats / 1000 <= 2500, '1K-2.5K sats',
                    amount_msats / 1000 <= 5000, '2.5K-5K sats',
                    amount_msats / 1000 <= 7500, '5K-7.5K sats',
                    amount_msats / 1000 <= 10000, '7.5K-10K sats',
                    amount_msats / 1000 <= 25000, '10K-25K sats',
                    amount_msats / 1000 <= 50000, '25K-50K sats',
                    amount_msats / 1000 <= 75000, '50K-75K sats',
                    amount_msats / 1000 <= 100000, '75K-100K sats',
                    '100K+ sats'
                ) AS bucket,
                multiIf(
                    amount_msats / 1000 <= 10, toUInt64(1),
                    amount_msats / 1000 <= 21, toUInt64(11),
                    amount_msats / 1000 <= 50, toUInt64(22),
                    amount_msats / 1000 <= 100, toUInt64(51),
                    amount_msats / 1000 <= 250, toUInt64(101),
                    amount_msats / 1000 <= 500, toUInt64(251),
                    amount_msats / 1000 <= 750, toUInt64(501),
                    amount_msats / 1000 <= 1000, toUInt64(751),
                    amount_msats / 1000 <= 2500, toUInt64(1001),
                    amount_msats / 1000 <= 5000, toUInt64(2501),
                    amount_msats / 1000 <= 7500, toUInt64(5001),
                    amount_msats / 1000 <= 10000, toUInt64(7501),
                    amount_msats / 1000 <= 25000, toUInt64(10001),
                    amount_msats / 1000 <= 50000, toUInt64(25001),
                    amount_msats / 1000 <= 75000, toUInt64(50001),
                    amount_msats / 1000 <= 100000, toUInt64(75001),
                    toUInt64(100001)
                ) AS min_sats,
                multiIf(
                    amount_msats / 1000 <= 10, toUInt64(10),
                    amount_msats / 1000 <= 21, toUInt64(21),
                    amount_msats / 1000 <= 50, toUInt64(50),
                    amount_msats / 1000 <= 100, toUInt64(100),
                    amount_msats / 1000 <= 250, toUInt64(250),
                    amount_msats / 1000 <= 500, toUInt64(500),
                    amount_msats / 1000 <= 750, toUInt64(750),
                    amount_msats / 1000 <= 1000, toUInt64(1000),
                    amount_msats / 1000 <= 2500, toUInt64(2500),
                    amount_msats / 1000 <= 5000, toUInt64(5000),
                    amount_msats / 1000 <= 7500, toUInt64(7500),
                    amount_msats / 1000 <= 10000, toUInt64(10000),
                    amount_msats / 1000 <= 25000, toUInt64(25000),
                    amount_msats / 1000 <= 50000, toUInt64(50000),
                    amount_msats / 1000 <= 75000, toUInt64(75000),
                    amount_msats / 1000 <= 100000, toUInt64(100000),
                    toUInt64(999999999)
                ) AS max_sats,
                count() AS count,
                toUInt64(sum(amount_msats) / 1000) AS total_sats,
                ifNull(round(100.0 * count() / nullIf((SELECT cnt FROM total_zaps), 0), 2), 0.0) AS pct_count,
                ifNull(round(100.0 * (sum(amount_msats) / 1000) / nullIf((SELECT sats FROM total_amount), 0), 2), 0.0) AS pct_sats
            FROM zap_amounts_data
            WHERE created_at >= now() - INTERVAL {days} DAY
                AND amount_msats > 0
            GROUP BY bucket, min_sats, max_sats
            ORDER BY min_sats ASC",
            days = days
        ))
        .fetch_all()
        .await?;

    Ok(Json(rows))
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
    /// Number of days included in the calculation.
    pub period_days: u32,
    /// Kind 1 events that are NOT replies (original posts).
    pub original_notes: u64,
    /// Kind 1 events that are replies (have e-tag).
    pub replies: u64,
    /// Kind 7 events (reactions/likes).
    pub reactions: u64,
    /// Average replies per original note.
    pub replies_per_note: f64,
    /// Average reactions per original note.
    pub reactions_per_note: f64,
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

    // Calculate all metrics from events_local consistently.
    // A reply is a kind=1 event that has at least one e-tag (references another event).
    let row: EngagementRow = state
        .clickhouse
        .query(&format!(
            "SELECT
                countIf(kind = 1) AS total_notes,
                countIf(kind = 1 AND arrayExists(t -> t[1] = 'e', tags)) AS total_replies,
                countIf(kind = 7) AS total_reactions
            FROM events_local
            WHERE created_at >= now() - INTERVAL {} DAY",
            days
        ))
        .fetch_one()
        .await?;

    // Original notes = total kind=1 events minus replies
    let original_notes = row.total_notes.saturating_sub(row.total_replies);
    let base = if original_notes > 0 {
        original_notes as f64
    } else {
        1.0
    };

    Ok(Json(EngagementStats {
        period_days: days,
        original_notes,
        replies: row.total_replies,
        reactions: row.total_reactions,
        replies_per_note: row.total_replies as f64 / base,
        reactions_per_note: row.total_reactions as f64 / base,
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

