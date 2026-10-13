//! Validation of migration 020's targets against direct oracles.
//!
//! **User flags** (`pubkey_flags`):
//! 1. Every pubkey of `pubkeys_with_profile_data` (`..._follows_data`) has a
//!    profile (follows) time in `pubkey_flags` and vice versa (anti-joins on
//!    `sipHash128(pubkey)` sets; a disagreement must persist over three reads
//!    to count, since the two tables are written by different views of one
//!    insert).
//! 2. Every `daily_user_stats` row of the last `--flag-days` days with a
//!    stored flag belongs to a pubkey flagged in `pubkey_flags`, so the
//!    cutover's join can never lose a flag the insert path set.
//! 3. For sampled pubkeys, every `daily_user_stats` (date, pubkey) of the last
//!    `--user-days` days is recomputed from `events_local`: counts as for
//!    publishers (no gap or double count across the `MODIFY QUERY` switch),
//!    and after the cutover `last_indexed_at` must equal the newest
//!    `indexed_at` of rows inserted after it. Pubkeys active around the
//!    recorded cutover time are always sampled.
//! 4. The daily active-users refresh is computed with migration 013's rule
//!    as deployed (`= 1`, see [`FlagRule::Legacy`]), with stored flags
//!    (`> 0`) and with the cutover's join; the last days are printed. Before
//!    the cutover the last two must agree exactly.
//!
//! **Relay lists** (`relay_list_latest`): sampled pubkeys' winners are
//! recomputed from all their kind 10002 versions (newest `created_at`, then
//! lowest id; 'r' tags as migration 014 selected them). The distribution the
//! cutover would publish is compared with the current `relay_distribution`
//! table (informational: the 014 rescan is up to 6 h old and broke
//! `created_at` ties arbitrarily).
//!
//! **Kinds** (`kind_totals`): the rollup is read, then `events_local` is
//! scanned once (`kind`, `indexed_at`, `created_at`). Rows indexed before the
//! cutoff were committed, view rows included, before the rollup was read (no
//! insert that started before the cutoff is running); rows the scan sees are
//! a superset of what the rollup saw. So the backfill share (`origin =
//! 'backfill'`) must equal the rows indexed before the planned view boundary
//! exactly, the view share must lie between the settled rows and all rows
//! since the boundary, and first/last `created_at` likewise.
//!
//! The approximate distinct pubkeys are compared with **exact** counts for a
//! sample of kinds (the largest by events and a seeded random choice of the
//! rest, `--pubkeys-kinds`, within `--pubkeys-max-exact`): one bounded scan
//! (`PREWHERE kind IN (...)`) counts the distinct pubkeys first indexed before
//! the cutoff, before the boundary, and in total. The estimate must lie in
//! `[settled, total]` within `max(--pubkeys-sigmas` standard errors`, 2)`, and
//! the backfill rows' estimate within the same tolerance of the exact count
//! before the boundary. (Until migration 021 the comparison was with the
//! hourly `kinds_stats_mv` snapshot, which fast-moving kinds outran, and it
//! stopped once the 020 cutover dropped that view.)

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::derived::{
    CUTOVER_JOIN_SETTINGS, FlagRule, Grain, KINDS_TABLE, NEVER, RELAY_DISTRIBUTION_VIEW,
    RELAY_LISTS_TABLE, RELAY_TAGS_EXPR, RelaySource, USER_FLAG_LOOKUPS, USER_FLAGS_TABLE,
    active_users_select_sql, relay_distribution_select_sql,
};
use super::plan::Plan;
use super::validate::{
    Ctx, Outcome, SKETCH_EXACT_UP_TO, SKETCH_RELATIVE_ERROR, SKETCH_SLACK, TargetReport,
    classify_count, classify_shares, id_range_predicate, placeholders, seeded_id_range,
    sketch_error, sketch_tolerance,
};
use super::{EXCLUDED_PUBLISHER_KINDS, Target, ops};

/// Disagreements listed per lookup comparison.
const MAX_DISAGREEMENTS: u64 = 10_000;

fn excluded_kinds() -> String {
    EXCLUDED_PUBLISHER_KINDS
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Days since 1970-01-01 as a date string.
fn day_label(day: u16) -> String {
    (chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch")
        + chrono::Days::new(u64::from(day)))
    .to_string()
}

// ---------------------------------------------------------------------------
// User flags
// ---------------------------------------------------------------------------

/// One `daily_user_stats` (date, pubkey), collapsed.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct DailyRow {
    pubkey: String,
    day: u16,
    event_count: u64,
    last_indexed_at: u32,
}

/// One (date, pubkey) recomputed from `events_local`.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct DailyOracle {
    pubkey: String,
    day: u16,
    rows: u64,
    events: u64,
    newest_indexed: u32,
}

/// One day of an active-users refresh.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
struct ActiveDay {
    day: u16,
    active_users: u64,
    has_profile: u64,
    has_follows: u64,
    has_both: u64,
    total_events: u64,
}

/// The flagged pubkeys of `pubkey_flags` for one flag, as a set of
/// `sipHash128` keys (16 bytes instead of a 64-byte hex string per entry).
fn flagged_keys(db: &str, since: &str) -> String {
    format!(
        "SELECT sipHash128(pubkey) FROM {db}.{USER_FLAGS_TABLE} FINAL \
         WHERE {since} < toDateTime({NEVER})"
    )
}

