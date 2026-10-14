//! SQL for migration 021's `kind_activity` rollup.
//!
//! `docs/migrations/prepared/021_kind_activity.sql` creates one insert-fed
//! table with, per kind, UTC hour (last 35 days), day and month of
//! `created_at`: the event count and the summed content length (sums, written
//! exactly once with the 019 boundary model) and a `uniqCombined64` sketch of
//! the authors (day and month rows). The `SELECT`s here are the single Rust
//! source for the backfill, the API reads, the validator and the integration
//! tests; unit tests check that the migration file contains the same text.
//!
//! It serves:
//!
//! - `GET /api/v1/kinds/{kind}` ([`kind_detail_sql`]): totals, first/last and
//!   approximate pubkeys from 020's `kind_totals`; average content length from
//!   the day rows; 24 h / 7 d / 30 d counts from the hour rows, starting at the
//!   UTC hour that contains `now() - N days` (so N days plus less than an
//!   hour), with no upper bound, like the old `created_at >= now() - N days`.
//! - `GET /api/v1/kinds/{kind}/activity` ([`kind_series_sql`]): day and month
//!   rows (weeks from day rows), periods starting at or before `now()`.
//!
//! Periods use the event's own `created_at` in UTC, unclamped (unlike 019's
//! `least(created_at, indexed_at + 1 day)`), because both endpoints filter on
//! `created_at` relative to `now()`: see the migration's header.

use super::BACKFILL_SENTINEL_INDEXED_AT;
use super::derived::KINDS_TABLE;

/// Kind activity table.
pub const KIND_ACTIVITY_TABLE: &str = "kind_activity";
/// View feeding [`KIND_ACTIVITY_TABLE`].
pub const KIND_ACTIVITY_VIEW: &str = "kind_activity_mv";

/// Days of hour rows kept and written (`TTL toDate(period) + 35 DAY` for
/// `grain = 'hour'`). Reads need 30 days plus the partial first hour.
pub const HOUR_ROWS_DAYS: u32 = 35;

/// Longest window read from hour rows (`events_30d`).
pub const HOUR_WINDOW_MAX_DAYS: u32 = 30;

/// `grain` column type.
const GRAIN_TYPE: &str = "Enum8(''hour'' = 1, ''day'' = 2, ''month'' = 3)";

/// `origin` column type (as `kind_totals`).
const ORIGIN_TYPE: &str = "Enum8(''backfill'' = 1, ''view'' = 2)";

/// The UTC period of `created_at` for grain slot 1 (hour), 2 (day), 3 (month).
pub const PERIOD_EXPR: &str = "multiIf(slot = 1, toStartOfHour(created_at, 'UTC'), \
     slot = 2, toStartOfDay(created_at, 'UTC'), \
     toDateTime(toStartOfMonth(created_at, 'UTC'), 'UTC'))";

/// Authors of day and month rows; hour rows carry an empty sketch.
pub const PUBKEYS_EXPR: &str = "uniqCombined64StateIf(pubkey, slot != 1)";

/// Content length in bytes, as the old `avg(length(content))` measured.
pub const CONTENT_BYTES_EXPR: &str = "sum(length(content))";

/// Grain slots of a row: hour, day and month while its hour is newer than
/// `floor - 35 days`, else day and month. The view uses `now()`, the backfill
/// the planned view boundary.
pub fn slots_expr(floor: &str) -> String {
    format!(
        "if(toStartOfHour(created_at, 'UTC') > {floor} - toIntervalDay({HOUR_ROWS_DAYS}), \
         [1, 2, 3], [2, 3])"
    )
}

/// Kind activity of the rows in `range_predicate` indexed before the view
/// boundary (same expressions as the view).
pub fn backfill_select_sql(database: &str, range_predicate: &str, indexed_before: u32) -> String {
    let slots = slots_expr(&format!("toDateTime({indexed_before})"));
    format!(
        "SELECT kind, \
         CAST(slot, '{GRAIN_TYPE}') AS grain, \
         {PERIOD_EXPR} AS period, \
         CAST('backfill', '{ORIGIN_TYPE}') AS origin, \
         count() AS event_count, \
         {CONTENT_BYTES_EXPR} AS content_bytes, \
         {PUBKEYS_EXPR} AS pubkeys, \
         toDateTime({BACKFILL_SENTINEL_INDEXED_AT}) AS live_min_indexed_at, \
         max(indexed_at) AS last_indexed_at \
         FROM ( \
         SELECT kind, pubkey, content, created_at, indexed_at \
         FROM {database}.events_local \
         WHERE indexed_at < toDateTime({indexed_before}) AND ({range_predicate}) \
         ) \
         ARRAY JOIN {slots} AS slot \
         GROUP BY kind, grain, period"
    )
}

// ---------------------------------------------------------------------------
// API reads
// ---------------------------------------------------------------------------

/// Hour-row window of `events_{days}` (and of `/stats/throughput` and
/// `/stats/activity/hourly`): from the UTC hour containing `now() - days`, no
/// upper bound.
pub(crate) fn hour_window(days: u32) -> String {
    format!("r.grain = 'hour' AND r.period >= toStartOfHour(now() - toIntervalDay({days}), 'UTC')")
}

