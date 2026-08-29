//! Stats endpoints for aggregate analytics.

use axum::Json;
use axum::extract::{Query, State};
use chrono::NaiveDate;
use clickhouse::Row;
use pensieve_core::{LatestEventWatermark, NOSTR_GENESIS_TIMESTAMP, read_latest_event_watermark};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache::{get_or_compute, get_or_compute_with_ttl, ttl};
use crate::error::ApiError;
use crate::state::{AnalyticsFamily, AppState};

// ═══════════════════════════════════════════════════════════════════════════
// Overview
// ═══════════════════════════════════════════════════════════════════════════

/// High-level stats overview (combined endpoint for convenience).
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
/// Returns high-level overview statistics (combined endpoint).
/// Cached for 1 minute.
/// For granular queries with independent caching, use the individual endpoints:
/// - GET /api/v1/stats/events/total
/// - GET /api/v1/stats/pubkeys/total
/// - GET /api/v1/stats/kinds/total
/// - GET /api/v1/stats/events/earliest
/// - GET /api/v1/stats/events/latest
pub async fn overview(State(state): State<AppState>) -> Result<Json<OverviewResponse>, ApiError> {
    let result = get_or_compute_with_ttl(&state.cache, "overview", ttl::OVERVIEW, || async {
        fetch_overview(&state).await
    })
    .await?;

    Ok(Json(result))
}

// ─────────────────────────────────────────────────────────────────────────────
// Granular stat endpoints (for dashboards with independent caching)
// ─────────────────────────────────────────────────────────────────────────────

/// Response for a single count value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountResponse {
    pub count: u64,
}

/// Response for a single timestamp value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampResponse {
    pub timestamp: u32,
}

/// `GET /api/v1/stats/events/total`
///
/// Returns approximate total event count (from system.parts, instant).
/// Cached for 5 minutes.
pub async fn total_events(State(state): State<AppState>) -> Result<Json<CountResponse>, ApiError> {
    let result = get_or_compute(&state.cache, "total_events", || async {
        let count = fetch_total_events(&state).await?;
        Ok(CountResponse { count })
    })
    .await?;

    Ok(Json(result))
}

/// `GET /api/v1/stats/pubkeys/total`
///
/// Returns total unique pubkeys (from pre-aggregated table, instant).
/// Cached for 5 minutes.
pub async fn total_pubkeys(State(state): State<AppState>) -> Result<Json<CountResponse>, ApiError> {
    let result = get_or_compute(&state.cache, "total_pubkeys", || async {
        let count = fetch_total_pubkeys(&state).await?;
        Ok(CountResponse { count })
    })
    .await?;

    Ok(Json(result))
}

/// `GET /api/v1/stats/kinds/total`
///
/// Returns distinct event kinds seen in the last 30 days.
/// Cached for 1 hour (kinds are stable).
pub async fn total_kinds(State(state): State<AppState>) -> Result<Json<CountResponse>, ApiError> {
    let result = get_or_compute_with_ttl(&state.cache, "total_kinds", ttl::STABLE, || async {
        let count = fetch_total_kinds(&state).await?;
        Ok(CountResponse { count })
    })
    .await?;

    Ok(Json(result))
}

/// `GET /api/v1/stats/events/earliest`
///
/// Returns earliest event timestamp (from aggregated first-seen data).
/// Cached for 1 hour (rarely changes).
pub async fn earliest_event(
    State(state): State<AppState>,
) -> Result<Json<TimestampResponse>, ApiError> {
    let result = get_or_compute_with_ttl(&state.cache, "earliest_event", ttl::STABLE, || async {
        let timestamp = fetch_earliest_event(&state).await?;
        Ok(TimestampResponse { timestamp })
    })
    .await?;

    Ok(Json(result))
}

