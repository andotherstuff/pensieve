//! Exact Slice A computation in a persistent DuckDB work database.

use std::path::Path;

use duckdb::{Connection, OptionalExt, params};
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::Serialize;

use crate::{Error, ResolvedSnapshot, Result};

/// Version of the SQL semantics materialized by this crate.
pub const QUERY_VERSION: &str = "slice-a-v1";
const API_TIMESTAMP_MAX: u64 = u32::MAX as u64;
const SEVEN_DAYS_SECS: u64 = 7 * 24 * 60 * 60;
const THIRTY_DAYS_SECS: u64 = 30 * 24 * 60 * 60;
const HOURS_PER_SEVEN_DAYS: f64 = 168.0;

/// Reproducible inputs to one full analytics build.
#[derive(Clone, Debug)]
pub struct BuildConfig {
    /// Fixed upper event-time boundary used by rolling/non-future metrics.
    pub as_of_epoch: u64,
    /// Operator/build identity recorded with the run.
    pub code_version: String,
    /// AWS region used by DuckDB's S3 credential-chain secret.
    pub s3_region: String,
    /// Use S3 path-style addressing instead of virtual-host addressing.
    pub s3_force_path_style: bool,
    /// DuckDB buffer-manager limit; lower values spill earlier to protect colocated services.
    pub memory_limit: String,
    /// DuckDB worker threads; fewer workers reduce per-query memory reservations.
    pub threads: usize,
}

/// One-row overview product.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Overview {
    /// Logical events after cross-file event-ID deduplication.
    pub total_events: u64,
    /// Logical events in the API-representable unsigned timestamp domain.
    pub api_representable_events: u64,
    /// Earliest representable event timestamp, clamped to Nostr genesis.
    pub earliest_event: u32,
    /// Latest event timestamp no later than `as_of`.
    pub latest_event: u32,
    /// Exact events in the inclusive rolling seven-day interval.
    pub events_7d: u64,
    /// Seven-day count divided by exactly 168 hours.
    pub events_per_hour_7d: f64,
    /// Distinct kinds in the rolling 30-day interval.
    pub kinds_30d: u64,
}

/// One UTC date event-count row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventDaily {
    /// ISO-8601 UTC date.
    pub day: String,
    /// Exact logical events on the date.
    pub event_count: u64,
}

/// One UTC date and event-kind count row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventDailyKind {
    /// ISO-8601 UTC date.
    pub day: String,
    /// Unsigned Nostr event kind.
    pub kind: u16,
    /// Exact logical events for the date and kind.
    pub event_count: u64,
}

/// One all-time event-kind count row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KindAllTime {
    /// Unsigned Nostr event kind.
    pub kind: u16,
    /// Exact logical events of this kind.
    pub event_count: u64,
}

/// Reconciled counts describing materialized DuckDB products.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BuildSummary {
    /// Physical rows asserted by the immutable input catalog.
    pub physical_rows: u64,
    /// Logical rows after event-ID deduplication.
    pub logical_events: u64,
    /// Physical duplicate rows removed.
    pub duplicate_rows: u64,
    /// Logical rows represented in UTC daily products.
    pub api_representable_events: u64,
    /// Rows in the all-kind daily relation.
    pub event_daily_rows: u64,
    /// Rows in the daily-kind relation.
    pub event_daily_kind_rows: u64,
    /// Rows in the all-time kind relation.
    pub kind_all_time_rows: u64,
}

/// One completed DuckDB build, ready for Postgres publication.
pub struct AnalyticsBuild {
    connection: Connection,
    /// Validated catalog and exact object locations used by the build.
    pub snapshot: ResolvedSnapshot,
    /// Fixed build configuration.
    pub config: BuildConfig,
    /// Reconciled materialized relation counts.
    pub summary: BuildSummary,
}

impl AnalyticsBuild {
    /// Build all Slice A products in `work_database`.
    ///
    /// The work database is persistent so large scans can spill out of core and
    /// the completed products survive a transient Postgres publication failure.
    pub fn create(
        work_database: impl AsRef<Path>,
        snapshot: ResolvedSnapshot,
        config: BuildConfig,
    ) -> Result<Self> {
        if config.as_of_epoch > API_TIMESTAMP_MAX {
            return Err(Error::Validation(format!(
                "as_of {} exceeds the V1 API timestamp maximum {}",
                config.as_of_epoch, API_TIMESTAMP_MAX
            )));
        }

        let connection = Connection::open(work_database)?;
        configure_execution(&connection, &config)?;
        connection.execute_batch(
            "
            SET TimeZone = 'UTC';
            SET preserve_insertion_order = false;
            DROP TABLE IF EXISTS rollup_overview;
            DROP TABLE IF EXISTS rollup_event_daily;
            DROP TABLE IF EXISTS rollup_event_daily_kind;
            DROP TABLE IF EXISTS rollup_kind_all_time;
            DROP TABLE IF EXISTS canonical_events;
            ",
        )?;
        configure_remote_access(&connection, &snapshot, &config)?;
        materialize_canonical_events(&connection, &snapshot)?;
        materialize_rollups(&connection, config.as_of_epoch)?;
        let summary = validate_rollups(&connection, &snapshot)?;

        Ok(Self {
            connection,
            snapshot,
            config,
            summary,
        })
    }

