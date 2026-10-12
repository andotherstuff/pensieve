//! Validate the rollups against a direct (slow) oracle on `events_local`.
//!
//! The migration 020 targets (user flags, relay lists, kinds) are validated in
//! `validate_derived`; see its module docs.
//!
//! **Publishers.** Pubkeys are sampled three ways: a hashed random subset of
//! the rollup, the pubkeys of one random id-prefix slice of `events_local`
//! (so pubkeys missing from the rollup are found), and, when a plan exists,
//! pubkeys with events indexed within `--boundary-window-secs` of the view
//! boundary (where a double count or gap would show). For each pubkey every
//! (grain, kind, period) row from the last [`PUBLISHER_ROLLUP_DAYS`] days is
//! recomputed from `events_local` and compared:
//!
//! - `event_count` must equal the number of distinct event ids. A larger
//!   count still within the raw row count is `duplicate_inflation` (unmerged
//!   duplicate rows that were inserted twice, which sums cannot undo); any
//!   other difference is `missing` or `excess`.
//! - kinds and first/last `created_at` must match exactly (`mismatch`).
//!
//! **Longform.** Every kind 30023 coordinate is cross-checked against migration
//! 017's `addressable_latest` (same winner rule) when that table exists, and
//! sampled authors are recomputed from all their source versions: the latest
//! version's id, `created_at` and content length must match.
//!
//! Consistency: `cutoff = now - --cutoff-margin-secs`. No insert that started
//! before the cutoff may still be running; the rollup is read before the
//! oracle, and keys with any source row indexed at or after the cutoff are
//! skipped, so concurrent ingest cannot show up as a difference. Pubkeys with
//! more than `--max-key-events` events in the window are skipped too: the
//! oracle reads them through the `idx_pubkey` bloom filter, and very active
//! pubkeys touch most of the table.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::plan::{Plan, utc_day};
use super::{
    ACTIVITY_AT_EXPR, ALL_KINDS, EXCLUDED_PUBLISHER_KINDS, LONGFORM_TABLE, PUBLISHER_ROLLUP_DAYS,
    PUBLISHER_TABLE, Target, ops, validate_derived,
};

/// Validation options.
#[derive(Debug, Clone)]
pub struct ValidateOptions {
    /// Targets to validate.
    pub targets: Vec<Target>,
    /// Keys per sampling source (rollup side and source side).
    pub sample: usize,
    /// Publishers: pubkeys sampled around the view boundary (needs a plan).
    pub boundary_sample: usize,
    /// Publishers: half-width of the boundary window, in seconds.
    pub boundary_window_secs: u32,
    /// Sampling seed.
    pub seed: u64,
    /// Rollup-side keys come from `cityHash64(key, seed) % 10000 < permyriad`.
    pub sample_permyriad: u32,
    /// Compare only data indexed at least this long ago.
    pub cutoff_margin: Duration,
    /// Keys per oracle query.
    pub batch: usize,
    /// Publishers: skip pubkeys with more events than this in the window.
    pub max_key_events: u64,
    /// Differences to keep as examples per target.
    pub max_examples: usize,
    /// User flags: days of `daily_user_stats` whose stored flags must all
    /// have a matching `pubkey_flags` entry.
    pub flag_days: u32,
    /// User flags: days of `daily_user_stats` compared per sampled pubkey.
    pub user_days: u32,
    /// Kinds: largest relative difference (percent) between the approximate
    /// and the exact (`kinds_stats_mv`) distinct pubkeys of a kind.
    pub pubkeys_tolerance_pct: f64,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        Self {
            targets: Target::ALL.to_vec(),
            sample: 300,
            boundary_sample: 200,
            boundary_window_secs: 120,
            seed: 19,
            sample_permyriad: 100,
            cutoff_margin: Duration::from_secs(300),
            batch: 100,
            max_key_events: 5_000,
            max_examples: 20,
            flag_days: 7,
            user_days: 3,
            pubkeys_tolerance_pct: 2.0,
        }
    }
}

/// Classification of one compared row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Rollup equals the oracle.
    Match,
    /// Count above the distinct events but within the raw (duplicate) rows.
    DuplicateInflation,
    /// Rollup row missing or counting fewer events than the source.
    Missing,
    /// Rollup row the source does not have, or more events than its rows.
    Excess,
    /// Same key, different kinds, timestamps, winner or content length.
    Mismatch,
}

impl Outcome {
    /// Whether this outcome fails validation.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Missing | Self::Excess | Self::Mismatch)
    }
}

