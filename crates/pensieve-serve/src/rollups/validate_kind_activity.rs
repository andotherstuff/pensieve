//! Validation of migration 021's `kind_activity` against `events_local`.
//!
//! Consistency: the rollup is read first and `events_local` is scanned after
//! it. Rows indexed before the cutoff were committed, view rows included,
//! before the rollup was read (`validate` refuses while an insert that
//! started before the cutoff is running), and the scan sees a superset of
//! what the rollup saw. So every summed share is bracketed exactly as for
//! `kind_totals` ([`classify_shares`]): the backfill share (`origin =
//! 'backfill'`) must equal the rows indexed before the planned view boundary,
//! the view share must lie between the settled rows and all rows since the
//! boundary. No group is skipped as fresh.
//!
//! - **Counts, every kind and period.** One scan of `kind`, `created_at` and
//!   `indexed_at` grouped by kind, UTC day and, for rows from the UTC hour 30
//!   days before the cutoff on, UTC hour. Day, month (summed from days) and
//!   hour groups are compared with every day and month row and those hour
//!   rows of the rollup (older hour rows are outside what the API reads and
//!   expire after 35 days).
//! - **Content bytes and author sketches, sampled kinds.** `content` can only
//!   be read by granule, and a kind with N events touches up to N granules, so
//!   seeded random kinds are sampled until their events reach
//!   `--content-budget-events`. One scan (`PREWHERE kind IN (...)`) recomputes
//!   their per-day content bytes (bracketed as above) and exact distinct
//!   pubkeys (first indexed before the cutoff .. after the rollup read), which
//!   the day rows' `uniqCombined64` sketches must match within
//!   [`sketch_tolerance`].

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::Datelike;
use serde::Deserialize;

use super::Target;
use super::kind_activity::{HOUR_WINDOW_MAX_DAYS, KIND_ACTIVITY_TABLE};
use super::plan::{Plan, utc_day};
use super::validate::{
    Ctx, Outcome, TargetReport, classify_shares, sketch_error, sketch_tolerance,
};
use super::validate_derived::{Worst, describe_worst, seeded_order, track_worst};

/// `log_comment` of the all-kinds count scan.
pub const COUNTS_LOG_COMMENT: &str = "api-rollups:validate:kind-activity:counts";
/// `log_comment` of the sampled content and sketch scan.
pub const CONTENT_LOG_COMMENT: &str = "api-rollups:validate:kind-activity:content";

/// `grain` values of `kind_activity`.
const HOUR: u8 = 1;
const DAY: u8 = 2;
const MONTH: u8 = 3;

/// One (kind, grain, period) of the rollup, origins split.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct RollupShares {
    kind: u16,
    grain: u8,
    period: u32,
    backfill: u64,
    view: u64,
}

/// One (kind, day, hour) of the count scan; `hour` is 0 for rows older than
/// the compared hours.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct CountScan {
    kind: u16,
    day: u32,
    hour: u32,
    before: u64,
    settled: u64,
    total: u64,
}

/// Rows (or bytes) before the boundary, before the cutoff, and in total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Bracket {
    pub(super) before: u64,
    pub(super) settled: u64,
    pub(super) total: u64,
}

impl Bracket {
    fn add(&mut self, before: u64, settled: u64, total: u64) {
        self.before += before;
        self.settled += settled;
        self.total += total;
    }
}

/// One (kind, day) of the sampled rollup rows.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct DayContent {
    kind: u16,
    period: u32,
    backfill: u64,
    view: u64,
    pubkeys: u64,
}

/// One (kind, day) of the sampled content scan.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct ContentScan {
    kind: u16,
    day: u32,
    before: u64,
    settled: u64,
    total: u64,
    keys_total: u64,
    keys_settled: u64,
}

/// Unix seconds of the first second of the UTC month containing `day_start`.
pub(super) fn month_of(day_start: u32) -> u32 {
    let first = utc_day(day_start).with_day(1).expect("day 1 exists");
    first
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .timestamp() as u32
}

/// First compared hour: the UTC hour `HOUR_WINDOW_MAX_DAYS` before `cutoff`.
pub(super) fn first_compared_hour(cutoff: u32) -> u32 {
    cutoff.saturating_sub(HOUR_WINDOW_MAX_DAYS * 86_400) / 3600 * 3600
}