    /// Read the one-row overview product.
    pub fn overview(&self) -> Result<Overview> {
        self.connection
            .query_row(
                "
                SELECT
                    total_events,
                    api_representable_events,
                    earliest_event,
                    latest_event,
                    events_7d,
                    events_per_hour_7d,
                    kinds_30d
                FROM rollup_overview
                ",
                [],
                |row| {
                    Ok(Overview {
                        total_events: row.get(0)?,
                        api_representable_events: row.get(1)?,
                        earliest_event: row.get(2)?,
                        latest_event: row.get(3)?,
                        events_7d: row.get(4)?,
                        events_per_hour_7d: row.get(5)?,
                        kinds_30d: row.get(6)?,
                    })
                },
            )
            .map_err(Error::from)
    }

    /// Visit daily event rows in stable ascending key order.
    pub fn for_each_event_daily(
        &self,
        mut visit: impl FnMut(EventDaily) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self.connection.prepare(
            "SELECT CAST(day AS VARCHAR), event_count FROM rollup_event_daily ORDER BY day",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            visit(EventDaily {
                day: row.get(0)?,
                event_count: row.get(1)?,
            })?;
        }
        Ok(())
    }

    /// Visit daily-kind rows in stable ascending key order.
    pub fn for_each_event_daily_kind(
        &self,
        mut visit: impl FnMut(EventDailyKind) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self.connection.prepare(
            "
            SELECT CAST(day AS VARCHAR), kind, event_count
            FROM rollup_event_daily_kind
            ORDER BY day, kind
            ",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            visit(EventDailyKind {
                day: row.get(0)?,
                kind: row.get(1)?,
                event_count: row.get(2)?,
            })?;
        }
        Ok(())
    }

    /// Visit all-time kind rows in stable ascending key order.
    pub fn for_each_kind_all_time(
        &self,
        mut visit: impl FnMut(KindAllTime) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self
            .connection
            .prepare("SELECT kind, event_count FROM rollup_kind_all_time ORDER BY kind")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            visit(KindAllTime {
                kind: row.get(0)?,
                event_count: row.get(1)?,
            })?;
        }
        Ok(())
    }
}

fn configure_execution(connection: &Connection, config: &BuildConfig) -> Result<()> {
    if config.threads == 0 {
        return Err(Error::Validation(
            "DuckDB worker thread count must be greater than zero".to_owned(),
        ));
    }
    let sql = format!(
        "SET memory_limit = {}; SET threads = {}",
        sql_string(&config.memory_limit),
        config.threads
    );
    connection.execute_batch(&sql)?;
    Ok(())
}

fn configure_remote_access(
    connection: &Connection,
    snapshot: &ResolvedSnapshot,
    config: &BuildConfig,
) -> Result<()> {
    let Some(endpoint) = snapshot.s3_endpoint.as_deref() else {
        return Ok(());
    };
    connection.execute_batch(
        "
        INSTALL httpfs;
        LOAD httpfs;
        INSTALL aws;
        LOAD aws;
        SET http_retries = 12;
        SET http_retry_wait_ms = 500;
        SET http_retry_backoff = 2;
        SET http_timeout = 120;
        ",
    )?;

    let url_style = if config.s3_force_path_style {
        "path"
    } else {
        "vhost"
    };
    let sql = format!(
        "
        CREATE OR REPLACE SECRET pensieve_analytics_s3 (
            TYPE s3,
            PROVIDER credential_chain,
            CHAIN 'env',
            REGION {},
            ENDPOINT {},
            URL_STYLE {}
        )
        ",
        sql_string(&config.s3_region),
        sql_string(endpoint),
        sql_string(url_style),
    );
    connection.execute_batch(&sql)?;
    Ok(())
}

fn materialize_canonical_events(
    connection: &Connection,
    snapshot: &ResolvedSnapshot,
) -> Result<()> {
    if snapshot.locations.is_empty() {
        connection.execute_batch(
            "
            CREATE TABLE canonical_events (
                id BLOB NOT NULL,
                created_at UBIGINT NOT NULL,
                kind USMALLINT NOT NULL
            );
            ",
        )?;
        return Ok(());
    }

    let paths = snapshot
        .locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    // Every input row was cryptographically validated before publication. A
    // Nostr event ID commits to created_at and kind, so duplicates of the
    // projected tuple are the exact logical-event duplicates we must remove.
    // Avoid carrying `sig` through a global window sort when it is not stored.
    let sql = format!(
        "
        CREATE TABLE canonical_events AS
        SELECT DISTINCT id, created_at, kind
        FROM read_parquet([{paths}], union_by_name = false);
        "
    );
    connection.execute_batch(&sql)?;
    Ok(())
}

