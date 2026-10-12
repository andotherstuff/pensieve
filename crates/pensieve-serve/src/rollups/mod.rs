//! Insert-fed ClickHouse rollups behind `/api/v1/stats/publishers` and
//! `/api/v1/stats/longform` (prepared migration 019).
//!
//! Both endpoints used to scan `events_local` (~2.4B rows, `ORDER BY id`, no
//! projections) and hit the API's 20 s query cap. Migration
//! `docs/migrations/prepared/019_api_publishers_longform.sql` adds two small
//! tables fed by materialized views:
//!
//! - `publisher_activity`: event counts per (grain, kind, period, pubkey),
//!   with a day grain and a month grain and an all-kinds rollup (`kind = -1`).
//! - `longform_latest`: the latest version of every kind 30023 article per
//!   (pubkey, d_tag), with its content length.
//!
//! This module holds the SQL shared by the API reads and the `api-rollups`
//! operator tool (plan, backfill, status, validate). The backfill `SELECT`
//! bodies must stay equivalent to the views in migration 019; the integration
//! tests in `tests/api_rollups.rs` insert the same events through both paths
//! and compare. See `docs/api_rollups.md`.

pub mod backfill;
pub mod ops;
pub mod plan;
pub mod status;
pub mod validate;

use std::fmt;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Publisher rollup table.
pub const PUBLISHER_TABLE: &str = "publisher_activity";
/// View feeding [`PUBLISHER_TABLE`].
pub const PUBLISHER_VIEW: &str = "publisher_activity_mv";
/// Longform rollup table.
pub const LONGFORM_TABLE: &str = "longform_latest";
/// View feeding [`LONGFORM_TABLE`].
pub const LONGFORM_VIEW: &str = "longform_latest_mv";

/// `publisher_activity` TTL in days (`TTL period + INTERVAL 400 DAY`).
pub const PUBLISHER_TTL_DAYS: u32 = 400;

/// Largest `days` window `/stats/publishers` accepts.
///
/// Longer windows exceed the API's 20 s ClickHouse cap: since mid-2026 20-45
/// million distinct pubkeys are active per month, so a 366-day ranking
/// aggregated ~150M (month, pubkey) rows and took 39 s in production
/// (2026-10-12), while 90 days answers in a few seconds. The rollup still keeps
/// [`PUBLISHER_TTL_DAYS`] of history, so this cap can be raised without a
/// migration once long windows have a precomputed path.
pub const PUBLISHER_MAX_DAYS: u32 = 90;

/// Kinds left out of `publisher_activity`: signed by throwaway keys (one
/// pubkey per event), so they carry no publisher signal. Same list as
/// `daily_user_stats` (migration 005).
pub const EXCLUDED_PUBLISHER_KINDS: [u16; 2] = [445, 1059];

/// `publisher_activity.kind` value of the all-kinds rollup rows.
pub const ALL_KINDS: i32 = -1;

/// `live_min_indexed_at` written by the backfill (2106-02-07 06:28:15), so
/// `min(live_min_indexed_at)` only sees rows the view wrote.
pub const BACKFILL_SENTINEL_INDEXED_AT: u32 = u32::MAX;

/// Effective event time for publisher activity: events dated more than a day
/// after they were indexed count on the day after ingestion.
pub const ACTIVITY_AT_EXPR: &str = "least(created_at, indexed_at + toIntervalDay(1))";

/// Grain, kind scope pairs every event is counted under (day and month grain,
/// all-kinds and own-kind rollups).
const PUBLISHER_SLOTS_EXPR: &str =
    "[(1, -1), (1, toInt32(event_kind)), (2, -1), (2, toInt32(event_kind))]";

/// `grain` column type.
const GRAIN_TYPE: &str = "Enum8(''day'' = 1, ''month'' = 2)";

/// Winner key for replaceable events: newest `created_at`, then lowest id.
const WINNER_KEY_EXPR: &str = "CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')";