/// Pubkeys where a lookup and `pubkey_flags` disagree: (only in the lookup,
/// only in `pubkey_flags`). Two anti-joins, each building one hashed set.
async fn flag_disagreements(
    ctx: &Ctx<'_>,
    lookup: &str,
    since: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let db = ctx.db;
    let only_lookup: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT DISTINCT pubkey FROM {db}.{lookup} \
             WHERE sipHash128(pubkey) NOT IN ({}) LIMIT {MAX_DISAGREEMENTS}",
            flagged_keys(db, since)
        ))
        .fetch_all()
        .await
        .with_context(|| format!("listing {lookup} pubkeys missing from {USER_FLAGS_TABLE}"))?;
    let only_flags: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.{USER_FLAGS_TABLE} FINAL \
             WHERE {since} < toDateTime({NEVER}) \
             AND sipHash128(pubkey) NOT IN (SELECT sipHash128(pubkey) FROM {db}.{lookup}) \
             LIMIT {MAX_DISAGREEMENTS}"
        ))
        .fetch_all()
        .await
        .with_context(|| format!("listing {USER_FLAGS_TABLE} pubkeys missing from {lookup}"))?;
    Ok((
        only_lookup.into_iter().collect(),
        only_flags.into_iter().collect(),
    ))
}

pub(super) async fn user_flags(ctx: &Ctx<'_>, cutover_at: Option<u32>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let mut report = TargetReport::new(Target::UserFlags);

    // 1. Lookup tables and pubkey_flags hold the same pubkeys.
    for (lookup, since) in [
        (USER_FLAG_LOOKUPS[0], "profile_since"),
        (USER_FLAG_LOOKUPS[1], "follows_since"),
    ] {
        let matched: u64 = ctx
            .client
            .query(&format!(
                "SELECT uniqExact(pubkey) FROM {db}.{lookup} WHERE sipHash128(pubkey) IN ({})",
                flagged_keys(db, since)
            ))
            .fetch_one()
            .await
            .with_context(|| format!("comparing {lookup} with {USER_FLAGS_TABLE}"))?;
        // The two tables are written by different views of one insert, so a
        // disagreement only counts if it survives three reads.
        let (mut only_lookup, mut only_flags) = flag_disagreements(ctx, lookup, since).await?;
        for _ in 0..2 {
            if only_lookup.is_empty() && only_flags.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let (again_lookup, again_flags) = flag_disagreements(ctx, lookup, since).await?;
            only_lookup.retain(|p| again_lookup.contains(p));
            only_flags.retain(|p| again_flags.contains(p));
        }
        *report.rows.entry(Outcome::Match).or_default() += matched;
        report.compared += matched as usize + only_lookup.len() + only_flags.len();
        report.sampled += matched as usize + only_lookup.len() + only_flags.len();
        for p in &only_lookup {
            report.record(
                &format!("{p} {since}"),
                Outcome::Missing,
                format!("in {lookup} but not flagged in {USER_FLAGS_TABLE}"),
                o.max_examples,
            );
        }
        for p in &only_flags {
            report.record(
                &format!("{p} {since}"),
                Outcome::Excess,
                format!("flagged in {USER_FLAGS_TABLE} but not in {lookup}"),
                o.max_examples,
            );
        }
        report.notes.push(format!(
            "{lookup}: {matched} pubkeys match {USER_FLAGS_TABLE}.{since}"
        ));
    }

    // 2. Stored flags of recent rows are all backed by pubkey_flags.
    for (flag, since) in [
        ("has_profile", "profile_since"),
        ("has_follows", "follows_since"),
    ] {
        let (flagged, unbacked, example): (u64, u64, String) = ctx
            .client
            .query(&format!(
                "SELECT count(), countIf(unbacked), anyIf(pubkey, unbacked) FROM ( \
                 SELECT pubkey, sipHash128(pubkey) NOT IN ({}) AS unbacked \
                 FROM {db}.daily_user_stats \
                 WHERE date >= today() - ? AND date <= today() AND {flag} > 0)",
                flagged_keys(db, since)
            ))
            .bind(o.flag_days)
            .fetch_one()
            .await
            .with_context(|| format!("checking stored {flag} flags"))?;
        if unbacked > 0 {
            *report.rows.entry(Outcome::Missing).or_default() += unbacked;
            report.examples.push(super::validate::Difference {
                key: format!("{example} {flag}"),
                outcome: Outcome::Missing,
                detail: format!(
                    "{unbacked} daily_user_stats row(s) of the last {} days store {flag} > 0 \
                     for a pubkey without {since}; the cutover would drop their flag",
                    o.flag_days
                ),
            });
        }
        report.notes.push(format!(
            "{flagged} daily_user_stats rows of the last {} days store {flag} > 0; \
             {unbacked} lack {USER_FLAGS_TABLE}.{since}",
            o.flag_days
        ));
    }

    // 3. daily_user_stats counts for sampled pubkeys.
    daily_user_stats_sample(ctx, cutover_at, &mut report).await?;

    // 4. The daily active-users refresh under each rule.
    active_users_comparison(ctx, cutover_at, &mut report).await?;
    Ok(report)
}

