//! Validation of migration 022's `hourly_pubkeys`, and of the all-kinds
//! author unions that `/stats/events` now reads from 021's `kind_activity`,
//! against exact distinct pubkeys from `events_local`.
//!
//! Consistency, as for the other sketches: the rollups are read first and
//! `events_local` is scanned after them. An author first indexed before the
//! cutoff was in the rollups when they were read (`validate` refuses while an
//! insert that started before the cutoff is running); an author counted by the
//! scan is a superset of what the rollups saw. So every estimate must lie in
//! `[settled, total]` within [`sketch_tolerance`].
//!
//! Compared, from the UTC day `--hour-days` days before the cutoff on (every
//! hour the endpoint reads lies within 30 days, so `--hour-days` is at most
//! 30). By default that is 30 days while `hourly_pubkeys_mv` is younger than
//! 35 days: until then hours the API reads may hold events indexed before the
//! view, which only the backfill writes, so a missing or partial backfill
//! shows up as `missing` rows. Once the view is older, every readable hour
//! was captured by the view and 2 days suffice. While the view is younger
//! than 35 days a backfill that is not complete per the checkpoints also fails
//! the target, whatever the window.
//!
//! - every (kind, hour) and (all kinds, hour) sketch of `hourly_pubkeys`;
//! - per UTC day, the union of every kind's `kind_activity` day sketch (what
//!   `/stats/events` without `kind` merges) and the day's event count
//!   (bracketed like the other summed shares).
//!
//! One scan: `PREWHERE created_at >= start` reads `created_at` for the whole
//! table and `kind`, `pubkey`, `indexed_at` of every granule holding a recent
//! event, which is nearly every granule (ids are random): about one full read
//! of `pubkey` (~150 GiB uncompressed), the read the old hourly endpoint made
//! on every refresh. Memory: ~60 B per distinct (kind, hour, pubkey),
//! (hour, pubkey) and (day, pubkey), spilling to disk beyond half of
//! `--max-memory-gib`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::hourly_pubkeys::{HOURLY_MAX_DAYS, HOURLY_PUBKEYS_TABLE, HOURLY_PUBKEYS_VIEW};
use super::kind_activity::{HOUR_ROWS_DAYS, KIND_ACTIVITY_TABLE};
use super::validate::{
    Ctx, Outcome, TargetReport, classify_shares, sketch_error, sketch_tolerance,
};
use super::validate_derived::{Worst, describe_worst, track_worst};
use super::{Target, ops};

/// `log_comment` of the exact distinct-pubkeys scan.
pub const SCAN_LOG_COMMENT: &str = "api-rollups:validate:hourly-pubkeys";

/// Grain codes of the scan.
const HOUR: u8 = 1;
const DAY: u8 = 2;

/// The all-kinds scope (`hourly_pubkeys.kind = -1`).
const ALL: i32 = super::ALL_KINDS;

/// One (kind, hour) sketch of `hourly_pubkeys`.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct HourSketch {
    kind: i32,
    period: u32,
    pubkeys: u64,
}

/// One UTC day of `kind_activity`, all kinds merged.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct DayUnion {
    period: u32,
    events: u64,
    pubkeys: u64,
}

/// One (scope, grain, period) of the exact scan.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct Scan {
    kind: i32,
    grain: u8,
    period: u32,
    keys_total: u64,
    keys_settled: u64,
    rows_total: u64,
    rows_settled: u64,
}

/// `--hour-days` once the view is older than the hour rows' 35 days.
const SETTLED_HOUR_DAYS: u32 = 2;

/// Whether hours the API reads may still depend on the backfill: the view
/// (created at `view_created_at`, `None` if missing) is younger than the
/// hour rows' [`HOUR_ROWS_DAYS`] at `cutoff`.
pub(super) fn backfill_matters(view_created_at: Option<u32>, cutoff: u32) -> bool {
    view_created_at
        .is_none_or(|t0| u64::from(t0) + u64::from(HOUR_ROWS_DAYS) * 86_400 > u64::from(cutoff))
}

/// Days compared by default: all [`HOURLY_MAX_DAYS`] while the backfill
/// matters, else [`SETTLED_HOUR_DAYS`].
pub(super) fn default_hour_days(backfill_matters: bool) -> u32 {
    if backfill_matters {
        HOURLY_MAX_DAYS
    } else {
        SETTLED_HOUR_DAYS
    }
}

/// First compared second: the start of the UTC day `days` days before
/// `cutoff`.
pub(super) fn first_compared_day(cutoff: u32, days: u32) -> u32 {
    cutoff.saturating_sub(days * 86_400) / 86_400 * 86_400
}