/// A recorded difference.
#[derive(Debug, Clone, Serialize)]
pub struct Difference {
    /// Row key.
    pub key: String,
    /// Classification.
    pub outcome: Outcome,
    /// Human-readable detail.
    pub detail: String,
}

/// Cross-check of `longform_latest` against `addressable_latest`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
pub struct CrossCheck {
    /// Coordinates with the same winning id in both tables.
    pub matched: u64,
    /// Coordinates whose winning id differs.
    pub different: u64,
    /// Coordinates only in `longform_latest`.
    pub only_longform: u64,
    /// Coordinates only in `addressable_latest`.
    pub only_addressable: u64,
    /// Coordinates skipped because they changed after the cutoff.
    pub skipped_fresh: u64,
}

impl CrossCheck {
    /// Any disagreement.
    pub fn disagrees(&self) -> bool {
        self.different + self.only_longform + self.only_addressable > 0
    }
}

/// Validation results for one target.
#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    /// Target.
    pub target: Target,
    /// Keys sampled (pubkeys; kinds for the kinds target).
    pub sampled: usize,
    /// Keys compared with the oracle.
    pub compared: usize,
    /// Keys skipped: a source row was indexed after the cutoff.
    pub skipped_fresh: usize,
    /// Keys skipped: too many events for a bounded oracle read.
    pub skipped_heavy: usize,
    /// Whether the source has rows for this target at all.
    pub source_has_rows: bool,
    /// Row outcomes.
    pub rows: BTreeMap<Outcome, u64>,
    /// Longform: cross-check with `addressable_latest`, when it exists.
    pub cross_check: Option<CrossCheck>,
    /// Example differences.
    pub examples: Vec<Difference>,
    /// Notes for the operator.
    pub notes: Vec<String>,
}

impl TargetReport {
    pub(super) fn new(target: Target) -> Self {
        Self {
            target,
            sampled: 0,
            compared: 0,
            skipped_fresh: 0,
            skipped_heavy: 0,
            source_has_rows: true,
            rows: BTreeMap::new(),
            cross_check: None,
            examples: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub(super) fn record(
        &mut self,
        key: &str,
        outcome: Outcome,
        detail: String,
        max_examples: usize,
    ) {
        *self.rows.entry(outcome).or_default() += 1;
        if outcome != Outcome::Match && self.examples.len() < max_examples {
            self.examples.push(Difference {
                key: key.to_string(),
                outcome,
                detail,
            });
        }
    }

    /// Rows that fail validation.
    pub fn failures(&self) -> u64 {
        self.rows
            .iter()
            .filter(|(o, _)| o.is_failure())
            .map(|(_, n)| n)
            .sum()
    }

    /// Nothing compared although the source has rows: no evidence either way.
    pub fn inconclusive(&self) -> bool {
        self.compared == 0 && self.source_has_rows
    }
}

/// Full validation report.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    /// Cutoff (Unix seconds): only data indexed before this is compared.
    pub cutoff: u32,
    /// Sampling seed.
    pub seed: u64,
    /// Per-target results.
    pub targets: Vec<TargetReport>,
}

impl ValidationReport {
    /// Failed rows across targets.
    pub fn failures(&self) -> u64 {
        self.targets.iter().map(TargetReport::failures).sum()
    }

    /// No failures and every target with source rows compared something.
    pub fn passed(&self) -> bool {
        self.failures() == 0 && !self.targets.iter().any(TargetReport::inconclusive)
    }

    /// Text summary.
    pub fn render(&self) -> String {
        let mut out = format!("cutoff {} (seed {})\n", self.cutoff, self.seed);
        for t in &self.targets {
            let _ = writeln!(
                out,
                "{}: sampled {}, compared {}, skipped fresh {}, skipped heavy {}\n  rows: {:?}",
                t.target, t.sampled, t.compared, t.skipped_fresh, t.skipped_heavy, t.rows
            );
            if let Some(c) = &t.cross_check {
                let _ = writeln!(out, "  addressable_latest cross-check: {c:?}");
            }
            for note in &t.notes {
                let _ = writeln!(out, "  note: {note}");
            }
            for d in &t.examples {
                let _ = writeln!(out, "    {:?} {}: {}", d.outcome, d.key, d.detail);
            }
            if t.inconclusive() {
                let _ = writeln!(
                    out,
                    "  INCONCLUSIVE: the source has rows but nothing was compared; widen \
                     --sample/--sample-permyriad/--max-key-events or retry later"
                );
            }
        }
        let _ = writeln!(out, "failed rows: {}", self.failures());
        out
    }
}

/// What validation knows from the checkpoint directory.
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidateInputs<'a> {
    /// Publisher plan: the view boundary for boundary sampling.
    pub publisher_plan: Option<&'a Plan>,
    /// Kinds plan: the view boundary splitting backfill and view shares.
    pub kinds_plan: Option<&'a Plan>,
    /// Migration 020 cutover time (`finished_at`), when it ran.
    pub cutover_at: Option<u32>,
}

