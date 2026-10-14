//! SQL for migration 022's `hourly_pubkeys` rollup and the
//! `/api/v1/stats/activity/hourly` read.
//!
//! `/stats/activity/hourly` reports, per hour of the day, the events and the
//! distinct authors of the last N days. Migration 021's `kind_activity` has
//! the counts (hour rows, last 35 days) but its hour rows carry an empty
//! author sketch, and no other rollup keeps authors per hour. So
//! `docs/migrations/prepared/022_hourly_pubkeys.sql` adds one small
//! insert-fed table: per kind, and for all kinds (`kind = -1`), and per UTC
//! hour of `created_at` (last 35 days), a `uniqCombined64` sketch of the
//! authors. Sketches are idempotent (a set union), so the table holds no sums
//! and its backfill follows migration 017's model (no view boundary; overlap
//! and re-runs are harmless).
//!
//! [`hourly_activity_sql`] takes the counts from `kind_activity`'s hour rows
//! (exact) and the authors from `hourly_pubkeys` (approximate), both from the
//! UTC hour containing `now() - days`, with no upper bound like the old
//! `created_at >= now() - N days`. `days` is limited to
//! [`HOURLY_MAX_DAYS`] = 30 (the hour rows keep 35 days); it used to be
//! silently capped at 90.
//!
//! The `SELECT`s here are the single Rust source for the backfill, the API
//! read, the validator and the integration tests; unit tests check that the
//! migration file contains the same expressions.

use std::fmt;

use super::ALL_KINDS;
use super::kind_activity::{
    HOUR_ROWS_DAYS, HOUR_WINDOW_MAX_DAYS, KIND_ACTIVITY_TABLE, hour_window,
};

/// Hourly author sketches table.
pub const HOURLY_PUBKEYS_TABLE: &str = "hourly_pubkeys";
/// View feeding [`HOURLY_PUBKEYS_TABLE`].
pub const HOURLY_PUBKEYS_VIEW: &str = "hourly_pubkeys_mv";

/// Default `days` of `/stats/activity/hourly`.
pub const HOURLY_DEFAULT_DAYS: u32 = 7;

/// Largest `days` of `/stats/activity/hourly`: the hour rows of both tables
/// keep [`HOUR_ROWS_DAYS`] days, enough for 30 days plus the partial first
/// hour.
pub const HOURLY_MAX_DAYS: u32 = HOUR_WINDOW_MAX_DAYS;

/// The UTC hour of `created_at`.
pub const HOUR_EXPR: &str = "toStartOfHour(created_at, 'UTC')";

/// Rows the view and the backfill write: hours newer than `now() - 35 days`
/// (older hour rows would expire at once and are never read).
pub fn recent_hour_predicate() -> String {
    format!("{HOUR_EXPR} > now() - toIntervalDay({HOUR_ROWS_DAYS})")
}

/// Every event is written under its own kind and under the all-kinds row.
pub const SCOPES_EXPR: &str = "[toInt32(event_kind), -1]";

/// Author sketch of a (kind, hour).
pub const PUBKEYS_EXPR: &str = "uniqCombined64State(pubkey)";

/// Hourly author sketches of the rows in `range_predicate` (same expressions
/// as the view; no `indexed_at` bound: sketches are idempotent).
pub fn backfill_select_sql(database: &str, range_predicate: &str) -> String {
    format!(
        "SELECT scope AS kind, \
         {HOUR_EXPR} AS period, \
         {PUBKEYS_EXPR} AS pubkeys, \
         max(indexed_at) AS last_indexed_at \
         FROM ( \
         SELECT kind AS event_kind, pubkey, created_at, indexed_at \
         FROM {database}.events_local \
         WHERE {} AND ({range_predicate}) \
         ) \
         ARRAY JOIN {SCOPES_EXPR} AS scope \
         GROUP BY kind, period",
        recent_hour_predicate()
    )
}

/// Why a `/stats/activity/hourly` request cannot be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourlyDaysError(pub u32);

impl fmt::Display for HourlyDaysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "days must be between 1 and {HOURLY_MAX_DAYS}, got {} (hourly rollups keep \
             {HOUR_ROWS_DAYS} days)",
            self.0
        )
    }
}

/// `days` of `/stats/activity/hourly`: default [`HOURLY_DEFAULT_DAYS`],
/// `1..=HOURLY_MAX_DAYS`.
pub fn hourly_days(days: Option<u32>) -> Result<u32, HourlyDaysError> {
    let days = days.unwrap_or(HOURLY_DEFAULT_DAYS);
    if (1..=HOURLY_MAX_DAYS).contains(&days) {
        Ok(days)
    } else {
        Err(HourlyDaysError(days))
    }
}