fn materialize_rollups(connection: &Connection, as_of: u64) -> Result<()> {
    let seven_day_start = as_of.saturating_sub(SEVEN_DAYS_SECS);
    let thirty_day_start = as_of.saturating_sub(THIRTY_DAYS_SECS);
    connection.execute(
        "
        CREATE TABLE rollup_overview AS
        SELECT
            count(*)::UBIGINT AS total_events,
            count(*) FILTER (WHERE created_at <= ?)::UBIGINT
                AS api_representable_events,
            greatest(
                coalesce(min(created_at) FILTER (WHERE created_at <= ?), 0),
                ?
            )::UINTEGER AS earliest_event,
            coalesce(
                max(created_at) FILTER (WHERE created_at <= ?),
                0
            )::UINTEGER AS latest_event,
            count(*) FILTER (
                WHERE created_at >= ? AND created_at <= ?
            )::UBIGINT AS events_7d,
            (
                count(*) FILTER (
                    WHERE created_at >= ? AND created_at <= ?
                )::DOUBLE / ?
            ) AS events_per_hour_7d,
            count(DISTINCT kind) FILTER (
                WHERE created_at >= ? AND created_at <= ?
            )::UBIGINT AS kinds_30d
        FROM canonical_events
        ",
        params![
            API_TIMESTAMP_MAX,
            API_TIMESTAMP_MAX,
            NOSTR_GENESIS_TIMESTAMP,
            as_of,
            seven_day_start,
            as_of,
            seven_day_start,
            as_of,
            HOURS_PER_SEVEN_DAYS,
            thirty_day_start,
            as_of,
        ],
    )?;
    connection.execute(
        "
        CREATE TABLE rollup_event_daily AS
        SELECT
            DATE '1970-01-01' + CAST(created_at // 86400 AS INTEGER) AS day,
            count(*)::UBIGINT AS event_count
        FROM canonical_events
        WHERE created_at <= ?
        GROUP BY day
        ORDER BY day
        ",
        params![API_TIMESTAMP_MAX],
    )?;
    connection.execute(
        "
        CREATE TABLE rollup_event_daily_kind AS
        SELECT
            DATE '1970-01-01' + CAST(created_at // 86400 AS INTEGER) AS day,
            kind,
            count(*)::UBIGINT AS event_count
        FROM canonical_events
        WHERE created_at <= ?
        GROUP BY day, kind
        ORDER BY day, kind
        ",
        params![API_TIMESTAMP_MAX],
    )?;
    connection.execute_batch(
        "
        CREATE TABLE rollup_kind_all_time AS
        SELECT kind, count(*)::UBIGINT AS event_count
        FROM canonical_events
        GROUP BY kind
        ORDER BY kind;
        ",
    )?;
    Ok(())
}

fn validate_rollups(connection: &Connection, snapshot: &ResolvedSnapshot) -> Result<BuildSummary> {
    let logical_events = scalar_u64(connection, "SELECT count(*) FROM canonical_events")?;
    let physical_rows = snapshot.catalog.totals().physical_rows;
    let duplicate_rows = physical_rows.checked_sub(logical_events).ok_or_else(|| {
        Error::Validation(format!(
            "catalog claims {physical_rows} physical rows but DuckDB produced {logical_events} logical rows"
        ))
    })?;
    let api_representable_events = scalar_u64(
        connection,
        "SELECT api_representable_events FROM rollup_overview",
    )?;
    validate_sum(connection, "rollup_event_daily", api_representable_events)?;
    validate_sum(
        connection,
        "rollup_event_daily_kind",
        api_representable_events,
    )?;
    validate_sum(connection, "rollup_kind_all_time", logical_events)?;

    Ok(BuildSummary {
        physical_rows,
        logical_events,
        duplicate_rows,
        api_representable_events,
        event_daily_rows: scalar_u64(connection, "SELECT count(*) FROM rollup_event_daily")?,
        event_daily_kind_rows: scalar_u64(
            connection,
            "SELECT count(*) FROM rollup_event_daily_kind",
        )?,
        kind_all_time_rows: scalar_u64(connection, "SELECT count(*) FROM rollup_kind_all_time")?,
    })
}

fn validate_sum(connection: &Connection, table: &str, expected: u64) -> Result<()> {
    let actual = scalar_u64(
        connection,
        &format!("SELECT coalesce(sum(event_count), 0)::UBIGINT FROM {table}"),
    )?;
    if actual != expected {
        return Err(Error::Validation(format!(
            "{table} sums to {actual}, expected {expected}"
        )));
    }
    Ok(())
}

fn scalar_u64(connection: &Connection, sql: &str) -> Result<u64> {
    connection
        .query_row(sql, [], |row| row.get(0))
        .optional()?
        .ok_or_else(|| Error::Validation(format!("query returned no scalar row: {sql}")))
}

// DuckDB does not accept bind parameters in `CREATE SECRET` options or in a
// `read_parquet` file-list literal. Those values come from the operator-selected
// catalog and CLI configuration, so quote and escape them in one place.
fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