/// `GET /api/v1/kinds/{kind}`: one row, or none when `kind_totals` has no
/// such kind. Binds `kind` (u16) twice.
///
/// Columns: kind, event_count, unique_pubkeys, first_seen, last_seen (from
/// `kind_totals`), events_all and content_bytes (day rows, for the average
/// content length), events_24h, events_7d, events_30d (hour rows). Reads
/// collapse unmerged rows and both origins with aggregates, so they never
/// depend on merges.
pub fn kind_detail_sql() -> String {
    format!(
        "SELECT t.kind AS kind, t.event_count AS event_count, \
         t.unique_pubkeys AS unique_pubkeys, t.first_seen AS first_seen, \
         t.last_seen AS last_seen, a.events_all AS events_all, \
         a.content_bytes AS content_bytes, a.events_24h AS events_24h, \
         a.events_7d AS events_7d, a.events_30d AS events_30d \
         FROM ( \
         SELECT k.kind AS kind, sum(k.event_count) AS event_count, \
         uniqCombined64Merge(k.pubkeys) AS unique_pubkeys, \
         toUInt32(min(k.first_created_at)) AS first_seen, \
         toUInt32(max(k.last_created_at)) AS last_seen \
         FROM {KINDS_TABLE} AS k WHERE k.kind = ? GROUP BY k.kind \
         ) AS t \
         CROSS JOIN ( \
         SELECT toUInt64(sumIf(r.event_count, r.grain = 'day')) AS events_all, \
         toUInt64(sumIf(r.content_bytes, r.grain = 'day')) AS content_bytes, \
         toUInt64(sumIf(r.event_count, {})) AS events_24h, \
         toUInt64(sumIf(r.event_count, {})) AS events_7d, \
         toUInt64(sumIf(r.event_count, {})) AS events_30d \
         FROM {KIND_ACTIVITY_TABLE} AS r \
         WHERE r.kind = ? AND r.grain IN ('hour', 'day') \
         ) AS a",
        hour_window(1),
        hour_window(7),
        hour_window(HOUR_WINDOW_MAX_DAYS),
    )
}

/// Average content length in bytes; 0 when no events are counted.
pub fn average_content_length(content_bytes: u64, events: u64) -> f64 {
    if events == 0 {
        0.0
    } else {
        content_bytes as f64 / events as f64
    }
}

/// Period of `GET /api/v1/kinds/{kind}/activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesGrain {
    /// UTC days (day rows).
    Day,
    /// Weeks starting on Monday (day rows grouped).
    Week,
    /// UTC months (month rows).
    Month,
}

impl SeriesGrain {
    /// Parse `group_by` (`None` is `day`).
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            None | Some("day") => Some(Self::Day),
            Some("week") => Some(Self::Week),
            Some("month") => Some(Self::Month),
            Some(_) => None,
        }
    }

    /// Name used in cache keys.
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    /// Largest `limit` for this grain.
    pub fn max_limit(self) -> u32 {
        match self {
            Self::Day => 365,
            Self::Week => 52,
            Self::Month => 120,
        }
    }

    /// (rows read, period expression over `r.period`).
    fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::Day => ("day", "toDate(r.period, 'UTC')"),
            Self::Week => ("day", "toMonday(toDate(r.period, 'UTC'))"),
            Self::Month => ("month", "toDate(r.period, 'UTC')"),
        }
    }
}

/// First UTC day of the `limit` most recent periods, the current one
/// included (`limit` 0 behaves like 1).
pub fn series_window_start(grain: SeriesGrain, limit: u32) -> String {
    let (start, unit) = match grain {
        SeriesGrain::Day => ("toDate(now(), 'UTC')", "DAY"),
        SeriesGrain::Week => ("toMonday(toDate(now(), 'UTC'))", "WEEK"),
        SeriesGrain::Month => ("toStartOfMonth(toDate(now(), 'UTC'))", "MONTH"),
    };
    format!("{start} - INTERVAL {} {unit}", limit.saturating_sub(1))
}