async fn daily_user_stats_sample(
    ctx: &Ctx<'_>,
    cutover_at: Option<u32>,
    report: &mut TargetReport,
) -> Result<()> {
    let (db, o) = (ctx.db, ctx.options);
    let days = o.user_days;
    let excluded = excluded_kinds();
    let mut pubkeys: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.daily_user_stats \
             WHERE date >= today() - ? AND cityHash64(pubkey, toUInt64(?)) % 10000 < ? \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(days)
        .bind(o.seed)
        .bind(o.sample_permyriad)
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling daily_user_stats pubkeys")?;
    let (slice_start, slice_end) = seeded_id_range(o.seed);
    let slice = id_range_predicate(&slice_start, slice_end.as_deref());
    let source: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.events_local \
             WHERE {slice} AND kind NOT IN ({excluded}) AND toDate(created_at) >= today() - ? \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(days)
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling events_local pubkeys")?;
    pubkeys.extend(source);
    if let Some(at) = cutover_at
        && o.boundary_sample > 0
    {
        let lo = at.saturating_sub(o.boundary_window_secs);
        let hi = at.saturating_add(o.boundary_window_secs);
        let around: Vec<String> = ctx
            .client
            .query(&format!(
                "SELECT pubkey FROM {db}.events_local \
                 WHERE indexed_at >= toDateTime(?) AND indexed_at < toDateTime(?) \
                 AND kind NOT IN ({excluded}) AND toDate(created_at) >= today() - ? \
                 GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
            ))
            .bind(lo)
            .bind(hi)
            .bind(days)
            .bind(o.seed)
            .bind(o.boundary_sample as u64)
            .fetch_all()
            .await
            .context("sampling pubkeys around the cutover")?;
        report.notes.push(format!(
            "{} pubkeys sampled with events indexed in [{lo}, {hi}) around the cutover {at}",
            around.len()
        ));
        pubkeys.extend(around);
    }
    let mut seen = BTreeSet::new();
    pubkeys.retain(|p| seen.insert(p.clone()));

    // Skip pubkeys too active for a bounded oracle read.
    let mut totals: HashMap<String, u64> = HashMap::new();
    for chunk in pubkeys.chunks(o.batch.max(1)) {
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, toUInt64(sum(event_count)) FROM {db}.daily_user_stats \
             WHERE date >= today() - {days} AND pubkey IN ({}) GROUP BY pubkey",
            placeholders(chunk.len())
        ));
        for p in chunk {
            q = q.bind(p);
        }
        totals.extend(
            q.fetch_all::<(String, u64)>()
                .await
                .context("reading daily_user_stats totals")?,
        );
    }
    let light: Vec<String> = pubkeys
        .iter()
        .filter(|p| totals.get(*p).copied().unwrap_or(0) <= o.max_key_events)
        .cloned()
        .collect();
    report.skipped_heavy += pubkeys.len() - light.len();

    let mut compared = 0usize;
    for chunk in light.chunks(o.batch.max(1)) {
        let list = placeholders(chunk.len());
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, toUInt16(date) AS day, toUInt64(sum(event_count)) AS event_count, \
             toUInt32(max(last_indexed_at)) AS last_indexed_at \
             FROM {db}.daily_user_stats WHERE pubkey IN ({list}) AND date >= today() - {days} \
             GROUP BY pubkey, date"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let rollup: Vec<DailyRow> = q.fetch_all().await.context("reading daily_user_stats")?;
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, toUInt16(toDate(created_at)) AS day, count() AS rows, \
             uniqExact(id) AS events, toUInt32(max(indexed_at)) AS newest_indexed \
             FROM {db}.events_local \
             WHERE pubkey IN ({list}) AND kind NOT IN ({excluded}) \
             AND toDate(created_at) >= today() - {days} \
             GROUP BY pubkey, day"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let oracle: Vec<DailyOracle> = q.fetch_all().await.context("reading the oracle")?;

        let fresh: BTreeSet<&str> = oracle
            .iter()
            .filter(|g| g.newest_indexed >= ctx.cutoff)
            .map(|g| g.pubkey.as_str())
            .collect();
        report.skipped_fresh += fresh.len();
        compared += chunk.len() - fresh.len();
        let expected: BTreeMap<(&str, u16), &DailyOracle> = oracle
            .iter()
            .filter(|g| !fresh.contains(g.pubkey.as_str()))
            .map(|g| ((g.pubkey.as_str(), g.day), g))
            .collect();
        let actual: BTreeMap<(&str, u16), &DailyRow> = rollup
            .iter()
            .filter(|r| !fresh.contains(r.pubkey.as_str()))
            .map(|r| ((r.pubkey.as_str(), r.day), r))
            .collect();
        let keys: BTreeSet<&(&str, u16)> = expected.keys().chain(actual.keys()).collect();
        for key in keys {
            let label = format!("{} {}", key.0, day_label(key.1));
            let (outcome, detail) = match (expected.get(key), actual.get(key)) {
                (Some(e), Some(a)) => {
                    let mut outcome = classify_count(a.event_count, e.events, e.rows);
                    // Rows inserted after the cutover carry their newest
                    // indexed_at for the refreshes' flag join.
                    if outcome == Outcome::Match
                        && cutover_at.is_some_and(|c| e.newest_indexed > c)
                        && a.last_indexed_at != e.newest_indexed
                    {
                        outcome = Outcome::Mismatch;
                    }
                    (
                        outcome,
                        format!(
                            "daily_user_stats {} events, last_indexed_at {}; source {} events \
                             ({} rows), newest indexed_at {}",
                            a.event_count, a.last_indexed_at, e.events, e.rows, e.newest_indexed
                        ),
                    )
                }
                (Some(e), None) => (
                    Outcome::Missing,
                    format!("no daily_user_stats row; source has {} events", e.events),
                ),
                (None, Some(a)) => (
                    Outcome::Excess,
                    format!(
                        "daily_user_stats counts {} events the source does not have",
                        a.event_count
                    ),
                ),
                (None, None) => unreachable!("key came from one of the maps"),
            };
            report.record(&label, outcome, detail, o.max_examples);
        }
    }
    report.compared += compared;
    report.sampled += pubkeys.len();
    report.notes.push(format!(
        "daily_user_stats: {compared} sampled pubkeys compared over the last {days} days"
    ));
    Ok(())
}

async fn active_users_days(ctx: &Ctx<'_>, rule: FlagRule) -> Result<Vec<ActiveDay>> {
    let select = active_users_select_sql(Grain::Day, rule, ctx.db);
    let select = select.trim_end_matches(CUTOVER_JOIN_SETTINGS);
    ctx.client
        .query(&format!(
            "SELECT toUInt16(date) AS day, active_users, has_profile, has_follows, has_both, \
             total_events FROM ({select}) ORDER BY day {CUTOVER_JOIN_SETTINGS}"
        ))
        .fetch_all()
        .await
        .with_context(|| format!("computing active_users_daily with the {rule:?} rule"))
}