/// `GET /api/v1/stats/events/latest`
///
/// Returns latest event timestamp (from recent data, excludes future timestamps).
/// Cached for 10 seconds.
pub async fn latest_event(
    State(state): State<AppState>,
) -> Result<Json<TimestampResponse>, ApiError> {
    let result = get_or_compute_with_ttl(&state.cache, "latest_event", ttl::REALTIME, || async {
        let timestamp = fetch_latest_event(&state).await?;
        Ok(TimestampResponse { timestamp })
    })
    .await?;

    Ok(Json(result))
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal fetch functions (reused by both combined and granular endpoints)
// ─────────────────────────────────────────────────────────────────────────────

async fn fetch_overview(state: &AppState) -> Result<OverviewResponse, ApiError> {
    if state.uses_postgres(AnalyticsFamily::Overview) {
        let client = state
            .postgres_client(AnalyticsFamily::Overview)
            .await
            .map_err(ApiError::Internal)?;
        let row = client
            .query_one(
                "SELECT total_events,total_pubkeys,kinds_30d,earliest_event
                   FROM pensieve_analytics.current_overview",
                &[],
            )
            .await
            .map_err(|error| ApiError::Internal(error.into()))?;
        return Ok(OverviewResponse {
            total_events: nonnegative_u64(row.get(0), "total_events")?,
            total_pubkeys: nonnegative_u64(row.get(1), "total_pubkeys")?,
            total_kinds: nonnegative_u64(row.get(2), "total_kinds")?,
            earliest_event: api_timestamp(row.get(3), "earliest_event")?
                .max(NOSTR_GENESIS_TIMESTAMP),
            latest_event: read_fresh_watermark(state)?,
        });
    }
    let (total_events, total_pubkeys, total_kinds, earliest_event, latest_event) = tokio::join!(
        fetch_total_events(state),
        fetch_total_pubkeys(state),
        fetch_total_kinds(state),
        fetch_earliest_event(state),
        fetch_latest_event(state),
    );
    Ok(OverviewResponse {
        total_events: total_events?,
        total_pubkeys: total_pubkeys?,
        total_kinds: total_kinds?,
        earliest_event: earliest_event?,
        latest_event: latest_event?,
    })
}

async fn fetch_total_events(state: &AppState) -> Result<u64, ApiError> {
    if state.uses_postgres(AnalyticsFamily::Overview) {
        return fetch_postgres_overview_metric(state, "total_events").await;
    }
    Ok(state
        .clickhouse
        .query("SELECT sum(rows) FROM system.parts WHERE database = currentDatabase() AND table = 'events_local' AND active")
        .fetch_one::<u64>()
        .await?)
}

async fn fetch_total_pubkeys(state: &AppState) -> Result<u64, ApiError> {
    if state.uses_postgres(AnalyticsFamily::Overview) {
        return fetch_postgres_overview_metric(state, "total_pubkeys").await;
    }
    Ok(state
        .clickhouse
        .query("SELECT count() FROM pubkey_first_seen_data")
        .fetch_one::<u64>()
        .await?)
}

async fn fetch_total_kinds(state: &AppState) -> Result<u64, ApiError> {
    if state.uses_postgres(AnalyticsFamily::Overview) {
        return fetch_postgres_overview_metric(state, "kinds_30d").await;
    }
    Ok(state
        .clickhouse
        .query("SELECT uniq(kind) FROM events_local WHERE created_at >= now() - INTERVAL 30 DAY")
        .fetch_one::<u64>()
        .await?)
}

async fn fetch_earliest_event(state: &AppState) -> Result<u32, ApiError> {
    if state.uses_postgres(AnalyticsFamily::Overview) {
        let value = fetch_postgres_overview_metric(state, "earliest_event").await?;
        return Ok(u32::try_from(value)
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("earliest_event exceeds u32")))?
            .max(NOSTR_GENESIS_TIMESTAMP));
    }
    // Use min() aggregate, clamp to Nostr genesis for correctness
    let ts = state
        .clickhouse
        .query("SELECT toUInt32(min(created_at)) FROM events_local")
        .fetch_one::<u32>()
        .await?;
    // Return the later of: actual earliest event or Nostr genesis date
    Ok(ts.max(NOSTR_GENESIS_TIMESTAMP))
}

async fn fetch_latest_event(state: &AppState) -> Result<u32, ApiError> {
    if state.uses_postgres(AnalyticsFamily::Overview) {
        return read_fresh_watermark(state);
    }
    // Use max() aggregate, excluding future timestamps
    Ok(state
        .clickhouse
        .query("SELECT toUInt32(max(created_at)) FROM events_local WHERE created_at <= now()")
        .fetch_one::<u32>()
        .await?)
}

async fn fetch_postgres_overview_metric(
    state: &AppState,
    column: &'static str,
) -> Result<u64, ApiError> {
    let query = match column {
        "total_events" => "SELECT total_events FROM pensieve_analytics.current_overview",
        "total_pubkeys" => "SELECT total_pubkeys FROM pensieve_analytics.current_overview",
        "kinds_30d" => "SELECT kinds_30d FROM pensieve_analytics.current_overview",
        "earliest_event" => "SELECT earliest_event FROM pensieve_analytics.current_overview",
        _ => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "invalid overview metric"
            )));
        }
    };
    let client = state
        .postgres_client(AnalyticsFamily::Overview)
        .await
        .map_err(ApiError::Internal)?;
    let row = client
        .query_one(query, &[])
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    nonnegative_u64(row.get(0), column)
}

fn read_fresh_watermark(state: &AppState) -> Result<u32, ApiError> {
    let path = state
        .config
        .latest_event_watermark_path
        .as_ref()
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("latest-event watermark is missing")))?;
    let watermark = read_latest_event_watermark(path).map_err(|error| {
        ApiError::Internal(anyhow::anyhow!("read latest-event watermark: {error}"))
    })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ApiError::Internal(error.into()))?
        .as_secs();
    validate_watermark_freshness(
        &watermark,
        now,
        state.config.latest_event_watermark_max_age_secs,
    )
}

fn validate_watermark_freshness(
    watermark: &LatestEventWatermark,
    now: u64,
    maximum_age: u64,
) -> Result<u32, ApiError> {
    let age = now
        .checked_sub(watermark.published_at_epoch)
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!(
                "latest-event watermark publication is in the future"
            ))
        })?;
    if age > maximum_age {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "latest-event watermark is stale"
        )));
    }
    metrics::gauge!("api_latest_event_watermark_age_seconds").set(age as f64);
    watermark
        .max_eligible_created_at
        .map_or(Ok(0), |timestamp| {
            u32::try_from(timestamp).map_err(|_| {
                ApiError::Internal(anyhow::anyhow!("latest-event watermark exceeds u32"))
            })
        })
}

fn nonnegative_u64(value: i64, name: &'static str) -> Result<u64, ApiError> {
    u64::try_from(value).map_err(|_| ApiError::Internal(anyhow::anyhow!("{name} is negative")))
}