/// `/stats/activity/hourly` over `days` (validated by [`hourly_days`]): per
/// UTC hour of the day, `event_count` (exact, `kind_activity` hour rows),
/// `unique_pubkeys` (merged `hourly_pubkeys` sketches) and
/// `avg_per_day = event_count / days`, in the `HourlyActivityRow` shape.
///
/// Binds, in order: with `kind`, the kind (u16) for `kind_activity`; then
/// the `hourly_pubkeys` kind (i32: the kind, or [`ALL_KINDS`]). See
/// [`hourly_pubkeys_kind`].
pub fn hourly_activity_sql(days: u32, kind: bool) -> String {
    let kind_filter = if kind { "r.kind = ? AND " } else { "" };
    format!(
        "SELECT c.hour AS hour, c.event_count AS event_count, \
         p.unique_pubkeys AS unique_pubkeys, \
         toFloat64(c.event_count) / {days} AS avg_per_day \
         FROM ( \
         SELECT toHour(r.period, 'UTC') AS hour, toUInt64(sum(r.event_count)) AS event_count \
         FROM {KIND_ACTIVITY_TABLE} AS r \
         WHERE {kind_filter}{} \
         GROUP BY hour \
         ) AS c \
         LEFT JOIN ( \
         SELECT toHour(h.period, 'UTC') AS hour, \
         uniqCombined64Merge(h.pubkeys) AS unique_pubkeys \
         FROM {HOURLY_PUBKEYS_TABLE} AS h \
         WHERE h.kind = ? AND h.period >= toStartOfHour(now() - toIntervalDay({days}), 'UTC') \
         GROUP BY hour \
         ) AS p ON c.hour = p.hour \
         ORDER BY hour ASC",
        hour_window(days)
    )
}

/// The `hourly_pubkeys.kind` bind value for an optional kind filter.
pub fn hourly_pubkeys_kind(kind: Option<u16>) -> i32 {
    kind.map_or(ALL_KINDS, i32::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATION: &str =
        include_str!("../../../../docs/migrations/prepared/022_hourly_pubkeys.sql");
    const ROLLBACK: &str =
        include_str!("../../../../docs/migrations/rollback/022_drop_hourly_pubkeys.sql");

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
        let sql = backfill_select_sql("db", "id < '1'");
        for fragment in [
            HOUR_EXPR,
            PUBKEYS_EXPR,
            SCOPES_EXPR,
            &recent_hour_predicate(),
            "scope AS kind",
            "ARRAY JOIN [toInt32(event_kind), -1] AS scope",
            "SELECT kind AS event_kind, pubkey, created_at, indexed_at",
            "max(indexed_at) AS last_indexed_at",
            "GROUP BY kind, period",
        ] {
            assert!(
                migration.contains(&squash(fragment)),
                "migration lacks {fragment}"
            );
            assert!(
                squash(&sql).contains(&squash(fragment)),
                "{fragment}: {sql}"
            );
        }
        for fragment in [
            "pubkeys AggregateFunction(uniqCombined64, String)",
            "ORDER BY (kind, period)",
            "TTL least(toDate(period, 'UTC') + INTERVAL 35 DAY, toDate('2106-01-01'))",
            "toStartOfHour(created_at, 'UTC') > now() - toIntervalDay(35)",
        ] {
            assert!(
                migration.contains(&squash(fragment)),
                "migration lacks {fragment}"
            );
        }
        assert!(sql.contains("FROM db.events_local WHERE"));
        assert!(sql.contains("AND (id < '1')"));
        assert!(!sql.contains('?') && !sql.contains("nostr."));
        assert!(squash(ROLLBACK).contains("DROPVIEWIFEXISTSnostr.hourly_pubkeys_mv"));
        assert!(squash(ROLLBACK).contains("DROPTABLEIFEXISTSnostr.hourly_pubkeys"));
    }

    #[test]
    fn hourly_days_default_to_seven_and_stay_inside_the_hour_rows() {
        assert_eq!(hourly_days(None), Ok(7));
        assert_eq!(hourly_days(Some(1)), Ok(1));
        assert_eq!(hourly_days(Some(30)), Ok(30));
        assert_eq!(hourly_days(Some(0)), Err(HourlyDaysError(0)));
        assert_eq!(hourly_days(Some(31)), Err(HourlyDaysError(31)));
        assert_eq!(hourly_days(Some(90)), Err(HourlyDaysError(90)));
        assert!(
            HourlyDaysError(90)
                .to_string()
                .contains("between 1 and 30, got 90")
        );
        const { assert!(HOURLY_MAX_DAYS < HOUR_ROWS_DAYS) };
    }

    #[test]
    fn hourly_sql_binds_the_kinds_in_order() {
        let all = hourly_activity_sql(7, false);
        assert_eq!(all.matches('?').count(), 1);
        assert!(all.contains(
            "WHERE r.grain = 'hour' AND r.period >= toStartOfHour(now() - toIntervalDay(7), 'UTC')"
        ));
        assert!(all.contains(
            "WHERE h.kind = ? AND h.period >= toStartOfHour(now() - toIntervalDay(7), 'UTC')"
        ));
        assert!(all.contains("toFloat64(c.event_count) / 7 AS avg_per_day"));
        let one = hourly_activity_sql(30, true);
        assert_eq!(one.matches('?').count(), 2);
        assert!(one.find("r.kind = ?").unwrap() < one.find("h.kind = ?").unwrap());
        assert!(one.ends_with("ORDER BY hour ASC"));
        assert_eq!(hourly_pubkeys_kind(None), -1);
        assert_eq!(hourly_pubkeys_kind(Some(1059)), 1059);
    }
}