async fn active_users_comparison(
    ctx: &Ctx<'_>,
    cutover_at: Option<u32>,
    report: &mut TargetReport,
) -> Result<()> {
    let legacy = active_users_days(ctx, FlagRule::Legacy).await?;
    let stored = active_users_days(ctx, FlagRule::Stored).await?;
    let cutover = active_users_days(ctx, FlagRule::Cutover).await?;
    let by_day = |rows: &[ActiveDay]| -> BTreeMap<u16, ActiveDay> {
        rows.iter().map(|r| (r.day, r.clone())).collect()
    };
    let (legacy, stored, cutover) = (by_day(&legacy), by_day(&stored), by_day(&cutover));
    for day in cutover.keys().rev().take(3) {
        let show = |m: &BTreeMap<u16, ActiveDay>| {
            m.get(day).map_or_else(
                || "-".to_string(),
                |r| {
                    format!(
                        "active {} profile {} follows {} both {} events {}",
                        r.active_users, r.has_profile, r.has_follows, r.has_both, r.total_events
                    )
                },
            )
        };
        report.notes.push(format!(
            "active_users_daily {}: 013 as deployed (= 1): {}; stored flags (> 0): {}; \
             cutover join: {}",
            day_label(*day),
            show(&legacy),
            show(&stored),
            show(&cutover)
        ));
    }
    if cutover_at.is_none() {
        // Before the cutover no row carries last_indexed_at, so the join must
        // reproduce the stored flags exactly.
        for (day, s) in &stored {
            if cutover.get(day) != Some(s) {
                report.record(
                    &format!("active_users_daily {}", day_label(*day)),
                    Outcome::Mismatch,
                    format!("stored flags {s:?}; cutover join {:?}", cutover.get(day)),
                    ctx.options.max_examples,
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Relay lists
// ---------------------------------------------------------------------------

/// One stored winner.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct StoredList {
    pubkey: String,
    id: String,
    created_at: u32,
    relays: Vec<Vec<String>>,
}

/// One source version.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct ListVersion {
    pubkey: String,
    id: String,
    created_at: u32,
    relays: Vec<Vec<String>>,
    indexed_at: u32,
}

/// Distribution row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
struct RelayCount {
    relay_url: String,
    user_count: u64,
    read_count: u64,
    write_count: u64,
}

/// Independent winner rule: highest `created_at`, then lowest id.
fn latest_lists(versions: &[ListVersion]) -> BTreeMap<&str, &ListVersion> {
    let mut winners: BTreeMap<&str, &ListVersion> = BTreeMap::new();
    for v in versions {
        match winners.get(v.pubkey.as_str()) {
            Some(w)
                if (w.created_at, std::cmp::Reverse(&w.id))
                    >= (v.created_at, std::cmp::Reverse(&v.id)) => {}
            _ => {
                winners.insert(&v.pubkey, v);
            }
        }
    }
    winners
}

pub(super) async fn relay_lists(ctx: &Ctx<'_>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let mut report = TargetReport::new(Target::RelayLists);

    let mut pubkeys: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.{RELAY_LISTS_TABLE} \
             WHERE cityHash64(pubkey, toUInt64(?)) % 10000 < ? \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(o.seed)
        .bind(o.sample_permyriad)
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling relay_list_latest pubkeys")?;
    // Two hex digits: 1/256 of the table, enough to find relay lists.
    let (slice_start, _) = seeded_id_range(o.seed);
    let prefix = &slice_start[..2];
    let next = u8::from_str_radix(prefix, 16).expect("hex prefix");
    let slice = id_range_predicate(
        prefix,
        next.checked_add(1).map(|n| format!("{n:02x}")).as_deref(),
    );
    let source: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.events_local WHERE {slice} AND kind = 10002 \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling kind 10002 authors")?;
    let rollup_empty = pubkeys.is_empty();
    pubkeys.extend(source);
    let mut seen = BTreeSet::new();
    pubkeys.retain(|p| seen.insert(p.clone()));
    report.sampled = pubkeys.len();
    if pubkeys.is_empty() && rollup_empty {
        let any: u64 = ctx
            .client
            .query(&format!(
                "SELECT count() FROM (SELECT 1 FROM {db}.events_local WHERE kind = 10002 LIMIT 1)"
            ))
            .fetch_one()
            .await
            .context("checking for kind 10002 rows")?;
        report.source_has_rows = any > 0;
    }

    for chunk in pubkeys.chunks(o.batch.max(1)) {
        let list = placeholders(chunk.len());
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, tupleElement(l, 1) AS id, toUInt32(tupleElement(l, 2)) AS created_at, \
             tupleElement(l, 3) AS relays \
             FROM (SELECT pubkey, argMinMerge(latest) AS l FROM {db}.{RELAY_LISTS_TABLE} \
             WHERE pubkey IN ({list}) GROUP BY pubkey)"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let stored: Vec<StoredList> = q.fetch_all().await.context("reading relay_list_latest")?;
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, id, toUInt32(created_at) AS created_at, {RELAY_TAGS_EXPR} AS relays, \
             toUInt32(indexed_at) AS indexed_at \
             FROM {db}.events_local WHERE kind = 10002 AND pubkey IN ({list})"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let versions: Vec<ListVersion> = q.fetch_all().await.context("reading relay lists")?;

        let fresh: BTreeSet<&str> = versions
            .iter()
            .filter(|v| v.indexed_at >= ctx.cutoff)
            .map(|v| v.pubkey.as_str())
            .collect();
        report.skipped_fresh += fresh.len();
        report.compared += chunk.len() - fresh.len();
        let expected = latest_lists(&versions);
        let actual: BTreeMap<&str, &StoredList> =
            stored.iter().map(|s| (s.pubkey.as_str(), s)).collect();
        let keys: BTreeSet<&&str> = expected.keys().chain(actual.keys()).collect();
        for key in keys {
            if fresh.contains(*key) {
                continue;
            }
            let (outcome, detail) = match (expected.get(*key), actual.get(*key)) {
                (Some(e), Some(a)) => {
                    let same = e.id == a.id && e.created_at == a.created_at && e.relays == a.relays;
                    (
                        if same {
                            Outcome::Match
                        } else {
                            Outcome::Mismatch
                        },
                        format!(
                            "stored {} at {} ({} relays); source {} at {} ({} relays)",
                            a.id,
                            a.created_at,
                            a.relays.len(),
                            e.id,
                            e.created_at,
                            e.relays.len()
                        ),
                    )
                }
                (Some(e), None) => (
                    Outcome::Missing,
                    format!("no stored list; latest source version {}", e.id),
                ),
                (None, Some(a)) => (
                    Outcome::Excess,
                    format!("stored list {} has no source version", a.id),
                ),
                (None, None) => unreachable!("key came from one of the maps"),
            };
            report.record(key, outcome, detail, o.max_examples);
        }
    }

    distribution_comparison(ctx, &mut report).await?;
    Ok(report)
}

async fn distribution_comparison(ctx: &Ctx<'_>, report: &mut TargetReport) -> Result<()> {
    let db = ctx.db;
    let recomputed: Vec<RelayCount> = ctx
        .client
        .query(&format!(
            "SELECT relay_url, user_count, read_count, write_count FROM ({})",
            relay_distribution_select_sql(RelaySource::LatestLists, db)
        ))
        .fetch_all()
        .await
        .context("recomputing relay_distribution from relay_list_latest")?;
    if ops::table_created_at(ctx.client, db, "relay_distribution")
        .await?
        .is_none()
    {
        report.notes.push(format!(
            "relay_distribution does not exist; recomputed {} relays",
            recomputed.len()
        ));
        return Ok(());
    }
    let current: Vec<RelayCount> = ctx
        .client
        .query(&format!(
            "SELECT relay_url, user_count, read_count, write_count \
             FROM {db}.relay_distribution FINAL"
        ))
        .fetch_all()
        .await
        .context("reading relay_distribution")?;
    let source: Option<(String, String)> = ctx
        .client
        .query(
            "SELECT if(position(t.create_table_query, 'relay_list_latest') > 0, \
             'relay_list_latest (020)', 'events_local rescan (014)'), \
             ifNull(toString(r.last_success_time), 'never') \
             FROM system.tables AS t LEFT JOIN system.view_refreshes AS r \
             ON r.database = t.database AND r.view = t.name \
             WHERE t.database = ? AND t.name = ?",
        )
        .bind(db)
        .bind(RELAY_DISTRIBUTION_VIEW)
        .fetch_optional()
        .await
        .context("reading relay_distribution_mv")?;
    let now: BTreeMap<&str, &RelayCount> =
        current.iter().map(|r| (r.relay_url.as_str(), r)).collect();
    let new: BTreeMap<&str, &RelayCount> = recomputed
        .iter()
        .map(|r| (r.relay_url.as_str(), r))
        .collect();
    let only_now = now.keys().filter(|k| !new.contains_key(*k)).count();
    let only_new = new.keys().filter(|k| !now.contains_key(*k)).count();
    let mut changed = 0;
    let mut largest: Option<(i64, &str)> = None;
    for (url, n) in &new {
        if let Some(c) = now.get(url)
            && c != n
        {
            changed += 1;
            let delta = n.user_count as i64 - c.user_count as i64;
            if largest.is_none_or(|(d, _)| delta.abs() > d.abs()) {
                largest = Some((delta, url));
            }
        }
    }
    let (written_by, refreshed) =
        source.unwrap_or_else(|| ("no view".to_string(), "never".to_string()));
    report.notes.push(format!(
        "relay_distribution now: {} relays, {} user listings, written by {written_by} (last \
         success {refreshed}); recomputed from {RELAY_LISTS_TABLE}: {} relays, {} user listings; \
         only now {only_now}, only recomputed {only_new}, different counts {changed}{}",
        current.len(),
        current.iter().map(|r| r.user_count).sum::<u64>(),
        recomputed.len(),
        recomputed.iter().map(|r| r.user_count).sum::<u64>(),
        largest.map_or_else(String::new, |(d, url)| format!(
            " (largest user_count change {d:+} at {url})"
        ))
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

/// Per-kind facts from one scan of `events_local`, run after the rollup was
/// read.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct KindScan {
    kind: u16,
    /// Rows indexed before the view boundary.
    before: u64,
    /// Rows indexed before the cutoff: committed, with their view rows,
    /// before the rollup was read.
    settled: u64,
    /// Every row at the scan.
    total: u64,
    /// Earliest / latest `created_at` of the settled rows (0 when none).
    settled_first: u32,
    settled_last: u32,
    /// Earliest / latest `created_at` of every row.
    first: u32,
    last: u32,
}

/// Per-kind rollup totals.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct KindRollup {
    kind: u16,
    backfill: u64,
    view: u64,
    /// Approximate distinct pubkeys, both origins.
    pubkeys: u64,
    /// Approximate distinct pubkeys of the backfill rows.
    backfill_pubkeys: u64,
    first: u32,
    last: u32,
}

/// Exact distinct pubkeys of one kind (on 128-bit pubkey hashes), split like
/// [`KindScan`].
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct ExactPubkeys {
    kind: u16,
    total: u64,
    settled: u64,
    before: u64,
}

/// `log_comment` of the exact distinct-pubkeys scan.
pub const UNIQUE_PUBKEYS_LOG_COMMENT: &str = "api-rollups:validate:kinds:unique-pubkeys";

async fn kind_scan(ctx: &Ctx<'_>, boundary: u32) -> Result<BTreeMap<u16, KindScan>> {
    let rows: Vec<KindScan> = ctx
        .client
        .query(&format!(
            "SELECT kind, countIf(indexed_at < toDateTime(?)) AS before, \
             countIf(indexed_at < toDateTime(?)) AS settled, count() AS total, \
             toUInt32(minIf(created_at, indexed_at < toDateTime(?))) AS settled_first, \
             toUInt32(maxIf(created_at, indexed_at < toDateTime(?))) AS settled_last, \
             toUInt32(min(created_at)) AS first, toUInt32(max(created_at)) AS last \
             FROM {}.events_local GROUP BY kind",
            ctx.db
        ))
        .bind(boundary)
        .bind(ctx.cutoff)
        .bind(ctx.cutoff)
        .bind(ctx.cutoff)
        .fetch_all()
        .await
        .context("scanning events_local by kind")?;
    Ok(rows.into_iter().map(|r| (r.kind, r)).collect())
}

/// Compare one kind's rollup with the scan that followed it.
fn compare_kind(rollup: &KindRollup, scan: &KindScan, has_boundary: bool) -> (Outcome, String) {
    let detail = format!(
        "rollup backfill {} + view {}, first {} last {}; source before boundary {}, \
         settled {}, total {}, first {}..{} last {}..{}",
        rollup.backfill,
        rollup.view,
        rollup.first,
        rollup.last,
        scan.before,
        scan.settled,
        scan.total,
        scan.first,
        scan.settled_first,
        scan.settled_last,
        scan.last
    );
    let outcome = classify_shares(
        rollup.backfill,
        rollup.view,
        scan.before,
        scan.settled,
        scan.total,
        has_boundary,
    );
    let times = scan.first <= rollup.first
        && rollup.first <= scan.settled_first
        && scan.settled_last <= rollup.last
        && rollup.last <= scan.last;
    if outcome == Outcome::Match && !times {
        (Outcome::Mismatch, detail)
    } else {
        (outcome, detail)
    }
}

pub(super) async fn kinds(ctx: &Ctx<'_>, plan: Option<&Plan>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let mut report = TargetReport::new(Target::Kinds);
    let boundary = plan.and_then(|p| p.source_bounds().indexed_before);
    match boundary {
        Some(c) => report.notes.push(format!(
            "backfill share compared with rows indexed before the boundary {c}"
        )),
        None => report
            .notes
            .push("no kinds plan found: only totals compared (no backfill/view split)".to_string()),
    }

    // Rollup, then one scan: rows indexed before the cutoff are certainly in
    // the rollup, rows the scan sees are a superset of it.
    let rollup: Vec<KindRollup> = ctx
        .client
        .query(&format!(
            "SELECT t.kind AS kind, sumIf(t.event_count, t.origin = 'backfill') AS backfill, \
             sumIf(t.event_count, t.origin = 'view') AS view, \
             uniqCombined64Merge(t.pubkeys) AS pubkeys, \
             uniqCombined64MergeIf(t.pubkeys, t.origin = 'backfill') AS backfill_pubkeys, \
             toUInt32(min(t.first_created_at)) AS first, \
             toUInt32(max(t.last_created_at)) AS last \
             FROM {db}.{KINDS_TABLE} AS t GROUP BY t.kind"
        ))
        .fetch_all()
        .await
        .context("reading kind_totals")?;
    let scan = kind_scan(ctx, boundary.unwrap_or(0)).await?;
    report.source_has_rows = !scan.is_empty();
    let rollup: BTreeMap<u16, KindRollup> = rollup.into_iter().map(|r| (r.kind, r)).collect();

    let kinds: BTreeSet<u16> = scan.keys().chain(rollup.keys()).copied().collect();
    report.sampled = kinds.len();
    for kind in kinds {
        let label = format!("kind {kind}");
        let (outcome, detail) = match (rollup.get(&kind), scan.get(&kind)) {
            // Only rows indexed after the cutoff: they may or may not have
            // been in the rollup when it was read.
            (_, Some(s)) if s.settled == 0 => {
                report.skipped_fresh += 1;
                continue;
            }
            (Some(r), Some(s)) => compare_kind(r, s, boundary.is_some()),
            (None, Some(s)) => (
                Outcome::Missing,
                format!("no kind_totals row; source has {} rows", s.total),
            ),
            (Some(r), None) => (
                Outcome::Excess,
                format!(
                    "kind_totals counts {} events the source does not have",
                    r.backfill + r.view
                ),
            ),
            (None, None) => continue,
        };
        report.compared += 1;
        report.record(&label, outcome, detail, o.max_examples);
    }

    unique_pubkeys(ctx, &rollup, boundary, &mut report).await?;
    Ok(report)
}

/// A kind's size, for choosing the exact distinct-pubkeys sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct KindSize {
    pub(super) kind: u16,
    pub(super) events: u64,
    pub(super) pubkeys: u64,
}

/// Kinds chosen for an exact comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct KindSample {
    /// Kinds to compare.
    pub(super) kinds: Vec<u16>,
    /// Largest kinds left out for exceeding the size cap.
    pub(super) too_large: Vec<u16>,
}

/// `n` kinds: the larger half the largest by events, the rest a seeded
/// random choice of the others, both within `max_pubkeys` approximate
/// pubkeys; plus `extra` regardless of size.
pub(super) fn pubkey_sample(
    sizes: &[KindSize],
    n: usize,
    max_pubkeys: u64,
    seed: u64,
    extra: &[u16],
) -> KindSample {
    let mut by_events: Vec<&KindSize> = sizes.iter().collect();
    by_events.sort_by(|a, b| b.events.cmp(&a.events).then(a.kind.cmp(&b.kind)));
    let mut sample = KindSample::default();
    for k in &by_events {
        if sample.kinds.len() >= n.div_ceil(2) {
            break;
        }
        if k.pubkeys > max_pubkeys {
            sample.too_large.push(k.kind);
        } else {
            sample.kinds.push(k.kind);
        }
    }
    let mut rest: Vec<&KindSize> = by_events
        .into_iter()
        .filter(|k| k.pubkeys <= max_pubkeys && !sample.kinds.contains(&k.kind))
        .collect();
    rest.sort_by_key(|k| (seeded_order(seed, k.kind), k.kind));
    let wanted = n.saturating_sub(sample.kinds.len());
    sample
        .kinds
        .extend(rest.into_iter().take(wanted).map(|k| k.kind));
    for kind in extra {
        if !sample.kinds.contains(kind) {
            sample.kinds.push(*kind);
        }
    }
    sample
}

/// A deterministic pseudo-random order of kinds for a seed (splitmix64).
pub(super) fn seeded_order(seed: u64, kind: u16) -> u64 {
    let mut x = (seed ^ u64::from(kind)).wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The largest relative sketch error seen: (error / exact, key, estimate,
/// exact lower bound, exact upper bound).
pub(super) type Worst = Option<(f64, String, u64, u64, u64)>;

/// Remember the comparison if its relative error is the largest so far.
pub(super) fn track_worst(worst: &mut Worst, key: String, approx: u64, lo: u64, hi: u64) {
    let rel = sketch_error(approx, lo, hi) as f64 / hi.max(1) as f64;
    if worst.as_ref().is_none_or(|(w, ..)| rel > *w) {
        *worst = Some((rel, key, approx, lo, hi));
    }
}

/// Text for the largest error.
pub(super) fn describe_worst(worst: &Worst) -> String {
    worst.as_ref().map_or_else(
        || "none".to_string(),
        |(rel, key, approx, lo, hi)| {
            let exact = if lo == hi {
                lo.to_string()
            } else {
                format!("{lo}..{hi}")
            };
            format!(
                "{:.3}% at {key} (approximate {approx}, exact {exact})",
                rel * 100.0
            )
        },
    )
}

/// Compare `kind_totals`' approximate distinct pubkeys with exact counts for
/// a sample of kinds. One bounded scan: `PREWHERE kind IN (sample)` reads the
/// `kind` column of the whole table, then `pubkey` and `indexed_at` of the
/// granules holding a sampled kind (nearly every granule once a large kind is
/// sampled), keeping one 128-bit hash and the first `indexed_at` per (kind,
/// pubkey). The rollup was read before the scan, so its sketch lies between
/// the pubkeys first indexed before the cutoff and all pubkeys at the scan;
/// the backfill rows' sketch is compared with the pubkeys indexed before the
/// boundary exactly.
async fn unique_pubkeys(
    ctx: &Ctx<'_>,
    rollup: &BTreeMap<u16, KindRollup>,
    boundary: Option<u32>,
    report: &mut TargetReport,
) -> Result<()> {
    let (db, o) = (ctx.db, ctx.options);
    let sizes: Vec<KindSize> = rollup
        .values()
        .map(|r| KindSize {
            kind: r.kind,
            events: r.backfill + r.view,
            pubkeys: r.pubkeys,
        })
        .collect();
    let sample = pubkey_sample(
        &sizes,
        o.pubkeys_kinds,
        o.pubkeys_max_exact,
        o.seed,
        &o.pubkeys_extra_kinds,
    );
    report.skipped_heavy += sample.too_large.len();
    if sample.kinds.is_empty() {
        report
            .notes
            .push("unique_pubkeys not compared (no kinds sampled)".to_string());
        return Ok(());
    }
    let list = sample
        .kinds
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let exact: Vec<ExactPubkeys> = ctx
        .client
        .query(&format!(
            "SELECT kind, count() AS total, countIf(first_indexed < toDateTime(?)) AS settled, \
             countIf(first_indexed < toDateTime(?)) AS before \
             FROM ( \
             SELECT kind, sipHash128(pubkey) AS key, min(indexed_at) AS first_indexed \
             FROM {db}.events_local PREWHERE kind IN ({list}) \
             GROUP BY kind, key \
             ) \
             GROUP BY kind"
        ))
        .bind(ctx.cutoff)
        .bind(boundary.unwrap_or(0))
        .with_option("log_comment", UNIQUE_PUBKEYS_LOG_COMMENT)
        .fetch_all()
        .await
        .context("counting exact distinct pubkeys of the sampled kinds")?;
    let exact: BTreeMap<u16, &ExactPubkeys> = exact.iter().map(|e| (e.kind, e)).collect();

    let (mut worst, mut worst_backfill): (Worst, Worst) = (None, None);
    for kind in &sample.kinds {
        let Some(r) = rollup.get(kind) else {
            // A requested kind without rollup rows: the count check above
            // already reports it if the source has rows.
            continue;
        };
        let (total, settled, before) = exact
            .get(kind)
            .map_or((0, 0, 0), |e| (e.total, e.settled, e.before));
        let error = sketch_error(r.pubkeys, settled, total);
        let allowed = sketch_tolerance(total, o.pubkeys_sigmas);
        track_worst(
            &mut worst,
            format!("kind {kind}"),
            r.pubkeys,
            settled,
            total,
        );
        report.record(
            &format!("kind {kind} unique_pubkeys"),
            if error > allowed {
                Outcome::Mismatch
            } else {
                Outcome::Match
            },
            format!(
                "approximate {} vs exact {settled} (first indexed before the cutoff) .. {total} \
                 (after the rollup read): error {error}, allowed {allowed}",
                r.pubkeys
            ),
            o.max_examples,
        );
        if boundary.is_some() {
            let error = r.backfill_pubkeys.abs_diff(before);
            let allowed = sketch_tolerance(before, o.pubkeys_sigmas);
            track_worst(
                &mut worst_backfill,
                format!("kind {kind}"),
                r.backfill_pubkeys,
                before,
                before,
            );
            report.record(
                &format!("kind {kind} unique_pubkeys (backfill share)"),
                if error > allowed {
                    Outcome::Mismatch
                } else {
                    Outcome::Match
                },
                format!(
                    "approximate {} vs exact {before} (indexed before the boundary): error \
                     {error}, allowed {allowed}",
                    r.backfill_pubkeys
                ),
                o.max_examples,
            );
        }
    }
    report.notes.push(format!(
        "unique_pubkeys of {} kinds ({list}) compared with exact counts (first indexed before \
         the cutoff .. after the rollup read), allowed max({} x {:.4}%, {SKETCH_SLACK}) above \
         {SKETCH_EXACT_UP_TO} values, else {SKETCH_SLACK}: largest error {}{}{}",
        sample.kinds.len(),
        o.pubkeys_sigmas,
        SKETCH_RELATIVE_ERROR * 100.0,
        describe_worst(&worst),
        if boundary.is_some() {
            format!(
                "; backfill share largest error {}",
                describe_worst(&worst_backfill)
            )
        } else {
            String::new()
        },
        if sample.too_large.is_empty() {
            String::new()
        } else {
            format!(
                "; kinds above --pubkeys-max-exact {} left out: {:?}",
                o.pubkeys_max_exact, sample.too_large
            )
        }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(
        before: u64,
        settled: u64,
        total: u64,
        first: (u32, u32),
        last: (u32, u32),
    ) -> KindScan {
        KindScan {
            kind: 1,
            before,
            settled,
            total,
            first: first.0,
            settled_first: first.1,
            settled_last: last.0,
            last: last.1,
        }
    }

    fn rollup(backfill: u64, view: u64, first: u32, last: u32) -> KindRollup {
        KindRollup {
            kind: 1,
            backfill,
            view,
            pubkeys: 0,
            backfill_pubkeys: 0,
            first,
            last,
        }
    }

    #[test]
    fn kind_shares_are_bounded_by_the_cutoff_and_the_scan() {
        // 100 rows before the boundary, 150 settled, 160 at the scan; first
        // created_at 10 among the settled rows, 5 overall; last 90 / 95.
        let s = scan(100, 150, 160, (5, 10), (90, 95));
        let check = |r: KindRollup| compare_kind(&r, &s, true).0;
        assert_eq!(check(rollup(100, 50, 10, 90)), Outcome::Match);
        assert_eq!(check(rollup(100, 60, 5, 95)), Outcome::Match);
        assert_eq!(check(rollup(99, 50, 10, 90)), Outcome::Missing);
        assert_eq!(check(rollup(101, 50, 10, 90)), Outcome::Excess);
        assert_eq!(check(rollup(100, 49, 10, 90)), Outcome::Missing);
        assert_eq!(check(rollup(100, 61, 10, 90)), Outcome::Excess);
        assert_eq!(check(rollup(100, 50, 11, 90)), Outcome::Mismatch);
        assert_eq!(check(rollup(100, 50, 10, 96)), Outcome::Mismatch);
        // Without a plan only the total is bounded.
        assert_eq!(
            compare_kind(&rollup(0, 155, 10, 90), &s, false).0,
            Outcome::Match
        );
        assert_eq!(
            compare_kind(&rollup(0, 149, 10, 90), &s, false).0,
            Outcome::Missing
        );
    }

    fn size(kind: u16, events: u64, pubkeys: u64) -> KindSize {
        KindSize {
            kind,
            events,
            pubkeys,
        }
    }

    #[test]
    fn pubkey_sample_takes_large_and_random_kinds_within_the_cap() {
        let sizes = [
            size(1059, 900, 900),
            size(1, 800, 50),
            size(7, 700, 40),
            size(3, 600, 30),
            size(20001, 10, 5),
            size(30023, 9, 3),
            size(4, 8, 2),
            size(5, 7, 1),
        ];
        let sample = pubkey_sample(&sizes, 4, 100, 19, &[]);
        assert_eq!(sample.too_large, vec![1059]);
        assert_eq!(&sample.kinds[..2], &[1, 7]);
        assert_eq!(sample.kinds.len(), 4);
        assert!(!sample.kinds.contains(&1059));
        // Deterministic for a seed; extras are added even above the cap.
        assert_eq!(pubkey_sample(&sizes, 4, 100, 19, &[]), sample);
        let with_extra = pubkey_sample(&sizes, 4, 100, 19, &[1059, 1]);
        assert_eq!(with_extra.kinds.len(), 5);
        assert_eq!(with_extra.kinds.last(), Some(&1059));
        assert!(pubkey_sample(&sizes, 0, 100, 19, &[]).kinds.is_empty());
        assert_eq!(pubkey_sample(&sizes, 100, 100, 19, &[]).kinds.len(), 7);
        let mut worst = None;
        track_worst(&mut worst, "kind 1".into(), 105, 90, 100);
        track_worst(&mut worst, "kind 7".into(), 95, 90, 100);
        assert!(describe_worst(&worst).starts_with("5.000% at kind 1"));
    }

    fn version(pubkey: &str, id: &str, created_at: u32) -> ListVersion {
        ListVersion {
            pubkey: pubkey.into(),
            id: id.into(),
            created_at,
            relays: vec![],
            indexed_at: 0,
        }
    }

    #[test]
    fn latest_list_prefers_newest_then_lowest_id() {
        let versions = [
            version("p", "b2", 10),
            version("p", "b1", 10),
            version("p", "a0", 9),
            version("q", "z", 1),
        ];
        let winners = latest_lists(&versions);
        assert_eq!(winners["p"].id, "b1");
        assert_eq!(winners["q"].id, "z");
        assert_eq!(day_label(20_000), "2024-10-04");
    }
}