/// Run validation. `client` should carry resource limits. `publisher_plan`
/// supplies the publisher view boundary for boundary sampling, when planned.
pub async fn run(
    client: &Client,
    database: &str,
    options: &ValidateOptions,
    publisher_plan: Option<&Plan>,
) -> Result<ValidationReport> {
    let inputs = ValidateInputs {
        publisher_plan,
        ..ValidateInputs::default()
    };
    run_with(client, database, options, &inputs).await
}

/// Run validation with everything known from the checkpoint directory.
pub async fn run_with(
    client: &Client,
    database: &str,
    options: &ValidateOptions,
    inputs: &ValidateInputs<'_>,
) -> Result<ValidationReport> {
    if options.sample == 0 && options.boundary_sample == 0 {
        bail!("validation needs --sample or --boundary-sample above 0");
    }
    let now = ops::server_now(client).await?;
    let cutoff = now.saturating_sub(options.cutoff_margin.as_secs() as u32);
    let running = ops::inserts_started_before(client, database, cutoff).await?;
    if running > 0 {
        bail!(
            "{running} INSERT(s) into events_local started before the cutoff are still running; retry"
        );
    }
    let ctx = Ctx {
        db: database,
        client,
        options,
        cutoff,
    };
    let mut targets = Vec::new();
    for target in &options.targets {
        targets.push(match target {
            Target::Publishers => validate_publishers(&ctx, inputs.publisher_plan).await?,
            Target::Longform => validate_longform(&ctx).await?,
            Target::UserFlags => validate_derived::user_flags(&ctx, inputs.cutover_at).await?,
            Target::RelayLists => validate_derived::relay_lists(&ctx).await?,
            Target::Kinds => validate_derived::kinds(&ctx, inputs.kinds_plan).await?,
        });
    }
    Ok(ValidationReport {
        cutoff,
        seed: options.seed,
        targets,
    })
}

pub(super) struct Ctx<'a> {
    pub(super) db: &'a str,
    pub(super) client: &'a Client,
    pub(super) options: &'a ValidateOptions,
    pub(super) cutoff: u32,
}