fn api_timestamp(value: i64, name: &'static str) -> Result<u32, ApiError> {
    u32::try_from(value)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("{name} is outside the API domain")))
}

#[cfg(test)]
mod postgres_analytics_tests {
    use pensieve_core::LatestEventWatermark;

    use super::{api_timestamp, nonnegative_u64, retention_response, validate_watermark_freshness};

    fn watermark() -> LatestEventWatermark {
        LatestEventWatermark {
            schema_version: 1,
            status: "published".to_owned(),
            max_sealed_segment_number: 7,
            max_eligible_created_at: Some(90),
            max_sealed_at_epoch: 100,
            published_at_epoch: 101,
        }
    }

    #[test]
    fn postgres_scalars_fail_closed_outside_the_api_domain() {
        assert_eq!(nonnegative_u64(7, "metric").unwrap(), 7);
        assert!(nonnegative_u64(-1, "metric").is_err());
        assert_eq!(api_timestamp(7, "timestamp").unwrap(), 7);
        assert!(api_timestamp(-1, "timestamp").is_err());
        assert!(api_timestamp(i64::from(u32::MAX) + 1, "timestamp").is_err());
    }

    #[test]
    fn watermark_freshness_rejects_stale_future_and_invalid_api_values() {
        assert_eq!(
            validate_watermark_freshness(&watermark(), 110, 10).unwrap(),
            90
        );
        assert!(validate_watermark_freshness(&watermark(), 112, 10).is_err());
        assert!(validate_watermark_freshness(&watermark(), 100, 10).is_err());
        let mut empty = watermark();
        empty.max_eligible_created_at = None;
        assert_eq!(validate_watermark_freshness(&empty, 101, 10).unwrap(), 0);
    }

    #[test]
    fn retention_adapter_uses_the_explicit_period_zero_denominator() {
        let response = retention_response(
            "2026-01-05".to_owned(),
            vec![
                ("2026-01-12".to_owned(), 4),
                ("2026-01-05".to_owned(), 10),
                ("2026-01-19".to_owned(), 2),
            ],
        );
        assert_eq!(response.cohort_size, 10);
        assert_eq!(response.retention, vec![4, 10, 2]);
        assert_eq!(response.retention_pct, vec![40.0, 100.0, 20.0]);

        let missing_period_zero =
            retention_response("2026-01-05".to_owned(), vec![("2026-01-12".to_owned(), 4)]);
        assert_eq!(missing_period_zero.cohort_size, 0);
        assert_eq!(missing_period_zero.retention_pct, vec![0.0]);
    }
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
///
/// **Performance notes:**
/// - When `kind` is not specified and `group_by` is set, uses pre-aggregated
///   `daily_user_stats` table for instant results.
/// - When `kind` is specified, queries `events_local` with projection optimization.
/// - Defaults to last 30 days if no time filter is provided.
///
/// Cached for 1 minute.
pub async fn events(
    State(state): State<AppState>,
    Query(params): Query<EventStatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);
    let kind = params.kind;
    let since = params.since;
    let until = params.until;
    let group_by = params.group_by.clone();

    // Default to 30 days if no time filter specified (prevents full table scans)
    let days = params.days.or_else(|| {
        if since.is_none() && until.is_none() {
            Some(30)
        } else {
            None
        }
    });

    // Validate group_by before caching
    if let Some(ref g) = group_by
        && !["day", "week", "month"].contains(&g.as_str())
    {
        return Err(ApiError::BadRequest(format!(
            "invalid group_by value: '{}'. Valid options: day, week, month",
            g
        )));
    }