/// Payload kept for the latest longform version.
const LONGFORM_PAYLOAD_EXPR: &str = "CAST((id, created_at, toUInt64(length(content))), \
     'Tuple(String, DateTime, UInt64)')";

/// A rollup table filled by the operator tool.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// `publisher_activity`: per-day and per-month publisher counts.
    Publishers,
    /// `longform_latest`: latest kind 30023 version per (pubkey, d_tag).
    Longform,
}

impl Target {
    /// Every target.
    pub const ALL: [Self; 2] = [Self::Publishers, Self::Longform];

    /// Short stable name used in paths and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Publishers => "publishers",
            Self::Longform => "longform",
        }
    }

    /// Target table.
    pub fn table(self) -> &'static str {
        match self {
            Self::Publishers => PUBLISHER_TABLE,
            Self::Longform => LONGFORM_TABLE,
        }
    }

    /// Materialized view feeding the target.
    pub fn materialized_view(self) -> &'static str {
        match self {
            Self::Publishers => PUBLISHER_VIEW,
            Self::Longform => LONGFORM_VIEW,
        }
    }

    /// Whether re-inserting a source row changes the target. Publisher counts
    /// are sums, so each source row must be written exactly once; longform
    /// states are `argMin` and idempotent.
    pub fn is_additive(self) -> bool {
        matches!(self, Self::Publishers)
    }

    /// Source columns a backfill shard reads (for read-cost estimates).
    pub fn source_columns(self) -> &'static [&'static str] {
        match self {
            Self::Publishers => &["id", "pubkey", "kind", "created_at", "indexed_at"],
            Self::Longform => &[
                "id",
                "pubkey",
                "kind",
                "created_at",
                "indexed_at",
                "content",
                "d_tag",
            ],
        }
    }

    /// Target columns, in the order the backfill `SELECT` produces them.
    pub fn columns(self) -> &'static str {
        match self {
            Self::Publishers => {
                "grain, kind, period, pubkey, event_count, kinds, first_created_at, \
                 last_created_at, live_min_indexed_at, last_indexed_at"
            }
            Self::Longform => "pubkey, d_tag, latest, last_indexed_at",
        }
    }

    /// `INSERT ... SELECT` that writes the target rows for source events in
    /// `range_predicate` (a trusted, internally generated predicate such as an
    /// id-prefix range). Publisher backfills also need `bounds`.
    pub fn backfill_sql(
        self,
        database: &str,
        range_predicate: &str,
        bounds: Option<&PublisherBounds>,
    ) -> String {
        format!(
            "INSERT INTO {database}.{} ({}) {}",
            self.table(),
            self.columns(),
            self.backfill_select_sql(database, range_predicate, bounds)
        )
    }

    /// The `SELECT` part of [`Self::backfill_sql`] (also used to size a
    /// sample shard during planning).
    pub fn backfill_select_sql(
        self,
        database: &str,
        range_predicate: &str,
        bounds: Option<&PublisherBounds>,
    ) -> String {
        match self {
            Self::Publishers => {
                let bounds = bounds.expect("publisher backfill needs bounds");
                publisher_backfill_select_sql(database, range_predicate, bounds)
            }
            Self::Longform => format!(
                "SELECT pubkey, d_tag, \
                 argMinState({LONGFORM_PAYLOAD_EXPR}, {WINNER_KEY_EXPR}) AS latest, \
                 max(indexed_at) AS last_indexed_at \
                 FROM {database}.events_local \
                 WHERE kind = 30023 AND ({range_predicate}) \
                 GROUP BY pubkey, d_tag"
            ),
        }
    }

    /// Source rows a shard reads that can produce target rows (for plan
    /// statistics). Publishers count only rows before the view boundary.
    pub fn source_filter(self, bounds: Option<&PublisherBounds>) -> String {
        match (self, bounds) {
            (Self::Publishers, Some(bounds)) => format!(
                "kind NOT IN ({}) AND indexed_at < toDateTime({})",
                excluded_kinds_sql(),
                bounds.indexed_before
            ),
            (Self::Publishers, None) => format!("kind NOT IN ({})", excluded_kinds_sql()),
            (Self::Longform, _) => "kind = 30023".to_string(),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Source bounds of a publisher backfill, fixed when it is planned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherBounds {
    /// Read only rows with `indexed_at` below this (Unix seconds): the first
    /// second the view wrote. Rows at or after it are the view's.
    pub indexed_before: u32,
    /// Read only rows whose activity day is on or after this UTC day
    /// (the TTL window at planning time).
    pub window_start: NaiveDate,
}

impl PublisherBounds {
    /// The TTL window start for a plan made on `today` (UTC): the oldest day
    /// the view still keeps, `today - (TTL - 1)`.
    pub fn window_start_for(today: NaiveDate) -> NaiveDate {
        today - chrono::Days::new(u64::from(PUBLISHER_TTL_DAYS - 1))
    }
}

/// SQL list of [`EXCLUDED_PUBLISHER_KINDS`].
fn excluded_kinds_sql() -> String {
    EXCLUDED_PUBLISHER_KINDS
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn publisher_backfill_select_sql(
    database: &str,
    range_predicate: &str,
    bounds: &PublisherBounds,
) -> String {
    let excluded = excluded_kinds_sql();
    let indexed_before = bounds.indexed_before;
    let window_start = bounds.window_start.format("%Y-%m-%d");
    format!(
        "SELECT \
         CAST(slot.1, '{GRAIN_TYPE}') AS grain, \
         slot.2 AS kind, \
         if(slot.1 = 1, toDate(activity_at, 'UTC'), toStartOfMonth(activity_at, 'UTC')) AS period, \
         pubkey, \
         count() AS event_count, \
         groupUniqArray(event_kind) AS kinds, \
         min(created_at) AS first_created_at, \
         max(created_at) AS last_created_at, \
         toDateTime({BACKFILL_SENTINEL_INDEXED_AT}) AS live_min_indexed_at, \
         max(indexed_at) AS last_indexed_at \
         FROM ( \
         SELECT pubkey, kind AS event_kind, created_at, indexed_at, \
         {ACTIVITY_AT_EXPR} AS activity_at \
         FROM {database}.events_local \
         WHERE kind NOT IN ({excluded}) \
         AND indexed_at < toDateTime({indexed_before}) \
         AND toDate(activity_at, 'UTC') >= toDate('{window_start}') \
         AND ({range_predicate}) \
         ) \
         ARRAY JOIN {PUBLISHER_SLOTS_EXPR} AS slot \
         GROUP BY grain, kind, period, pubkey"
    )
}

// ---------------------------------------------------------------------------
// API reads
// ---------------------------------------------------------------------------

/// Top publishers over `[start_day, +inf)`, where `start_day` is the UTC day
/// containing `now() - days`.
///
/// Binds, in order: `days` (u32), `kind` (i32, [`ALL_KINDS`] for no filter),
/// `limit` (u32). Day rows cover `start_day` up to the next month boundary
/// (`split_day`) and month rows cover `split_day` onwards, so every event in
/// the window is counted exactly once. Reads collapse unmerged rows with
/// `GROUP BY`, so they never depend on merges.
pub fn publishers_sql() -> String {
    format!(
        "WITH \
         toDate(now() - toIntervalDay(?), 'UTC') AS start_day, \
         if(toStartOfMonth(start_day) = start_day, start_day, \
         toStartOfMonth(start_day) + toIntervalMonth(1)) AS split_day \
         SELECT \
         a.pubkey AS pubkey, \
         sum(a.event_count) AS event_count, \
         toUInt64(length(groupUniqArrayArray(a.kinds))) AS kinds_count, \
         toUInt32(min(a.first_created_at)) AS first_event, \
         toUInt32(max(a.last_created_at)) AS last_event \
         FROM {PUBLISHER_TABLE} AS a \
         WHERE a.kind = ? \
         AND ((a.grain = 'day' AND a.period >= start_day AND a.period < split_day) \
         OR (a.grain = 'month' AND a.period >= split_day)) \
         GROUP BY pubkey \
         ORDER BY event_count DESC, pubkey ASC \
         LIMIT ?"
    )
}

/// Why a `/stats/publishers` request cannot be served from the rollup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublisherParamError {
    /// `days` outside `1..=PUBLISHER_MAX_DAYS`.
    Days(u32),
    /// A kind left out of the rollup.
    ExcludedKind(u16),
}

impl fmt::Display for PublisherParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Days(days) => write!(
                f,
                "days must be between 1 and {PUBLISHER_MAX_DAYS}, got {days}"
            ),
            Self::ExcludedKind(kind) => write!(
                f,
                "kind {kind} is signed by throwaway keys and is not ranked by publisher"
            ),
        }
    }
}