pub(super) fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn excluded_kinds() -> String {
    EXCLUDED_PUBLISHER_KINDS
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A random 3-hex-digit id prefix range derived from the seed.
pub(super) fn seeded_id_range(seed: u64) -> (String, Option<String>) {
    let bucket = (seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 52) as usize; // 0..4096
    let start = format!("{bucket:03x}");
    let end = (bucket + 1 < 4096).then(|| format!("{:03x}", bucket + 1));
    (start, end)
}

pub(super) fn id_range_predicate(start: &str, end: Option<&str>) -> String {
    match end {
        Some(end) => format!("id >= '{start}' AND id < '{end}'"),
        None => format!("id >= '{start}'"),
    }
}

// ---------------------------------------------------------------------------
// Publishers
// ---------------------------------------------------------------------------

/// One (pubkey, grain, kind, period) row recomputed from `events_local`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
pub struct OracleGroup {
    /// Pubkey.
    pub pubkey: String,
    /// 1 = day, 2 = month.
    pub grain: u8,
    /// Kind, or -1 for all kinds.
    pub kind: i32,
    /// Period as days since 1970-01-01.
    pub period: u16,
    /// Raw source rows (unmerged duplicates included).
    pub rows: u64,
    /// Distinct event ids.
    pub events: u64,
    /// Sorted distinct kinds.
    pub kinds: Vec<u16>,
    /// Earliest `created_at`.
    pub first: u32,
    /// Latest `created_at`.
    pub last: u32,
    /// Latest `indexed_at`.
    pub newest_indexed: u32,
}

/// One rollup row, collapsed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
pub struct RollupGroup {
    /// Pubkey.
    pub pubkey: String,
    /// 1 = day, 2 = month.
    pub grain: u8,
    /// Kind, or -1 for all kinds.
    pub kind: i32,
    /// Period as days since 1970-01-01.
    pub period: u16,
    /// Summed events.
    pub event_count: u64,
    /// Sorted distinct kinds.
    pub kinds: Vec<u16>,
    /// Earliest `created_at`.
    pub first: u32,
    /// Latest `created_at`.
    pub last: u32,
}

type GroupKey = (String, u8, i32, u16);

fn group_label(key: &GroupKey) -> String {
    let (pubkey, grain, kind, period) = key;
    let day =
        NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch") + chrono::Days::new(u64::from(*period));
    let grain = if *grain == 1 { "day" } else { "month" };
    let kind = if *kind == ALL_KINDS {
        "all".to_string()
    } else {
        kind.to_string()
    };
    format!("{pubkey} {grain} {day} kind={kind}")
}

/// Classify a summed count against the source's distinct events and raw
/// rows (unmerged duplicates included).
pub fn classify_count(counted: u64, events: u64, rows: u64) -> Outcome {
    if counted == events {
        Outcome::Match
    } else if counted < events {
        Outcome::Missing
    } else if counted <= rows {
        Outcome::DuplicateInflation
    } else {
        Outcome::Excess
    }
}

/// Compare the rollup rows of some pubkeys with their oracle rows.
pub fn compare_publisher_groups(
    oracle: &[OracleGroup],
    rollup: &[RollupGroup],
) -> Vec<(String, Outcome, String)> {
    let mut expected: BTreeMap<GroupKey, &OracleGroup> = BTreeMap::new();
    for o in oracle {
        expected.insert((o.pubkey.clone(), o.grain, o.kind, o.period), o);
    }
    let mut actual: BTreeMap<GroupKey, &RollupGroup> = BTreeMap::new();
    for r in rollup {
        actual.insert((r.pubkey.clone(), r.grain, r.kind, r.period), r);
    }
    let keys: BTreeSet<&GroupKey> = expected.keys().chain(actual.keys()).collect();
    let mut out = Vec::new();
    for key in keys {
        let label = group_label(key);
        match (expected.get(key), actual.get(key)) {
            (Some(o), Some(r)) => {
                let outcome = classify_count(r.event_count, o.events, o.rows);
                let fields_match = r.kinds == o.kinds && r.first == o.first && r.last == o.last;
                let outcome = if outcome == Outcome::Match && !fields_match {
                    Outcome::Mismatch
                } else {
                    outcome
                };
                let detail = format!(
                    "rollup count {} kinds {:?} first {} last {}; source {} events ({} rows) \
                     kinds {:?} first {} last {}",
                    r.event_count,
                    r.kinds,
                    r.first,
                    r.last,
                    o.events,
                    o.rows,
                    o.kinds,
                    o.first,
                    o.last
                );
                out.push((label, outcome, detail));
            }
            (Some(o), None) => out.push((
                label,
                Outcome::Missing,
                format!("no rollup row; source has {} events", o.events),
            )),
            (None, Some(r)) => out.push((
                label,
                Outcome::Excess,
                format!(
                    "rollup counts {} events the source does not have",
                    r.event_count
                ),
            )),
            (None, None) => unreachable!("key came from one of the maps"),
        }
    }
    out
}

async fn validate_publishers(ctx: &Ctx<'_>, plan: Option<&Plan>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let mut report = TargetReport::new(Target::Publishers);
    let today = utc_day(ops::server_now(ctx.client).await?);
    let window_start = today - chrono::Days::new(u64::from(PUBLISHER_ROLLUP_DAYS));
    let start = window_start.format("%Y-%m-%d").to_string();
    let excluded = excluded_kinds();
    let activity = ACTIVITY_AT_EXPR;

    // 1. Rollup-side sample.
    let mut pubkeys: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.{PUBLISHER_TABLE} \
             WHERE grain = 'month' AND kind = {ALL_KINDS} \
             AND cityHash64(pubkey, toUInt64(?)) % 10000 < ? \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(o.seed)
        .bind(o.sample_permyriad)
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling rollup pubkeys")?;

    // 2. Source-side sample: pubkeys of one id-prefix slice (a primary-key read).
    let (slice_start, slice_end) = seeded_id_range(o.seed);
    let slice = id_range_predicate(&slice_start, slice_end.as_deref());
    let source: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.events_local \
             WHERE {slice} AND kind NOT IN ({excluded}) \
             AND toDate({activity}, 'UTC') >= toDate('{start}') \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling source pubkeys")?;
    report.source_has_rows = !source.is_empty() || !pubkeys.is_empty();
    pubkeys.extend(source);

    // 3. Boundary sample: pubkeys with events indexed around the view boundary.
    match plan.and_then(|p| p.publisher_bounds.as_ref()) {
        Some(bounds) if o.boundary_sample > 0 => {
            let lo = bounds.indexed_before.saturating_sub(o.boundary_window_secs);
            let hi = bounds.indexed_before.saturating_add(o.boundary_window_secs);
            let boundary: Vec<String> = ctx
                .client
                .query(&format!(
                    "SELECT pubkey FROM {db}.events_local \
                     WHERE indexed_at >= toDateTime(?) AND indexed_at < toDateTime(?) \
                     AND kind NOT IN ({excluded}) \
                     GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
                ))
                .bind(lo)
                .bind(hi)
                .bind(o.seed)
                .bind(o.boundary_sample as u64)
                .fetch_all()
                .await
                .context("sampling pubkeys around the view boundary")?;
            report.notes.push(format!(
                "{} pubkeys sampled with events indexed in [{lo}, {hi}) around the view \
                 boundary {}",
                boundary.len(),
                bounds.indexed_before
            ));
            pubkeys.extend(boundary);
        }
        Some(_) => {}
        None => report
            .notes
            .push("no publisher plan found: boundary sampling skipped".to_string()),
    }
    let mut seen = BTreeSet::new();
    pubkeys.retain(|p| seen.insert(p.clone()));
    report.sampled = pubkeys.len();

    // 4. Skip pubkeys too active for a bounded oracle read.
    let mut totals: HashMap<String, u64> = HashMap::new();
    for chunk in pubkeys.chunks(o.batch.max(1)) {
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, sum(event_count) FROM {db}.{PUBLISHER_TABLE} \
             WHERE grain = 'month' AND kind = {ALL_KINDS} \
             AND period >= toStartOfMonth(toDate('{start}')) \
             AND pubkey IN ({}) GROUP BY pubkey",
            placeholders(chunk.len())
        ));
        for p in chunk {
            q = q.bind(p);
        }
        totals.extend(
            q.fetch_all::<(String, u64)>()
                .await
                .context("reading rollup totals")?,
        );
    }
    let light: Vec<String> = pubkeys
        .into_iter()
        .filter(|p| totals.get(p).copied().unwrap_or(0) <= o.max_key_events)
        .collect();
    report.skipped_heavy = report.sampled - light.len();

    // 5. Rollup first, then the oracle (see the module docs).
    for chunk in light.chunks(o.batch.max(1)) {
        let list = placeholders(chunk.len());
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, toUInt8(grain), kind, toUInt16(period), sum(event_count), \
             arraySort(groupUniqArrayArray(kinds)), toUInt32(min(first_created_at)), \
             toUInt32(max(last_created_at)) \
             FROM {db}.{PUBLISHER_TABLE} \
             WHERE pubkey IN ({list}) AND period >= toDate('{start}') \
             GROUP BY pubkey, grain, kind, period"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let rollup: Vec<RollupGroup> = q.fetch_all().await.context("reading rollup rows")?;

        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, slot.1 AS grain, slot.2 AS kind, \
             toUInt16(if(slot.1 = 1, toDate(activity_at, 'UTC'), \
             toStartOfMonth(activity_at, 'UTC'))) AS period, \
             count() AS rows, uniqExact(id) AS events, \
             arraySort(groupUniqArray(event_kind)) AS kinds, \
             toUInt32(min(created_at)) AS first, toUInt32(max(created_at)) AS last, \
             toUInt32(max(indexed_at)) AS newest_indexed \
             FROM ( \
             SELECT id, pubkey, kind AS event_kind, created_at, indexed_at, \
             {activity} AS activity_at \
             FROM {db}.events_local \
             WHERE pubkey IN ({list}) AND kind NOT IN ({excluded}) \
             AND toDate(activity_at, 'UTC') >= toDate('{start}') \
             ) \
             ARRAY JOIN [(toUInt8(1), toInt32(-1)), (toUInt8(1), toInt32(event_kind)), \
             (toUInt8(2), toInt32(-1)), (toUInt8(2), toInt32(event_kind))] AS slot \
             GROUP BY pubkey, grain, kind, period \
             HAVING period >= toUInt16(toDate('{start}'))"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let oracle: Vec<OracleGroup> = q.fetch_all().await.context("reading oracle rows")?;

        let fresh: BTreeSet<&str> = oracle
            .iter()
            .filter(|g| g.newest_indexed >= ctx.cutoff)
            .map(|g| g.pubkey.as_str())
            .collect();
        report.skipped_fresh += fresh.len();
        report.compared += chunk.len() - fresh.len();
        let oracle: Vec<OracleGroup> = oracle
            .iter()
            .filter(|g| !fresh.contains(g.pubkey.as_str()))
            .cloned()
            .collect();
        let rollup: Vec<RollupGroup> = rollup
            .into_iter()
            .filter(|g| !fresh.contains(g.pubkey.as_str()))
            .collect();
        for (key, outcome, detail) in compare_publisher_groups(&oracle, &rollup) {
            report.record(&key, outcome, detail, o.max_examples);
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Longform
// ---------------------------------------------------------------------------

/// One source version of a kind 30023 article.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
pub struct ArticleVersion {
    /// Author.
    pub pubkey: String,
    /// `d` tag.
    pub d_tag: String,
    /// Event id.
    pub id: String,
    /// `created_at`.
    pub created_at: u32,
    /// Content length in bytes.
    pub content_length: u64,
    /// `indexed_at`.
    pub indexed_at: u32,
}

/// The latest version of one article as stored in `longform_latest`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
pub struct LatestArticle {
    /// Author.
    pub pubkey: String,
    /// `d` tag.
    pub d_tag: String,
    /// Winning event id.
    pub id: String,
    /// Winning `created_at`.
    pub created_at: u32,
    /// Winning content length.
    pub content_length: u64,
}

/// Independent winner rule: highest `created_at`, then lowest id.
pub fn latest_versions(versions: &[ArticleVersion]) -> BTreeMap<(String, String), &ArticleVersion> {
    let mut winners: BTreeMap<(String, String), &ArticleVersion> = BTreeMap::new();
    for v in versions {
        let key = (v.pubkey.clone(), v.d_tag.clone());
        match winners.get(&key) {
            Some(w)
                if (w.created_at, std::cmp::Reverse(&w.id))
                    >= (v.created_at, std::cmp::Reverse(&v.id)) => {}
            _ => {
                winners.insert(key, v);
            }
        }
    }
    winners
}

async fn validate_longform(ctx: &Ctx<'_>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let mut report = TargetReport::new(Target::Longform);

    // 1. Full cross-check with migration 017's addressable_latest.
    let mut adjudicate: Vec<String> = Vec::new();
    if ops::table_created_at(ctx.client, db, "addressable_latest")
        .await?
        .is_some()
    {
        let joined = format!(
            "SELECT pubkey, d_tag, l.id AS lid, a.id AS aid, l.present AS lp, a.present AS ap, \
             greatest(l.li, a.li) AS li \
             FROM ( \
             SELECT pubkey, d_tag, tupleElement(argMinMerge(latest), 1) AS id, \
             toUInt32(max(last_indexed_at)) AS li, toUInt8(1) AS present \
             FROM {db}.{LONGFORM_TABLE} GROUP BY pubkey, d_tag) AS l \
             FULL OUTER JOIN ( \
             SELECT pubkey, d_tag, argMinMerge(latest) AS id, \
             toUInt32(max(last_indexed_at)) AS li, toUInt8(1) AS present \
             FROM {db}.addressable_latest WHERE kind = 30023 GROUP BY pubkey, d_tag) AS a \
             USING (pubkey, d_tag)"
        );
        let cross: CrossCheck = ctx
            .client
            .query(&format!(
                "SELECT \
                 countIf(li < ? AND lp = 1 AND ap = 1 AND lid = aid) AS matched, \
                 countIf(li < ? AND lp = 1 AND ap = 1 AND lid != aid) AS different, \
                 countIf(li < ? AND lp = 1 AND ap = 0) AS only_longform, \
                 countIf(li < ? AND lp = 0 AND ap = 1) AS only_addressable, \
                 countIf(li >= ?) AS skipped_fresh \
                 FROM ({joined}) SETTINGS join_use_nulls = 0"
            ))
            .bind(ctx.cutoff)
            .bind(ctx.cutoff)
            .bind(ctx.cutoff)
            .bind(ctx.cutoff)
            .bind(ctx.cutoff)
            .fetch_one()
            .await
            .context("cross-checking longform_latest with addressable_latest")?;
        if cross.disagrees() {
            // Let the oracle decide who is right for some of the disagreements.
            adjudicate = ctx
                .client
                .query(&format!(
                    "SELECT DISTINCT pubkey FROM ({joined}) \
                     WHERE li < ? AND NOT (lp = 1 AND ap = 1 AND lid = aid) LIMIT ? \
                     SETTINGS join_use_nulls = 0"
                ))
                .bind(ctx.cutoff)
                .bind(o.sample as u64)
                .fetch_all()
                .await
                .context("listing cross-check disagreements")?;
            report.notes.push(format!(
                "{} author(s) with disagreements re-checked against the oracle",
                adjudicate.len()
            ));
        }
        report.cross_check = Some(cross);
    } else {
        report
            .notes
            .push("addressable_latest not found: cross-check skipped".to_string());
    }

    // 2. Sampled authors: random from the rollup, plus one source slice.
    let mut pubkeys: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.{LONGFORM_TABLE} \
             WHERE cityHash64(pubkey, toUInt64(?)) % 10000 < ? \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(o.seed)
        .bind(o.sample_permyriad)
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling longform authors")?;
    let (slice_start, _) = seeded_id_range(o.seed);
    // Two hex digits: 1/256 of the table, enough to find some articles.
    let prefix = &slice_start[..2];
    let next = u8::from_str_radix(prefix, 16).expect("hex prefix");
    let slice = id_range_predicate(
        prefix,
        next.checked_add(1).map(|n| format!("{n:02x}")).as_deref(),
    );
    let source: Vec<String> = ctx
        .client
        .query(&format!(
            "SELECT pubkey FROM {db}.events_local WHERE {slice} AND kind = 30023 \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64(?)) LIMIT ?"
        ))
        .bind(o.seed)
        .bind(o.sample as u64)
        .fetch_all()
        .await
        .context("sampling longform source authors")?;
    let rollup_empty = pubkeys.is_empty();
    pubkeys.extend(source);
    pubkeys.extend(adjudicate);
    let mut seen = BTreeSet::new();
    pubkeys.retain(|p| seen.insert(p.clone()));
    report.sampled = pubkeys.len();
    if pubkeys.is_empty() && rollup_empty {
        let any: u64 = ctx
            .client
            .query(&format!(
                "SELECT count() FROM (SELECT 1 FROM {db}.events_local WHERE kind = 30023 LIMIT 1)"
            ))
            .fetch_one()
            .await
            .context("checking for kind 30023 rows")?;
        report.source_has_rows = any > 0;
    }

    // 3. Rollup first, then every source version of those authors.
    for chunk in pubkeys.chunks(o.batch.max(1)) {
        let list = placeholders(chunk.len());
        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, d_tag, tupleElement(l, 1), toUInt32(tupleElement(l, 2)), \
             tupleElement(l, 3) \
             FROM (SELECT pubkey, d_tag, argMinMerge(latest) AS l FROM {db}.{LONGFORM_TABLE} \
             WHERE pubkey IN ({list}) GROUP BY pubkey, d_tag)"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let rollup: Vec<LatestArticle> = q.fetch_all().await.context("reading longform rows")?;

        let mut q = ctx.client.query(&format!(
            "SELECT pubkey, d_tag, id, toUInt32(created_at), toUInt64(length(content)), \
             toUInt32(indexed_at) \
             FROM {db}.events_local WHERE kind = 30023 AND pubkey IN ({list})"
        ));
        for p in chunk {
            q = q.bind(p);
        }
        let versions: Vec<ArticleVersion> =
            q.fetch_all().await.context("reading article versions")?;

        let fresh: BTreeSet<&str> = versions
            .iter()
            .filter(|v| v.indexed_at >= ctx.cutoff)
            .map(|v| v.pubkey.as_str())
            .collect();
        report.skipped_fresh += fresh.len();
        report.compared += chunk.len() - fresh.len();
        let expected = latest_versions(&versions);
        let actual: BTreeMap<(String, String), &LatestArticle> = rollup
            .iter()
            .map(|a| ((a.pubkey.clone(), a.d_tag.clone()), a))
            .collect();
        let keys: BTreeSet<&(String, String)> = expected.keys().chain(actual.keys()).collect();
        for key in keys {
            if fresh.contains(key.0.as_str()) {
                continue;
            }
            let label = format!("{} d={:?}", key.0, key.1);
            let (outcome, detail) = match (expected.get(key), actual.get(key)) {
                (Some(e), Some(a)) => {
                    let same = e.id == a.id
                        && e.created_at == a.created_at
                        && e.content_length == a.content_length;
                    (
                        if same {
                            Outcome::Match
                        } else {
                            Outcome::Mismatch
                        },
                        format!(
                            "rollup {} at {} ({} bytes); source {} at {} ({} bytes)",
                            a.id,
                            a.created_at,
                            a.content_length,
                            e.id,
                            e.created_at,
                            e.content_length
                        ),
                    )
                }
                (Some(e), None) => (
                    Outcome::Missing,
                    format!("no rollup row; latest source version {}", e.id),
                ),
                (None, Some(a)) => (
                    Outcome::Excess,
                    format!("rollup row {} has no source version", a.id),
                ),
                (None, None) => unreachable!("key came from one of the maps"),
            };
            report.record(&label, outcome, detail, o.max_examples);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle(kind: i32, events: u64, rows: u64) -> OracleGroup {
        OracleGroup {
            pubkey: "p".into(),
            grain: 1,
            kind,
            period: 20_000,
            rows,
            events,
            kinds: vec![1],
            first: 10,
            last: 20,
            newest_indexed: 30,
        }
    }

    fn rollup(kind: i32, event_count: u64) -> RollupGroup {
        RollupGroup {
            pubkey: "p".into(),
            grain: 1,
            kind,
            period: 20_000,
            event_count,
            kinds: vec![1],
            first: 10,
            last: 20,
        }
    }

    fn outcomes(o: &[OracleGroup], r: &[RollupGroup]) -> Vec<Outcome> {
        compare_publisher_groups(o, r)
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect()
    }

    #[test]
    fn publisher_counts_are_classified() {
        assert_eq!(
            outcomes(&[oracle(1, 3, 3)], &[rollup(1, 3)]),
            [Outcome::Match]
        );
        assert_eq!(
            outcomes(&[oracle(1, 3, 3)], &[rollup(1, 2)]),
            [Outcome::Missing]
        );
        // Counted twice but the duplicate row is still visible: explained.
        assert_eq!(
            outcomes(&[oracle(1, 3, 4)], &[rollup(1, 4)]),
            [Outcome::DuplicateInflation]
        );
        // Counted twice with no duplicate row in the source: a double count.
        assert_eq!(
            outcomes(&[oracle(1, 3, 3)], &[rollup(1, 4)]),
            [Outcome::Excess]
        );
        assert_eq!(outcomes(&[oracle(1, 3, 3)], &[]), [Outcome::Missing]);
        assert_eq!(outcomes(&[], &[rollup(1, 1)]), [Outcome::Excess]);
        let mut wrong_last = rollup(1, 3);
        wrong_last.last = 21;
        assert_eq!(
            outcomes(&[oracle(1, 3, 3)], &[wrong_last]),
            [Outcome::Mismatch]
        );
        assert!(
            group_label(&("p".into(), 2, ALL_KINDS, 20_000))
                .contains("p month 2024-10-04 kind=all")
        );
    }

    fn version(d: &str, id: &str, created_at: u32, len: u64) -> ArticleVersion {
        ArticleVersion {
            pubkey: "p".into(),
            d_tag: d.into(),
            id: id.into(),
            created_at,
            content_length: len,
            indexed_at: 0,
        }
    }

    #[test]
    fn latest_version_prefers_newest_then_lowest_id() {
        let versions = [
            version("a", "b2", 10, 1),
            version("a", "b1", 10, 2),
            version("a", "a9", 9, 3),
            version("b", "z", 5, 4),
            version("a", "b3", 10, 5),
        ];
        let winners = latest_versions(&versions);
        assert_eq!(winners[&("p".into(), "a".into())].id, "b1");
        assert_eq!(winners[&("p".into(), "b".into())].id, "z");
    }

    #[test]
    fn seeded_slices_are_valid_hex_ranges() {
        for seed in [0, 1, 19, u64::MAX] {
            let (start, end) = seeded_id_range(seed);
            assert_eq!(start.len(), 3);
            assert!(start.bytes().all(|b| b.is_ascii_hexdigit()));
            if let Some(end) = end {
                assert!(end > start);
            }
        }
        assert_eq!(id_range_predicate("ab", None), "id >= 'ab'");
    }

    #[test]
    fn report_passes_only_with_comparisons_and_no_failures() {
        let mut r = TargetReport::new(Target::Publishers);
        assert!(r.inconclusive());
        r.compared = 1;
        r.record("k", Outcome::Match, String::new(), 5);
        r.record("k", Outcome::DuplicateInflation, "d".into(), 5);
        assert_eq!(r.failures(), 0);
        let report = ValidationReport {
            cutoff: 0,
            seed: 0,
            targets: vec![r.clone()],
        };
        assert!(report.passed());
        r.record("k", Outcome::Excess, "x".into(), 5);
        let report = ValidationReport {
            cutoff: 0,
            seed: 0,
            targets: vec![r],
        };
        assert!(!report.passed());
        assert!(report.render().contains("failed rows: 1"));
    }
}