/// `GET /api/v1/kinds/{kind}/activity`: the `limit` most recent periods that
/// started at or before `now()`, newest first, in the `KindTimeSeriesRow`
/// shape (`period` as `YYYY-MM-DD`, `event_count`, `unique_pubkeys`). Binds
/// `kind` (u16). `unique_pubkeys` merges the period's sketches (approximate).
pub fn kind_series_sql(grain: SeriesGrain, limit: u32) -> String {
    let (rows, period) = grain.parts();
    let start = series_window_start(grain, limit);
    format!(
        "SELECT toString(p) AS period, toUInt64(sum(n)) AS event_count, \
         uniqCombined64Merge(s) AS unique_pubkeys \
         FROM ( \
         SELECT {period} AS p, r.event_count AS n, r.pubkeys AS s \
         FROM {KIND_ACTIVITY_TABLE} AS r \
         WHERE r.kind = ? AND r.grain = '{rows}' \
         AND r.period >= toDateTime({start}, 'UTC') AND r.period <= now() \
         ) \
         GROUP BY p \
         ORDER BY p DESC \
         LIMIT {limit}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATION: &str =
        include_str!("../../../../docs/migrations/prepared/021_kind_activity.sql");
    const ROLLBACK: &str =
        include_str!("../../../../docs/migrations/rollback/021_drop_kind_activity.sql");

    /// SQL without comments and whitespace.
    fn squash(sql: &str) -> String {
        sql.lines()
            .map(|l| l.find("--").map_or(l, |i| &l[..i]))
            .collect::<String>()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn backfill_matches_the_view_expressions() {
        let migration = squash(MIGRATION);
        for fragment in [
            PERIOD_EXPR,
            PUBKEYS_EXPR,
            CONTENT_BYTES_EXPR,
            &slots_expr("now()"),
            "CAST(slot, 'Enum8(''hour'' = 1, ''day'' = 2, ''month'' = 3)') AS grain",
            "CAST('view', 'Enum8(''backfill'' = 1, ''view'' = 2)') AS origin",
            "pubkeys AggregateFunction(uniqCombined64, String)",
            "ORDER BY (kind, grain, period, origin)",
            "TTL least(toDate(period, 'UTC') + INTERVAL 35 DAY, toDate('2106-01-01')) \
             DELETE WHERE grain = 'hour'",
            "GROUP BY kind, grain, period",
            "min(indexed_at) AS live_min_indexed_at",
        ] {
            assert!(
                migration.contains(&squash(fragment)),
                "migration lacks {fragment}"
            );
        }
        let sql = backfill_select_sql("db", "id < '1'", 1_700_000_000);
        for fragment in [
            PERIOD_EXPR,
            PUBKEYS_EXPR,
            "WHERE indexed_at < toDateTime(1700000000) AND (id < '1')",
            "toDateTime(4294967295) AS live_min_indexed_at",
            "CAST('backfill', 'Enum8(''backfill'' = 1, ''view'' = 2)') AS origin",
            "> toDateTime(1700000000) - toIntervalDay(35), [1, 2, 3], [2, 3]",
            "FROM db.events_local",
        ] {
            assert!(sql.contains(fragment), "{fragment}: {sql}");
        }
        assert!(!sql.contains('?') && !sql.contains("nostr."));
        assert!(squash(ROLLBACK).contains("DROPVIEWIFEXISTSnostr.kind_activity_mv"));
        assert!(squash(ROLLBACK).contains("DROPTABLEIFEXISTSnostr.kind_activity"));
    }

    #[test]
    fn detail_reads_bind_the_kind_twice_and_cover_the_hour_rows() {
        let sql = kind_detail_sql();
        assert_eq!(sql.matches('?').count(), 2);
        assert!(sql.contains("FROM kind_totals AS k WHERE k.kind = ?"));
        assert!(sql.contains("WHERE r.kind = ? AND r.grain IN ('hour', 'day')"));
        for days in [1, 7, 30] {
            assert!(sql.contains(&format!(
                "r.period >= toStartOfHour(now() - toIntervalDay({days}), 'UTC')"
            )));
        }
        // The 30-day window plus its partial first hour stays inside the
        // hour rows' TTL.
        const { assert!(HOUR_WINDOW_MAX_DAYS < HOUR_ROWS_DAYS) };
        assert_eq!(average_content_length(10, 4), 2.5);
        assert_eq!(average_content_length(0, 0), 0.0);
    }

    #[test]
    fn series_windows_include_the_current_period() {
        assert_eq!(
            series_window_start(SeriesGrain::Day, 30),
            "toDate(now(), 'UTC') - INTERVAL 29 DAY"
        );
        assert_eq!(
            series_window_start(SeriesGrain::Week, 1),
            "toMonday(toDate(now(), 'UTC')) - INTERVAL 0 WEEK"
        );
        assert_eq!(
            series_window_start(SeriesGrain::Month, 24),
            "toStartOfMonth(toDate(now(), 'UTC')) - INTERVAL 23 MONTH"
        );
        assert_eq!(
            series_window_start(SeriesGrain::Day, 0),
            "toDate(now(), 'UTC') - INTERVAL 0 DAY"
        );
        let week = kind_series_sql(SeriesGrain::Week, 12);
        assert_eq!(week.matches('?').count(), 1);
        assert!(week.contains("toMonday(toDate(r.period, 'UTC')) AS p"));
        assert!(week.contains("r.grain = 'day'"));
        assert!(week.ends_with("LIMIT 12"));
        assert!(kind_series_sql(SeriesGrain::Month, 3).contains("r.grain = 'month'"));
        assert_eq!(SeriesGrain::parse(None), Some(SeriesGrain::Day));
        assert_eq!(SeriesGrain::parse(Some("week")), Some(SeriesGrain::Week));
        assert_eq!(SeriesGrain::parse(Some("year")), None);
        assert_eq!(SeriesGrain::Month.max_limit(), 120);
    }
}