/// Validate `/stats/publishers` parameters and return the `kind` bind value.
pub fn publisher_params(days: u32, kind: Option<u16>) -> Result<i32, PublisherParamError> {
    if !(1..=PUBLISHER_MAX_DAYS).contains(&days) {
        return Err(PublisherParamError::Days(days));
    }
    match kind {
        Some(kind) if EXCLUDED_PUBLISHER_KINDS.contains(&kind) => {
            Err(PublisherParamError::ExcludedKind(kind))
        }
        Some(kind) => Ok(i32::from(kind)),
        None => Ok(ALL_KINDS),
    }
}

/// Longform totals over the latest version of every (pubkey, d_tag).
///
/// With `windowed`, binds one `days` (u32) value and keeps only articles whose
/// latest version has `created_at >= now() - days` (no upper bound, like the
/// old query). Returns `(articles, authors, total_content_length)`.
pub fn longform_sql(windowed: bool) -> String {
    let window = if windowed {
        "WHERE tupleElement(latest_version, 2) >= now() - toIntervalDay(?)"
    } else {
        ""
    };
    format!(
        "SELECT \
         count() AS articles_count, \
         uniqExact(pubkey) AS unique_authors, \
         sum(tupleElement(latest_version, 3)) AS total_content_length \
         FROM ( \
         SELECT pubkey, argMinMerge(latest) AS latest_version \
         FROM {LONGFORM_TABLE} \
         GROUP BY pubkey, d_tag \
         ) \
         {window}"
    )
}