/// Expected (kind, grain, period) brackets from the count scan.
fn expected_counts(scan: &[CountScan]) -> BTreeMap<(u16, u8, u32), Bracket> {
    let mut out: BTreeMap<(u16, u8, u32), Bracket> = BTreeMap::new();
    for s in scan {
        out.entry((s.kind, DAY, s.day))
            .or_default()
            .add(s.before, s.settled, s.total);
        out.entry((s.kind, MONTH, month_of(s.day)))
            .or_default()
            .add(s.before, s.settled, s.total);
        if s.hour != 0 {
            out.entry((s.kind, HOUR, s.hour))
                .or_default()
                .add(s.before, s.settled, s.total);
        }
    }
    out
}

/// Label of a (kind, grain, period).
fn label(kind: u16, grain: u8, period: u32) -> String {
    let at = chrono::DateTime::from_timestamp(i64::from(period), 0).expect("u32 is in range");
    let when = match grain {
        HOUR => at.format("hour %Y-%m-%d %H:00").to_string(),
        DAY => at.format("day %Y-%m-%d").to_string(),
        _ => at.format("month %Y-%m").to_string(),
    };
    format!("kind {kind} {when}")
}

pub(super) async fn kind_activity(ctx: &Ctx<'_>, plan: Option<&Plan>) -> Result<TargetReport> {
    let mut report = TargetReport::new(Target::KindActivity);
    let boundary = plan.and_then(|p| p.source_bounds().indexed_before);
    match boundary {
        Some(c) => report.notes.push(format!(
            "backfill shares compared with rows indexed before the boundary {c}"
        )),
        None => report.notes.push(
            "no kind-activity plan found: only totals compared (no backfill/view split)"
                .to_string(),
        ),
    }
    let counts = compare_counts(ctx, boundary, &mut report).await?;
    compare_content(ctx, boundary, &counts, &mut report).await?;
    Ok(report)
}

/// Every (kind, grain, period) count. Returns the events per kind (day rows
/// of the rollup) for the content sample.
async fn compare_counts(
    ctx: &Ctx<'_>,
    boundary: Option<u32>,
    report: &mut TargetReport,
) -> Result<BTreeMap<u16, u64>> {
    let (db, o) = (ctx.db, ctx.options);
    let hour_start = first_compared_hour(ctx.cutoff);
    let rollup: Vec<RollupShares> = ctx
        .client
        .query(&format!(
            "SELECT r.kind AS kind, toUInt8(r.grain) AS grain, toUInt32(r.period) AS period, \
             toUInt64(sumIf(r.event_count, r.origin = 'backfill')) AS backfill, \
             toUInt64(sumIf(r.event_count, r.origin = 'view')) AS view \
             FROM {db}.{KIND_ACTIVITY_TABLE} AS r \
             WHERE r.grain != 'hour' OR r.period >= toDateTime(?) \
             GROUP BY r.kind, r.grain, r.period"
        ))
        .bind(hour_start)
        .fetch_all()
        .await
        .context("reading kind_activity")?;
    let scan: Vec<CountScan> = ctx
        .client
        .query(&format!(
            "SELECT k AS kind, toUInt32(d) AS day, toUInt32(h) AS hour, \
             countIf(i < toDateTime(?)) AS before, \
             countIf(i < toDateTime(?)) AS settled, count() AS total \
             FROM ( \
             SELECT kind AS k, toStartOfDay(created_at, 'UTC') AS d, \
             if(created_at >= toDateTime(?), toStartOfHour(created_at, 'UTC'), \
             toDateTime(0, 'UTC')) AS h, indexed_at AS i \
             FROM {db}.events_local \
             ) \
             GROUP BY k, d, h"
        ))
        .bind(boundary.unwrap_or(0))
        .bind(ctx.cutoff)
        .bind(hour_start)
        .with_option("log_comment", COUNTS_LOG_COMMENT)
        .fetch_all()
        .await
        .context("scanning events_local by kind and period")?;
    report.source_has_rows = !scan.is_empty();

    let expected = expected_counts(&scan);
    let actual: BTreeMap<(u16, u8, u32), (u64, u64)> = rollup
        .iter()
        .map(|r| ((r.kind, r.grain, r.period), (r.backfill, r.view)))
        .collect();
    let mut events: BTreeMap<u16, u64> = BTreeMap::new();
    for ((kind, grain, _), (backfill, view)) in &actual {
        if *grain == DAY {
            *events.entry(*kind).or_default() += backfill + view;
        }
    }
    let keys: BTreeSet<&(u16, u8, u32)> = expected.keys().chain(actual.keys()).collect();
    let mut by_grain = [0usize; 3];
    for key in keys {
        let &(kind, grain, period) = key;
        let e = expected.get(key).copied().unwrap_or_default();
        let (backfill, view) = actual.get(key).copied().unwrap_or((0, 0));
        let outcome = classify_shares(
            backfill,
            view,
            e.before,
            e.settled,
            e.total,
            boundary.is_some(),
        );
        let detail = if outcome == Outcome::Match {
            String::new()
        } else {
            format!(
                "rollup backfill {backfill} + view {view}; source before boundary {}, settled \
                 {}, total {}",
                e.before, e.settled, e.total
            )
        };
        report.compared += 1;
        by_grain[usize::from(grain.clamp(HOUR, MONTH) - 1)] += 1;
        report.record(&label(kind, grain, period), outcome, detail, o.max_examples);
    }
    report.sampled = report.compared;
    report.notes.push(format!(
        "event counts of {} hour (from {hour_start}), {} day and {} month groups compared",
        by_grain[0], by_grain[1], by_grain[2]
    ));
    Ok(events)
}