    // Build cache key from all params
    let cache_key = format!(
        "events:kind={}&days={}&since={}&until={}&group_by={}&limit={}",
        kind.map(|k| k.to_string()).unwrap_or_default(),
        days.map(|d| d.to_string()).unwrap_or_default(),
        since.map(|d| d.to_string()).unwrap_or_default(),
        until.map(|d| d.to_string()).unwrap_or_default(),
        group_by.as_deref().unwrap_or(""),
        limit
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        // Use pre-aggregated daily_user_stats when possible (no kind filter)
        // This is MUCH faster than scanning events_local
        let use_preaggregated = kind.is_none() && group_by.is_some();

        if use_preaggregated {
            // Build date filter for daily_user_stats
            let mut date_conditions = vec!["date <= today()".to_string()];

            if let Some(days) = days {
                date_conditions.push(format!("date >= today() - INTERVAL {} DAY", days));
            } else {
                if let Some(since) = since {
                    date_conditions.push(format!("date >= '{}'", since));
                }
                if let Some(until) = until {
                    date_conditions.push(format!("date < '{}'", until));
                }
            }

            let date_where = format!("WHERE {}", date_conditions.join(" AND "));

            match group_by.as_deref() {
                Some("day") => {
                    let query = format!(
                        "SELECT
                            toString(date) AS period,
                            sum(event_count) AS count,
                            count() AS unique_pubkeys
                        FROM daily_user_stats FINAL
                        {}
                        GROUP BY date
                        ORDER BY date DESC
                        LIMIT {}",
                        date_where, limit
                    );
                    let rows: Vec<EventCountByPeriod> =
                        state.clickhouse.query(&query).fetch_all().await?;
                    return Ok(serde_json::to_value(rows)?);
                }
                Some("week") => {
                    let query = format!(
                        "SELECT
                            toString(toMonday(date)) AS period,
                            sum(event_count) AS count,
                            uniq(pubkey) AS unique_pubkeys
                        FROM daily_user_stats FINAL
                        {}
                        GROUP BY period
                        ORDER BY period DESC
                        LIMIT {}",
                        date_where, limit
                    );
                    let rows: Vec<EventCountByPeriod> =
                        state.clickhouse.query(&query).fetch_all().await?;
                    return Ok(serde_json::to_value(rows)?);
                }
                Some("month") => {
                    let query = format!(
                        "SELECT
                            toString(toStartOfMonth(date)) AS period,
                            sum(event_count) AS count,
                            uniq(pubkey) AS unique_pubkeys
                        FROM daily_user_stats FINAL
                        {}
                        GROUP BY period
                        ORDER BY period DESC
                        LIMIT {}",
                        date_where, limit
                    );
                    let rows: Vec<EventCountByPeriod> =
                        state.clickhouse.query(&query).fetch_all().await?;
                    return Ok(serde_json::to_value(rows)?);
                }
                _ => {} // Fall through to events_local
            }
        }

        // Fall back to events_local for kind-filtered queries or aggregates
        let mut conditions = Vec::new();

        // Always exclude future dates (malformed or malicious events)
        conditions.push("created_at <= now()".to_string());

        if let Some(kind) = kind {
            conditions.push(format!("kind = {}", kind));
        }

        if let Some(days) = days {
            conditions.push(format!("created_at >= now() - INTERVAL {} DAY", days));
        } else {
            if let Some(since) = since {
                conditions.push(format!("created_at >= '{}'", since));
            }
            if let Some(until) = until {
                conditions.push(format!("created_at < '{}'", until));
            }
        }

        let where_clause = format!("WHERE {}", conditions.join(" AND "));

        // Determine if we're grouping by time period
        match group_by.as_deref() {
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

                let rows: Vec<EventCountByPeriod> =
                    state.clickhouse.query(&query).fetch_all().await?;
                Ok(serde_json::to_value(rows)?)
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

                let rows: Vec<EventCountByPeriod> =
                    state.clickhouse.query(&query).fetch_all().await?;
                Ok(serde_json::to_value(rows)?)
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

                let rows: Vec<EventCountByPeriod> =
                    state.clickhouse.query(&query).fetch_all().await?;
                Ok(serde_json::to_value(rows)?)
            }
            _ => {
                // No grouping - return aggregate
                let query = format!(
                    "SELECT
                        count() AS count,
                        uniq(pubkey) AS unique_pubkeys
                    FROM events_local
                    {}",
                    where_clause
                );

                let stats: EventCountResponse = state.clickhouse.query(&query).fetch_one().await?;
                Ok(serde_json::to_value(stats)?)
            }
        }
    })
    .await?;

    Ok(Json(result))
}

// ═══════════════════════════════════════════════════════════════════════════
// Active Users
// ═══════════════════════════════════════════════════════════════════════════

/// Active users summary (current DAU/WAU/MAU).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// Returns current DAU/WAU/MAU summary (most recent values).
/// Queries the small pre-computed tables directly.
/// Cached for 10 minutes.
pub async fn active_users_summary(
    State(state): State<AppState>,
) -> Result<Json<ActiveUsersSummary>, ApiError> {
    let result = get_or_compute(&state.cache, "active_users_summary", || async {
        // Run all three queries in parallel - each fetches from tiny summary tables
        let (daily, weekly, monthly) = tokio::join!(
            fetch_latest_daily_active_users(&state),
            fetch_latest_weekly_active_users(&state),
            fetch_latest_monthly_active_users(&state),
        );

        Ok(ActiveUsersSummary {
            daily: daily?,
            weekly: weekly?,
            monthly: monthly?,
        })
    })
    .await?;

    Ok(Json(result))
}

/// Fetch the most recent daily active users from the pre-computed table.
async fn fetch_latest_daily_active_users(state: &AppState) -> Result<ActiveUsersCount, ApiError> {
    if state.uses_postgres(AnalyticsFamily::ActiveUsers) {
        return fetch_latest_postgres_active_users(state, "day").await;
    }
    state
        .clickhouse
        .query(
            "SELECT
                active_users,
                has_profile,
                has_follows AS has_follows_list,
                has_both AS has_profile_and_follows_list,
                total_events
            FROM active_users_daily FINAL
            WHERE date >= toDate('2020-11-07') AND date <= today()
            ORDER BY date DESC
            LIMIT 1",
        )
        .fetch_one()
        .await
        .map_err(ApiError::from)
}

/// Fetch the most recent weekly active users from the pre-computed table.
async fn fetch_latest_weekly_active_users(state: &AppState) -> Result<ActiveUsersCount, ApiError> {
    if state.uses_postgres(AnalyticsFamily::ActiveUsers) {
        return fetch_latest_postgres_active_users(state, "week").await;
    }
    state
        .clickhouse
        .query(
            "SELECT
                active_users,
                has_profile,
                has_follows AS has_follows_list,
                has_both AS has_profile_and_follows_list,
                total_events
            FROM active_users_weekly FINAL
            WHERE week >= toDate('2020-11-07') AND week <= toMonday(today())
            ORDER BY week DESC
            LIMIT 1",
        )
        .fetch_one()
        .await
        .map_err(ApiError::from)
}