/// Label of a compared group.
fn label(kind: i32, grain: u8, period: u32) -> String {
    let at = chrono::DateTime::from_timestamp(i64::from(period), 0).expect("u32 is in range");
    let scope = if kind == ALL {
        "all kinds".to_string()
    } else {
        format!("kind {kind}")
    };
    match grain {
        HOUR => format!("{scope} hour {}", at.format("%Y-%m-%d %H:00")),
        _ => format!("{scope} day {}", at.format("%Y-%m-%d")),
    }
}

/// Outcome of a sketch estimate against the exact interval.
fn sketch_outcome(approx: u64, lo: u64, hi: u64, sigmas: f64) -> Outcome {
    if sketch_error(approx, lo, hi) <= sketch_tolerance(hi, sigmas) {
        Outcome::Match
    } else if approx < lo {
        Outcome::Missing
    } else {
        Outcome::Excess
    }
}

pub(super) async fn hourly_pubkeys(
    ctx: &Ctx<'_>,
    backfill_complete: Option<bool>,
) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let mut report = TargetReport::new(Target::HourlyPubkeys);
    let view_created_at = ops::table_created_at(ctx.client, db, HOURLY_PUBKEYS_VIEW).await?;
    let matters = backfill_matters(view_created_at, ctx.cutoff);
    let days = o
        .hour_days
        .unwrap_or_else(|| default_hour_days(matters))
        .clamp(1, HOURLY_MAX_DAYS);
    let start = first_compared_day(ctx.cutoff, days);
    match (backfill_complete, matters) {
        (Some(false), true) => report.record(
            "hourly-pubkeys backfill",
            Outcome::Missing,
            "the backfill is not complete per the checkpoints (or not planned here) while \
             hours the API reads predate the view: finish `backfill --migration 022` first"
                .to_string(),
            o.max_examples,
        ),
        (Some(false), false) => report.notes.push(
            "the backfill is not complete per the checkpoints, but the view is older than 35 \
             days: every hour the API reads was captured by the view"
                .to_string(),
        ),
        (Some(true), _) => report
            .notes
            .push("backfill complete per the checkpoints".to_string()),
        (None, _) => {}
    }

    // Rollups first.
    let hours: Vec<HourSketch> = ctx
        .client
        .query(&format!(
            "SELECT h.kind AS kind, toUInt32(h.period) AS period, \
             uniqCombined64Merge(h.pubkeys) AS pubkeys \
             FROM {db}.{HOURLY_PUBKEYS_TABLE} AS h \
             WHERE h.period >= toDateTime(?) \
             GROUP BY h.kind, h.period"
        ))
        .bind(start)
        .fetch_all()
        .await
        .context("reading hourly_pubkeys")?;
    let unions: Vec<DayUnion> = ctx
        .client
        .query(&format!(
            "SELECT toUInt32(r.period) AS period, toUInt64(sum(r.event_count)) AS events, \
             uniqCombined64Merge(r.pubkeys) AS pubkeys \
             FROM {db}.{KIND_ACTIVITY_TABLE} AS r \
             WHERE r.grain = 'day' AND r.period >= toDateTime(?) \
             GROUP BY r.period"
        ))
        .bind(start)
        .fetch_all()
        .await
        .context("reading kind_activity day rows")?;

    // Then the exact scan.
    let scan: Vec<Scan> = ctx
        .client
        .query(&format!(
            "SELECT s.1 AS kind, s.2 AS grain, toUInt32(s.3) AS period, \
             count() AS keys_total, countIf(first_indexed < toDateTime(?)) AS keys_settled, \
             toUInt64(sum(n)) AS rows_total, toUInt64(sum(n_settled)) AS rows_settled \
             FROM ( \
             SELECT s, key, min(i) AS first_indexed, count() AS n, \
             countIf(i < toDateTime(?)) AS n_settled \
             FROM ( \
             SELECT toInt32(kind) AS k, toStartOfHour(created_at, 'UTC') AS h, \
             toStartOfDay(created_at, 'UTC') AS d, sipHash128(pubkey) AS key, \
             indexed_at AS i \
             FROM {db}.events_local PREWHERE created_at >= toDateTime(?) \
             ) \
             ARRAY JOIN [(k, toUInt8({HOUR}), h), (toInt32({ALL}), toUInt8({HOUR}), h), \
             (toInt32({ALL}), toUInt8({DAY}), d)] AS s \
             GROUP BY s, key \
             ) \
             GROUP BY s"
        ))
        .bind(ctx.cutoff)
        .bind(ctx.cutoff)
        .bind(start)
        .with_option("log_comment", SCAN_LOG_COMMENT)
        .fetch_all()
        .await
        .context("counting exact distinct pubkeys per hour and day")?;
    report.source_has_rows = !scan.is_empty();

    let expected: BTreeMap<(i32, u8, u32), &Scan> = scan
        .iter()
        .map(|s| ((s.kind, s.grain, s.period), s))
        .collect();
    let mut actual: BTreeMap<(i32, u8, u32), (u64, Option<u64>)> = BTreeMap::new();
    for h in &hours {
        actual.insert((h.kind, HOUR, h.period), (h.pubkeys, None));
    }
    for u in &unions {
        actual.insert((ALL, DAY, u.period), (u.pubkeys, Some(u.events)));
    }

    let keys: BTreeSet<&(i32, u8, u32)> = expected.keys().chain(actual.keys()).collect();
    let (mut worst_hour, mut worst_day): (Worst, Worst) = (None, None);
    let (mut hour_groups, mut day_groups) = (0usize, 0usize);
    for key in keys {
        let &(kind, grain, period) = key;
        let place = label(kind, grain, period);
        let (lo, hi, rows_settled, rows_total) = expected.get(key).map_or((0, 0, 0, 0), |s| {
            (s.keys_settled, s.keys_total, s.rows_settled, s.rows_total)
        });
        let (approx, events) = actual.get(key).copied().unwrap_or((0, None));
        let outcome = sketch_outcome(approx, lo, hi, o.pubkeys_sigmas);
        report.compared += 1;
        let detail = format!(
            "approximate {approx} vs exact {lo}..{hi}: error {}, allowed {}",
            sketch_error(approx, lo, hi),
            sketch_tolerance(hi, o.pubkeys_sigmas)
        );
        if grain == HOUR {
            hour_groups += 1;
            track_worst(&mut worst_hour, place.clone(), approx, lo, hi);
            report.record(&format!("{place} pubkeys"), outcome, detail, o.max_examples);
        } else {
            day_groups += 1;
            track_worst(&mut worst_day, place.clone(), approx, lo, hi);
            report.record(
                &format!("{place} kind_activity pubkeys"),
                outcome,
                detail,
                o.max_examples,
            );
            let events = events.unwrap_or(0);
            report.record(
                &format!("{place} kind_activity events"),
                classify_shares(0, events, 0, rows_settled, rows_total, false),
                format!("rollup {events}; source settled {rows_settled}, total {rows_total}"),
                o.max_examples,
            );
        }
    }
    report.sampled = report.compared;
    report.notes.push(format!(
        "hourly_pubkeys: {hour_groups} (kind, hour) and (all kinds, hour) sketches from {start} \
         ({days} days before the cutoff, whole UTC days) compared with exact counts: largest \
         error {}",
        describe_worst(&worst_hour)
    ));
    report.notes.push(format!(
        "kind_activity all-kinds day unions (/stats/events): {day_groups} days compared: \
         largest error {}",
        describe_worst(&worst_day)
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_window_is_compared_while_the_backfill_matters() {
        let cutoff = 1_791_763_200;
        assert!(backfill_matters(None, cutoff));
        assert!(backfill_matters(Some(cutoff - 5 * 86_400), cutoff));
        assert!(backfill_matters(Some(cutoff - 35 * 86_400 + 1), cutoff));
        assert!(!backfill_matters(Some(cutoff - 35 * 86_400), cutoff));
        assert_eq!(default_hour_days(true), 30);
        assert_eq!(default_hour_days(false), 2);
    }

    #[test]
    fn windows_start_on_whole_utc_days() {
        // 2026-10-13 10:00 UTC.
        let cutoff = 1_791_763_200 + 86_400 + 36_000;
        assert_eq!(first_compared_day(cutoff, 2), 1_791_763_200 - 86_400);
        assert_eq!(first_compared_day(cutoff, 0), 1_791_763_200 + 86_400);
        assert_eq!(first_compared_day(100, 30), 0);
        assert_eq!(
            label(ALL, HOUR, 1_791_763_200 + 3600),
            "all kinds hour 2026-10-12 01:00"
        );
        assert_eq!(label(7, DAY, 1_791_763_200), "kind 7 day 2026-10-12");
    }

    #[test]
    fn sketches_are_bracketed_by_the_cutoff_and_the_scan() {
        assert_eq!(sketch_outcome(100, 100, 100, 4.0), Outcome::Match);
        assert_eq!(sketch_outcome(105, 100, 110, 4.0), Outcome::Match);
        // Exact below ~4k values: slack 2.
        assert_eq!(sketch_outcome(97, 100, 110, 4.0), Outcome::Missing);
        assert_eq!(sketch_outcome(113, 100, 110, 4.0), Outcome::Excess);
        // A missing row of a small hour is caught.
        assert_eq!(sketch_outcome(0, 3, 3, 4.0), Outcome::Missing);
        assert_eq!(sketch_outcome(0, 2, 2, 4.0), Outcome::Match);
        // 1M authors: 4 x 0.2873%.
        assert_eq!(
            sketch_outcome(1_011_000, 1_000_000, 1_000_000, 4.0),
            Outcome::Match
        );
        assert_eq!(
            sketch_outcome(1_012_000, 1_000_000, 1_000_000, 4.0),
            Outcome::Excess
        );
    }
}