/// Seeded random kinds whose events add up to at most `budget`.
pub(super) fn content_sample(events: &BTreeMap<u16, u64>, budget: u64, seed: u64) -> Vec<u16> {
    let mut kinds: Vec<(u16, u64)> = events
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| (*k, *n))
        .collect();
    kinds.sort_by_key(|(k, _)| (seeded_order(seed, *k), *k));
    let mut used = 0u64;
    let mut out = Vec::new();
    for (kind, n) in kinds {
        if used + n <= budget {
            used += n;
            out.push(kind);
        }
    }
    out.sort_unstable();
    out
}

/// Per-day content bytes and author sketches of sampled kinds.
async fn compare_content(
    ctx: &Ctx<'_>,
    boundary: Option<u32>,
    events: &BTreeMap<u16, u64>,
    report: &mut TargetReport,
) -> Result<()> {
    let (db, o) = (ctx.db, ctx.options);
    let kinds = content_sample(events, o.content_budget_events, o.seed);
    if kinds.is_empty() {
        report.notes.push(format!(
            "content bytes and day sketches not compared: no kinds within \
             --content-budget-events {}",
            o.content_budget_events
        ));
        return Ok(());
    }
    let list = kinds
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let rollup: Vec<DayContent> = ctx
        .client
        .query(&format!(
            "SELECT r.kind AS kind, toUInt32(r.period) AS period, \
             toUInt64(sumIf(r.content_bytes, r.origin = 'backfill')) AS backfill, \
             toUInt64(sumIf(r.content_bytes, r.origin = 'view')) AS view, \
             uniqCombined64Merge(r.pubkeys) AS pubkeys \
             FROM {db}.{KIND_ACTIVITY_TABLE} AS r \
             WHERE r.grain = 'day' AND r.kind IN ({list}) \
             GROUP BY r.kind, r.period"
        ))
        .fetch_all()
        .await
        .context("reading sampled kind_activity day rows")?;
    let scan: Vec<ContentScan> = ctx
        .client
        .query(&format!(
            "SELECT k AS kind, toUInt32(d) AS day, toUInt64(sum(b)) AS before, \
             toUInt64(sum(s)) AS settled, toUInt64(sum(t)) AS total, \
             count() AS keys_total, countIf(first_indexed < toDateTime(?)) AS keys_settled \
             FROM ( \
             SELECT kind AS k, toStartOfDay(created_at, 'UTC') AS d, sipHash128(pubkey) AS key, \
             min(indexed_at) AS first_indexed, \
             sumIf(length(content), indexed_at < toDateTime(?)) AS b, \
             sumIf(length(content), indexed_at < toDateTime(?)) AS s, \
             sum(length(content)) AS t \
             FROM {db}.events_local PREWHERE kind IN ({list}) \
             GROUP BY k, d, key \
             ) \
             GROUP BY k, d"
        ))
        .bind(ctx.cutoff)
        .bind(boundary.unwrap_or(0))
        .bind(ctx.cutoff)
        .with_option("log_comment", CONTENT_LOG_COMMENT)
        .fetch_all()
        .await
        .context("recomputing content bytes and authors of the sampled kinds")?;

    let expected: BTreeMap<(u16, u32), &ContentScan> =
        scan.iter().map(|s| ((s.kind, s.day), s)).collect();
    let actual: BTreeMap<(u16, u32), &DayContent> =
        rollup.iter().map(|r| ((r.kind, r.period), r)).collect();
    let keys: BTreeSet<&(u16, u32)> = expected.keys().chain(actual.keys()).collect();
    let mut worst: Worst = None;
    let groups = keys.len();
    for key in keys {
        let &(kind, day) = key;
        let place = label(kind, DAY, day);
        let (before, settled, total, keys_settled, keys_total) =
            expected.get(key).map_or((0, 0, 0, 0, 0), |s| {
                (s.before, s.settled, s.total, s.keys_settled, s.keys_total)
            });
        let (backfill, view, pubkeys) = actual
            .get(key)
            .map_or((0, 0, 0), |r| (r.backfill, r.view, r.pubkeys));
        let outcome = classify_shares(backfill, view, before, settled, total, boundary.is_some());
        report.record(
            &format!("{place} content_bytes"),
            outcome,
            format!(
                "rollup backfill {backfill} + view {view} bytes; source before boundary \
                 {before}, settled {settled}, total {total}"
            ),
            o.max_examples,
        );
        let error = sketch_error(pubkeys, keys_settled, keys_total);
        let allowed = sketch_tolerance(keys_total, o.pubkeys_sigmas);
        track_worst(&mut worst, place.clone(), pubkeys, keys_settled, keys_total);
        report.record(
            &format!("{place} pubkeys"),
            if error > allowed {
                Outcome::Mismatch
            } else {
                Outcome::Match
            },
            format!(
                "approximate {pubkeys} vs exact {keys_settled}..{keys_total}: error {error}, \
                 allowed {allowed}"
            ),
            o.max_examples,
        );
    }
    let sampled_events: u64 = kinds.iter().map(|k| events[k]).sum();
    report.notes.push(format!(
        "content bytes and author sketches of {groups} (kind, day) groups of {} kinds ({list}; \
         {sampled_events} events, budget {}) compared: largest sketch error {}",
        kinds.len(),
        o.content_budget_events,
        describe_worst(&worst)
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(kind: u16, day: u32, hour: u32, before: u64, settled: u64, total: u64) -> CountScan {
        CountScan {
            kind,
            day,
            hour,
            before,
            settled,
            total,
        }
    }

    #[test]
    fn months_and_hours_are_summed_from_the_scan() {
        // 2026-10-12 and 2026-10-13 (UTC), one hour on the 13th.
        let d12 = 1_791_763_200;
        let d13 = d12 + 86_400;
        let scan = [
            count(1, d12, 0, 2, 3, 4),
            count(1, d13, 0, 1, 1, 1),
            count(1, d13, d13 + 3600, 0, 5, 7),
            count(7, d13, d13 + 3600, 1, 1, 1),
        ];
        let e = expected_counts(&scan);
        let oct = month_of(d12);
        assert_eq!(oct, 1_790_812_800, "2026-10-01");
        assert_eq!(month_of(d13), oct);
        assert_eq!(
            e[&(1, MONTH, oct)],
            Bracket {
                before: 3,
                settled: 9,
                total: 12
            }
        );
        assert_eq!(
            e[&(1, DAY, d13)],
            Bracket {
                before: 1,
                settled: 6,
                total: 8
            }
        );
        assert_eq!(e[&(1, HOUR, d13 + 3600)].total, 7);
        assert!(!e.contains_key(&(1, HOUR, 0)));
        assert_eq!(e.len(), 7);
        assert_eq!(label(1, HOUR, d13 + 3600), "kind 1 hour 2026-10-13 01:00");
        assert_eq!(label(1, MONTH, oct), "kind 1 month 2026-10");
        assert_eq!(first_compared_hour(d13 + 5000), d13 + 3600 - 30 * 86_400);
    }

    #[test]
    fn content_sample_stays_within_the_budget() {
        let events: BTreeMap<u16, u64> = [
            (1, 1_000_000),
            (7, 40),
            (9, 30),
            (11, 20),
            (13, 0),
            (15, 10),
        ]
        .into_iter()
        .collect();
        let sample = content_sample(&events, 60, 19);
        let used: u64 = sample.iter().map(|k| events[k]).sum();
        assert!(used <= 60 && !sample.is_empty(), "{sample:?}");
        assert!(!sample.contains(&1) && !sample.contains(&13));
        assert_eq!(content_sample(&events, 60, 19), sample);
        assert!(content_sample(&events, 0, 19).is_empty());
        assert_eq!(content_sample(&events, 100, 19), vec![7, 9, 11, 15]);
    }
}