/// Fetch the most recent monthly active users from the pre-computed table.
async fn fetch_latest_monthly_active_users(state: &AppState) -> Result<ActiveUsersCount, ApiError> {
    if state.uses_postgres(AnalyticsFamily::ActiveUsers) {
        return fetch_latest_postgres_active_users(state, "month").await;
    }
    state
        .clickhouse
        .query(
            "SELECT
                active_users,
                has_profile,
                has_follows AS has_follows_list,
                has_both AS has_profile_and_follows_list,
                total_events
            FROM active_users_monthly FINAL
            WHERE month >= toDate('2020-11-07') AND month <= toStartOfMonth(today())
            ORDER BY month DESC
            LIMIT 1",
        )
        .fetch_one()
        .await
        .map_err(ApiError::from)
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
/// Fetches from small pre-computed table (~1500 rows total).
/// Cached for 10 minutes.
pub async fn active_users_daily(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365) as usize;
    let since = params.since;

    // Cache key includes query params
    let cache_key = format!(
        "active_users_daily:limit={}&since={}",
        limit,
        since.map(|d| d.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        if state.uses_postgres(AnalyticsFamily::ActiveUsers) {
            return fetch_postgres_active_users(&state, "day", since, limit).await;
        }
        // Fetch all daily data from the small summary table, filter in Rust
        let mut rows: Vec<ActiveUsersRow> = state
            .clickhouse
            .query(
                "SELECT
                    toString(date) AS period,
                    active_users,
                    has_profile,
                    has_follows AS has_follows_list,
                    has_both AS has_profile_and_follows_list,
                    total_events
                FROM active_users_daily FINAL
                WHERE date >= toDate('2020-11-07') AND date <= today()
                ORDER BY date DESC",
            )
            .fetch_all()
            .await?;

        // Apply filters in Rust (simple and fast for small data)
        if let Some(since) = since {
            let since_str = since.to_string();
            rows.retain(|r| r.period >= since_str);
        }
        rows.truncate(limit);

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}

/// `GET /api/v1/stats/users/active/weekly`
///
/// Returns weekly active users time series.
/// Fetches from small pre-computed table (~215 rows total).
/// Cached for 10 minutes.
pub async fn active_users_weekly(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(52) as usize;
    let since = params.since;

    let cache_key = format!(
        "active_users_weekly:limit={}&since={}",
        limit,
        since.map(|d| d.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        if state.uses_postgres(AnalyticsFamily::ActiveUsers) {
            return fetch_postgres_active_users(&state, "week", since, limit).await;
        }
        // Fetch all weekly data from the small summary table, filter in Rust
        let mut rows: Vec<ActiveUsersRow> = state
            .clickhouse
            .query(
                "SELECT
                    toString(week) AS period,
                    active_users,
                    has_profile,
                    has_follows AS has_follows_list,
                    has_both AS has_profile_and_follows_list,
                    total_events
                FROM active_users_weekly FINAL
                WHERE week >= toDate('2020-11-07') AND week <= toMonday(today())
                ORDER BY week DESC",
            )
            .fetch_all()
            .await?;

        // Apply filters in Rust
        if let Some(since) = since {
            let since_str = since.to_string();
            rows.retain(|r| r.period >= since_str);
        }
        rows.truncate(limit);

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}

/// `GET /api/v1/stats/users/active/monthly`
///
/// Returns monthly active users time series.
/// Fetches from small pre-computed table (~50 rows total).
/// Cached for 10 minutes.
pub async fn active_users_monthly(
    State(state): State<AppState>,
    Query(params): Query<ActiveUsersQuery>,
) -> Result<Json<Vec<ActiveUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(120) as usize;
    let since = params.since;

    let cache_key = format!(
        "active_users_monthly:limit={}&since={}",
        limit,
        since.map(|d| d.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        if state.uses_postgres(AnalyticsFamily::ActiveUsers) {
            return fetch_postgres_active_users(&state, "month", since, limit).await;
        }
        // Fetch all monthly data from the small summary table, filter in Rust
        let mut rows: Vec<ActiveUsersRow> = state
            .clickhouse
            .query(
                "SELECT
                    toString(month) AS period,
                    active_users,
                    has_profile,
                    has_follows AS has_follows_list,
                    has_both AS has_profile_and_follows_list,
                    total_events
                FROM active_users_monthly FINAL
                WHERE month >= toDate('2020-11-07') AND month <= toStartOfMonth(today())
                ORDER BY month DESC",
            )
            .fetch_all()
            .await?;

        // Apply filters in Rust
        if let Some(since) = since {
            let since_str = since.to_string();
            rows.retain(|r| r.period >= since_str);
        }
        rows.truncate(limit);

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}

async fn fetch_latest_postgres_active_users(
    state: &AppState,
    grain: &'static str,
) -> Result<ActiveUsersCount, ApiError> {
    let client = state
        .postgres_client(AnalyticsFamily::ActiveUsers)
        .await
        .map_err(ApiError::Internal)?;
    let row = client
        .query_one(
            "SELECT active_users,has_profile,has_follows_list,
                    has_profile_and_follows_list,total_events
               FROM pensieve_analytics.current_active_users_period
              WHERE grain=$1
              ORDER BY period_start DESC
              LIMIT 1",
            &[&grain],
        )
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    postgres_active_users_count(&row)
}

async fn fetch_postgres_active_users(
    state: &AppState,
    grain: &'static str,
    since: Option<NaiveDate>,
    limit: usize,
) -> Result<Vec<ActiveUsersRow>, ApiError> {
    let client = state
        .postgres_client(AnalyticsFamily::ActiveUsers)
        .await
        .map_err(ApiError::Internal)?;
    let limit = i64::try_from(limit)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("active-user limit exceeds i64")))?;
    let rows = client
        .query(
            "SELECT to_char(period_start,'YYYY-MM-DD'),active_users,has_profile,
                    has_follows_list,has_profile_and_follows_list,total_events
               FROM pensieve_analytics.current_active_users_period
              WHERE grain=$1 AND ($2::date IS NULL OR period_start >= $2)
              ORDER BY period_start DESC
              LIMIT $3",
            &[&grain, &since, &limit],
        )
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    rows.iter().map(postgres_active_users_row).collect()
}

fn postgres_active_users_count(row: &tokio_postgres::Row) -> Result<ActiveUsersCount, ApiError> {
    Ok(ActiveUsersCount {
        active_users: nonnegative_u64(row.get(0), "active_users")?,
        has_profile: nonnegative_u64(row.get(1), "has_profile")?,
        has_follows_list: nonnegative_u64(row.get(2), "has_follows_list")?,
        has_profile_and_follows_list: nonnegative_u64(row.get(3), "has_profile_and_follows_list")?,
        total_events: nonnegative_u64(row.get(4), "total_events")?,
    })
}

fn postgres_active_users_row(row: &tokio_postgres::Row) -> Result<ActiveUsersRow, ApiError> {
    Ok(ActiveUsersRow {
        period: row.get(0),
        active_users: nonnegative_u64(row.get(1), "active_users")?,
        has_profile: nonnegative_u64(row.get(2), "has_profile")?,
        has_follows_list: nonnegative_u64(row.get(3), "has_follows_list")?,
        has_profile_and_follows_list: nonnegative_u64(row.get(4), "has_profile_and_follows_list")?,
        total_events: nonnegative_u64(row.get(5), "total_events")?,
    })
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputResponse {
    /// Average events per hour over the last 7 days.
    pub events_per_hour: f64,
    /// Total events in the 7-day window.
    pub total_events_7d: u64,
}

/// `GET /api/v1/stats/throughput`
///
/// Returns 7-day rolling average of events created per hour.
/// Cached for 5 minutes.
pub async fn throughput(
    State(state): State<AppState>,
    Query(params): Query<ThroughputQuery>,
) -> Result<Json<ThroughputResponse>, ApiError> {
    let kind = params.kind;
    let cache_key = format!(
        "throughput:kind={}",
        kind.map(|k| k.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        let kind_clause = match kind {
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

        Ok(ThroughputResponse {
            events_per_hour: row.events_per_hour,
            total_events_7d: row.total_events_7d,
        })
    })
    .await?;

    Ok(Json(result))
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// Requires the cohort_retention_weekly/monthly tables (migration 012).
/// Cached for 5 minutes.
pub async fn user_retention(
    State(state): State<AppState>,
    Query(params): Query<RetentionQuery>,
) -> Result<Json<Vec<CohortRetention>>, ApiError> {
    let limit = params.limit.unwrap_or(12).min(52);
    let cohort_size_param = params.cohort_size.clone();
    let cohort_start = params.cohort_start;

    // Validate before caching
    let cohort_size = cohort_size_param.as_deref().unwrap_or("week");
    let (view_name, interval_unit) = match cohort_size {
        "month" => ("cohort_retention_monthly_view", "MONTH"),
        "week" => ("cohort_retention_weekly_view", "WEEK"),
        other => {
            return Err(ApiError::BadRequest(format!(
                "invalid cohort_size value: '{}'. Valid options: week, month",
                other
            )));
        }
    };

    let cache_key = format!(
        "user_retention:cohort_size={}&limit={}&cohort_start={}",
        cohort_size,
        limit,
        cohort_start.map(|d| d.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        if state.uses_postgres(AnalyticsFamily::Retention) {
            return fetch_postgres_retention(&state, cohort_size, cohort_start, limit as usize)
                .await;
        }
        // Build cohort filter clause
        let cohort_filter = match cohort_start {
            Some(date) => format!("WHERE cohort >= '{}'", date),
            None => format!(
                "WHERE cohort >= toString(today() - INTERVAL {} {})",
                limit, interval_unit
            ),
        };

        // Query pre-aggregated summary tables (migration 012)
        // These tables are much faster than joining events_local with pubkey_first_seen
        let rows: Vec<CohortActivityRow> = state
            .clickhouse
            .query(&format!(
                "SELECT
                    cohort,
                    activity_period,
                    active_count
                FROM {}
                {}
                ORDER BY cohort, activity_period",
                view_name, cohort_filter
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
                // cohort_size is the count from the period matching the cohort (period 0).
                // We must find activity_period == cohort explicitly, not just take the first
                // sorted entry, because users may have no activity in their cohort period.
                let cohort_size = periods
                    .iter()
                    .find(|(period, _)| period == &cohort)
                    .map(|(_, c)| *c)
                    .unwrap_or(0);
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

        Ok(result)
    })
    .await?;

    Ok(Json(result))
}

async fn fetch_postgres_retention(
    state: &AppState,
    grain: &str,
    cohort_start: Option<NaiveDate>,
    limit: usize,
) -> Result<Vec<CohortRetention>, ApiError> {
    let client = state
        .postgres_client(AnalyticsFamily::Retention)
        .await
        .map_err(ApiError::Internal)?;
    let limit = i64::try_from(limit)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("retention limit exceeds i64")))?;
    let rows = client
        .query(
            "WITH selected AS (
                 SELECT DISTINCT cohort_start
                   FROM pensieve_analytics.current_cohort_retention_period
                  WHERE grain=$1 AND ($2::date IS NULL OR cohort_start >= $2)
                  ORDER BY cohort_start DESC
                  LIMIT $3
             )
             SELECT to_char(retention.cohort_start,'YYYY-MM-DD'),
                    to_char(retention.activity_period,'YYYY-MM-DD'),
                    retention.active_pubkeys
               FROM pensieve_analytics.current_cohort_retention_period retention
               JOIN selected USING (cohort_start)
              WHERE retention.grain=$1
              ORDER BY retention.cohort_start DESC, retention.activity_period ASC",
            &[&grain, &cohort_start, &limit],
        )
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    let mut groups = Vec::<(String, Vec<(String, u64)>)>::new();
    for row in rows {
        let cohort: String = row.get(0);
        let activity_period: String = row.get(1);
        let active_count = nonnegative_u64(row.get(2), "active_pubkeys")?;
        if groups.last().is_none_or(|(current, _)| current != &cohort) {
            groups.push((cohort.clone(), Vec::new()));
        }
        groups
            .last_mut()
            .expect("retention group was just inserted")
            .1
            .push((activity_period, active_count));
    }
    Ok(groups
        .into_iter()
        .map(|(cohort, periods)| retention_response(cohort, periods))
        .collect())
}

fn retention_response(cohort: String, periods: Vec<(String, u64)>) -> CohortRetention {
    let cohort_size = periods
        .iter()
        .find(|(period, _)| period == &cohort)
        .map_or(0, |(_, count)| *count);
    let retention = periods.iter().map(|(_, count)| *count).collect::<Vec<_>>();
    let retention_pct = retention
        .iter()
        .map(|count| {
            if cohort_size == 0 {
                0.0
            } else {
                (*count as f64 / cohort_size as f64) * 100.0
            }
        })
        .collect();
    CohortRetention {
        cohort,
        cohort_size,
        retention,
        retention_pct,
    }
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
/// Requires the new_users_daily table (migration 013).
/// Cached for 5 minutes.
pub async fn new_users(
    State(state): State<AppState>,
    Query(params): Query<NewUsersQuery>,
) -> Result<Json<Vec<NewUsersRow>>, ApiError> {
    let limit = params.limit.unwrap_or(30).min(365);
    let group_by = params.group_by.clone();
    let since = params.since;

    // Validate group_by before caching
    let (group_expr, max_limit) = match group_by.as_deref() {
        Some("week") => ("toMonday(date)", 52u32),
        Some("month") => ("toStartOfMonth(date)", 120u32),
        Some("day") | None => ("date", 365u32),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "invalid group_by value: '{}'. Valid options: day, week, month",
                other
            )));
        }
    };

    let limit = limit.min(max_limit);

    let cache_key = format!(
        "new_users:group_by={}&limit={}&since={}",
        group_by.as_deref().unwrap_or("day"),
        limit,
        since.map(|d| d.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        if state.uses_postgres(AnalyticsFamily::NewUsers) {
            return fetch_postgres_new_users(
                &state,
                group_by.as_deref().unwrap_or("day"),
                since,
                limit as usize,
            )
            .await;
        }
        let since_clause = match since {
            Some(date) => format!("AND date >= '{}'", date),
            None => String::new(),
        };

        let rows: Vec<NewUsersRow> = state
            .clickhouse
            .query(&format!(
                "SELECT
                    toString({}) AS period,
                    sum(new_users) AS new_users
                FROM new_users_daily FINAL
                WHERE 1=1 {}
                GROUP BY period
                ORDER BY period DESC
                LIMIT {}",
                group_expr, since_clause, limit
            ))
            .fetch_all()
            .await?;

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}

async fn fetch_postgres_new_users(
    state: &AppState,
    grain: &str,
    since: Option<NaiveDate>,
    limit: usize,
) -> Result<Vec<NewUsersRow>, ApiError> {
    let period = match grain {
        "day" => "day",
        "week" => "date_trunc('week',day)::date",
        "month" => "date_trunc('month',day)::date",
        _ => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "invalid new-user grain"
            )));
        }
    };
    let query = format!(
        "SELECT to_char({period},'YYYY-MM-DD'),SUM(new_pubkeys)::bigint
           FROM pensieve_analytics.current_new_users_daily
          WHERE ($1::date IS NULL OR day >= $1)
          GROUP BY {period}
          ORDER BY {period} DESC
          LIMIT $2"
    );
    let limit = i64::try_from(limit)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("new-user limit exceeds i64")))?;
    let client = state
        .postgres_client(AnalyticsFamily::NewUsers)
        .await
        .map_err(ApiError::Internal)?;
    client
        .query(&query, &[&since, &limit])
        .await
        .map_err(|error| ApiError::Internal(error.into()))?
        .iter()
        .map(|row| {
            Ok(NewUsersRow {
                period: row.get(0),
                new_users: nonnegative_u64(row.get(1), "new_users")?,
            })
        })
        .collect()
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
/// Cached for 10 minutes.
pub async fn hourly_activity(
    State(state): State<AppState>,
    Query(params): Query<HourlyActivityQuery>,
) -> Result<Json<Vec<HourlyActivityRow>>, ApiError> {
    let days = params.days.unwrap_or(7).min(90);
    let kind = params.kind;

    let cache_key = format!(
        "hourly_activity:days={}&kind={}",
        days,
        kind.map(|k| k.to_string()).unwrap_or_default()
    );

    let result = get_or_compute_with_ttl(&state.cache, &cache_key, ttl::TIME_SERIES, || async {
        let kind_clause = match kind {
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

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
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
/// Cached for 5 minutes.
pub async fn zap_stats(
    State(state): State<AppState>,
    Query(params): Query<ZapStatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let days = params.days.unwrap_or(30);
    let limit = params.limit.unwrap_or(30).min(365);
    let group_by = params.group_by.clone();

    // Validate before caching
    if let Some(ref g) = group_by
        && !["day", "week", "month"].contains(&g.as_str())
    {
        return Err(ApiError::BadRequest(format!(
            "invalid group_by value: '{}'. Valid options: day, week, month",
            g
        )));
    }

    let cache_key = format!(
        "zap_stats:days={}&group_by={}&limit={}",
        days,
        group_by.as_deref().unwrap_or(""),
        limit
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        match group_by.as_deref() {
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
                Ok(serde_json::to_value(rows)?)
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
                Ok(serde_json::to_value(rows)?)
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
                Ok(serde_json::to_value(rows)?)
            }
            _ => {
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
                Ok(serde_json::to_value(stats)?)
            }
        }
    })
    .await?;

    Ok(Json(result))
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
/// Cached for 10 minutes.
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
    let cache_key = format!("zap_histogram:days={}", days);

    let result = get_or_compute(&state.cache, &cache_key, || async {
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

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// Cached for 10 minutes.
pub async fn engagement(
    State(state): State<AppState>,
    Query(params): Query<EngagementQuery>,
) -> Result<Json<EngagementStats>, ApiError> {
    let days = params.days.unwrap_or(30);
    let cache_key = format!("engagement:days={}", days);

    let result = get_or_compute_with_ttl(&state.cache, &cache_key, ttl::TIME_SERIES, || async {
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

        Ok(EngagementStats {
            period_days: days,
            original_notes,
            replies: row.total_replies,
            reactions: row.total_reactions,
            replies_per_note: row.total_replies as f64 / base,
            reactions_per_note: row.total_reactions as f64 / base,
        })
    })
    .await?;

    Ok(Json(result))
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
/// Cached for 10 minutes.
pub async fn longform(
    State(state): State<AppState>,
    Query(params): Query<LongformQuery>,
) -> Result<Json<LongformStats>, ApiError> {
    let days = params.days;
    let cache_key = format!(
        "longform:days={}",
        days.map(|d| d.to_string()).unwrap_or_default()
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        let days_clause = match days {
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

        Ok(stats)
    })
    .await?;

    Ok(Json(result))
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

// ═══════════════════════════════════════════════════════════════════════════
// Relay Distribution (NIP-65)
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for relay distribution.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayDistributionQuery {
    /// Number of relays to return (default: 100, max: 1000).
    pub limit: Option<u32>,
}

/// Relay distribution row from NIP-65 relay lists.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct RelayDistributionRow {
    /// Normalized relay URL.
    pub relay_url: String,
    /// Number of users listing this relay in their latest kind 10002 event.
    pub user_count: u64,
    /// Users listing this relay for reading (includes read+write).
    pub read_count: u64,
    /// Users listing this relay for writing (includes read+write).
    pub write_count: u64,
}

/// `GET /api/v1/stats/relays/distribution`
///
/// Returns relay popularity distribution from NIP-65 relay lists (kind 10002).
/// Only includes each user's latest relay list event.
/// Cached for 10 minutes (data refreshes every 6 hours via MV).
pub async fn relay_distribution(
    State(state): State<AppState>,
    Query(params): Query<RelayDistributionQuery>,
) -> Result<Json<Vec<RelayDistributionRow>>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);
    let cache_key = format!("relay_distribution:limit={}", limit);

    let result = get_or_compute(&state.cache, &cache_key, || async {
        let rows: Vec<RelayDistributionRow> = state
            .clickhouse
            .query(&format!(
                "SELECT
                    relay_url,
                    user_count,
                    read_count,
                    write_count
                FROM relay_distribution FINAL
                ORDER BY user_count DESC
                LIMIT {}",
                limit
            ))
            .fetch_all()
            .await?;

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
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
/// Cached for 5 minutes.
pub async fn publishers(
    State(state): State<AppState>,
    Query(params): Query<PublishersQuery>,
) -> Result<Json<Vec<PublisherRow>>, ApiError> {
    let limit = params.limit.unwrap_or(100).min(1000);
    let days = params.days.unwrap_or(30);
    let kind = params.kind;

    let cache_key = format!(
        "publishers:days={}&kind={}&limit={}",
        days,
        kind.map(|k| k.to_string()).unwrap_or_default(),
        limit
    );

    let result = get_or_compute(&state.cache, &cache_key, || async {
        let kind_clause = match kind {
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

        Ok(rows)
    })
    .await?;

    Ok(Json(result))
}