/// Whether a ClickHouse error says a table does not exist (code 60), as when
/// migration 019 has not been applied.
pub fn is_unknown_table(error: &clickhouse::error::Error) -> bool {
    match error {
        clickhouse::error::Error::BadResponse(message) => {
            message.contains("UNKNOWN_TABLE") || message.starts_with("Code: 60.")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATION: &str =
        include_str!("../../../../docs/migrations/prepared/019_api_publishers_longform.sql");

    fn bounds() -> PublisherBounds {
        PublisherBounds {
            indexed_before: 1_700_000_000,
            window_start: NaiveDate::from_ymd_opt(2025, 9, 9).unwrap(),
        }
    }

    #[test]
    fn backfill_sql_is_scoped_and_bind_free() {
        for target in Target::ALL {
            let sql = target.backfill_sql("scratch", "id >= 'a'", Some(&bounds()));
            assert!(sql.starts_with(&format!("INSERT INTO scratch.{}", target.table())));
            assert!(sql.contains("FROM scratch.events_local"));
            assert!(sql.contains("AND (id >= 'a')"));
            assert!(!sql.contains("nostr."));
            assert!(!sql.contains('?'), "{target} backfill SQL must not bind");
        }
    }

    #[test]
    fn publisher_backfill_reads_only_rows_before_the_view_boundary() {
        let sql = Target::Publishers.backfill_sql("db", "1", Some(&bounds()));
        assert!(sql.contains("indexed_at < toDateTime(1700000000)"));
        assert!(sql.contains("toDate(activity_at, 'UTC') >= toDate('2025-09-09')"));
        assert!(sql.contains("kind NOT IN (445, 1059)"));
        assert!(sql.contains("toDateTime(4294967295) AS live_min_indexed_at"));
    }

    #[test]
    fn backfill_matches_the_view_expressions() {
        // The view and backfill must aggregate the same expressions; compare
        // the distinctive fragments against the migration text.
        let migration = MIGRATION.replace('\n', " ");
        let migration = migration.split_whitespace().collect::<Vec<_>>().join(" ");
        for fragment in [
            PUBLISHER_SLOTS_EXPR,
            "least(created_at, indexed_at + toIntervalDay(1)) AS activity_at",
            "if(slot.1 = 1, toDate(activity_at, 'UTC'), toStartOfMonth(activity_at, 'UTC')) AS period",
            "CAST(slot.1, 'Enum8(''day'' = 1, ''month'' = 2)') AS grain",
            "groupUniqArray(event_kind) AS kinds",
            "WHERE kind NOT IN (445, 1059)",
            "TTL period + INTERVAL 400 DAY",
            "toDate(activity_at, 'UTC') > toDate(now(), 'UTC') - 400",
            "CAST((id, created_at, toUInt64(length(content))), 'Tuple(String, DateTime, UInt64)')",
            "CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')",
            "WHERE kind = 30023",
        ] {
            assert!(migration.contains(fragment), "migration lacks {fragment}");
        }
        let window =
            PublisherBounds::window_start_for(NaiveDate::from_ymd_opt(2026, 10, 12).unwrap());
        // The view keeps day > today - 400, i.e. day >= today - 399.
        assert_eq!(window, NaiveDate::from_ymd_opt(2025, 9, 8).unwrap());
    }

    #[test]
    fn publisher_params_bound_days_and_kinds() {
        assert_eq!(publisher_params(30, None), Ok(ALL_KINDS));
        assert_eq!(publisher_params(1, Some(1)), Ok(1));
        assert_eq!(publisher_params(90, Some(30023)), Ok(30023));
        assert_eq!(publisher_params(0, None), Err(PublisherParamError::Days(0)));
        assert_eq!(
            publisher_params(91, None),
            Err(PublisherParamError::Days(91))
        );
        assert_eq!(
            publisher_params(30, Some(1059)),
            Err(PublisherParamError::ExcludedKind(1059))
        );
        assert_eq!(
            publisher_params(30, Some(445)),
            Err(PublisherParamError::ExcludedKind(445))
        );
        assert!(
            PublisherParamError::Days(0)
                .to_string()
                .contains("between 1 and 90")
        );
    }

    #[test]
    fn read_sql_binds_in_order() {
        let sql = publishers_sql();
        assert_eq!(sql.matches('?').count(), 3);
        let days = sql.find("toIntervalDay(?)").unwrap();
        let kind = sql.find("a.kind = ?").unwrap();
        let limit = sql.find("LIMIT ?").unwrap();
        assert!(days < kind && kind < limit);
        assert!(sql.contains("ORDER BY event_count DESC, pubkey ASC"));

        assert_eq!(longform_sql(false).matches('?').count(), 0);
        assert_eq!(longform_sql(true).matches('?').count(), 1);
    }

    #[test]
    fn unknown_table_errors_are_recognised() {
        let missing = clickhouse::error::Error::BadResponse(
            "Code: 60. DB::Exception: Unknown table expression identifier 'publisher_activity' \
             in scope SELECT 1. (UNKNOWN_TABLE) (version 24.8.14.39 (official build))"
                .into(),
        );
        assert!(is_unknown_table(&missing));
        let timeout = clickhouse::error::Error::BadResponse(
            "Code: 159. DB::Exception: Timeout exceeded. (TIMEOUT_EXCEEDED)".into(),
        );
        assert!(!is_unknown_table(&timeout));
        assert!(!is_unknown_table(&clickhouse::error::Error::TimedOut));
    }
}
