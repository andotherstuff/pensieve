//! Validate compact lookups against the source events and the legacy reads.
//!
//! Keys are sampled deterministically from `events_local` (not from the
//! compact tables, so missing keys are detected): a hashed random subset plus
//! the hottest keys. For each key the harness:
//!
//! 1. reads every source version and computes the expected answer with the
//!    independent Rust [`oracle`](super::oracle);
//! 2. skips the key if any source row was indexed after the cutoff — checked
//!    both before and again after the compact and legacy reads, so a
//!    concurrent insert cannot be misread as a difference;
//! 3. requires the compact read (the exact function the preview uses) to
//!    equal the oracle — any difference is **unexplained**;
//! 4. compares the legacy read and classifies differences as known legacy
//!    defects (timestamp tie-breaks, id/content from different rows,
//!    duplicate inflation, missing amount rows) or unexplained.
//!
//! A target whose sampled keys were all skipped is **inconclusive**, not a
//! pass.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::compact;
use super::ops;
use super::oracle::{self, SourceEvent};
use super::plan::projection_client;
use super::target::{D_TAG_EXPR, Target, ZAP_TARGETS_EXPR};
use crate::query;

/// Validation options.
#[derive(Debug, Clone)]
pub struct ValidateOptions {
    /// Targets to validate.
    pub targets: Vec<Target>,
    /// Random keys per target.
    pub sample: usize,
    /// Hottest keys per target.
    pub hot: usize,
    /// Sampling seed.
    pub seed: u64,
    /// Random keys are drawn from `cityHash64(key, seed) % 10000 < permyriad`.
    pub sample_permyriad: u32,
    /// Compare only data indexed at least this long ago.
    pub cutoff_margin: Duration,
    /// Keys per source query.
    pub batch: usize,
    /// Maximum legacy per-key reads for zaps and naddr (each scans a kind slice).
    pub legacy_sample: usize,
    /// Force sampling and source reads through `events_by_kind`.
    pub require_projection: bool,
    /// Differences to keep as examples per target.
    pub max_examples: usize,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        Self {
            targets: Target::ALL.to_vec(),
            sample: 500,
            hot: 50,
            seed: 17,
            sample_permyriad: 100,
            cutoff_margin: Duration::from_secs(300),
            batch: 200,
            legacy_sample: 100,
            require_projection: true,
            max_examples: 20,
        }
    }
}

/// Classification of one comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Results agree.
    Match,
    /// Legacy picked a different event among several with the newest
    /// `created_at` (the compact rule picks the lowest id).
    LegacyTieBreak,
    /// Legacy returned an id and content from different events.
    LegacyIdContentMix,
    /// Legacy counted unmerged duplicate rows.
    LegacyDuplicateInflation,
    /// Legacy missed receipts lacking a `zap_amounts_data` row.
    LegacyAmountRowMissing,
    /// Not explained by a known legacy defect.
    Unexplained,
}

/// A recorded difference.
#[derive(Debug, Clone, Serialize)]
pub struct Difference {
    /// Which comparison (`compact` vs source, or `legacy` vs source).
    pub side: &'static str,
    /// Lookup key.
    pub key: String,
    /// Classification.
    pub outcome: Outcome,
    /// Human-readable detail.
    pub detail: String,
}

/// Validation results for one target.
#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    /// Target.
    pub target: Target,
    /// Keys sampled.
    pub sampled: usize,
    /// Keys skipped because source rows were indexed after the cutoff
    /// (before or during validation).
    pub skipped_after_cutoff: usize,
    /// Compact-vs-source outcomes.
    pub compact: BTreeMap<Outcome, u64>,
    /// Legacy-vs-source outcomes.
    pub legacy: BTreeMap<Outcome, u64>,
    /// Example differences.
    pub examples: Vec<Difference>,
}

impl TargetReport {
    fn new(target: Target, sampled: usize) -> Self {
        Self {
            target,
            sampled,
            skipped_after_cutoff: 0,
            compact: BTreeMap::new(),
            legacy: BTreeMap::new(),
            examples: Vec::new(),
        }
    }

    fn record(
        &mut self,
        side: &'static str,
        key: &str,
        outcome: Outcome,
        detail: impl FnOnce() -> String,
        max_examples: usize,
    ) {
        let counts = if side == "compact" {
            &mut self.compact
        } else {
            &mut self.legacy
        };
        *counts.entry(outcome).or_default() += 1;
        if outcome != Outcome::Match && self.examples.len() < max_examples {
            self.examples.push(Difference {
                side,
                key: key.to_string(),
                outcome,
                detail: detail(),
            });
        }
    }

    /// Keys whose compact read was compared with the source.
    pub fn compared(&self) -> u64 {
        self.compact.values().sum()
    }

    /// Keys were sampled but none could be compared, so the target provides
    /// no correctness evidence.
    pub fn inconclusive(&self) -> bool {
        self.sampled > 0 && self.compared() == 0
    }

    /// Unexplained differences on either side.
    pub fn unexplained(&self) -> u64 {
        self.compact
            .get(&Outcome::Unexplained)
            .copied()
            .unwrap_or(0)
            + self.legacy.get(&Outcome::Unexplained).copied().unwrap_or(0)
    }
}

/// Full validation report.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    /// Cutoff (Unix seconds): only data indexed at or before this is compared.
    pub cutoff: u32,
    /// Sampling seed.
    pub seed: u64,
    /// Per-target results.
    pub targets: Vec<TargetReport>,
}

impl ValidationReport {
    /// Total unexplained differences.
    pub fn unexplained(&self) -> u64 {
        self.targets.iter().map(TargetReport::unexplained).sum()
    }

    /// Targets with sampled keys but no comparisons.
    pub fn inconclusive_targets(&self) -> Vec<Target> {
        self.targets
            .iter()
            .filter(|t| t.inconclusive())
            .map(|t| t.target)
            .collect()
    }

    /// No unexplained differences and every target compared at least one key.
    pub fn passed(&self) -> bool {
        self.unexplained() == 0 && self.inconclusive_targets().is_empty()
    }

    /// Text summary.
    pub fn render(&self) -> String {
        let mut out = format!("cutoff {} (seed {})\n", self.cutoff, self.seed);
        for t in &self.targets {
            out.push_str(&format!(
                "{}: sampled {}, compared {}, skipped after cutoff {}\n  compact vs source: {:?}\n  legacy vs source:  {:?}\n",
                t.target, t.sampled, t.compared(), t.skipped_after_cutoff, t.compact, t.legacy
            ));
            for d in &t.examples {
                out.push_str(&format!(
                    "    [{}] {:?} {}: {}\n",
                    d.side, d.outcome, d.key, d.detail
                ));
            }
        }
        out.push_str(&format!(
            "unexplained differences: {}\n",
            self.unexplained()
        ));
        let inconclusive = self.inconclusive_targets();
        if !inconclusive.is_empty() {
            out.push_str(&format!(
                "INCONCLUSIVE (no keys compared): {inconclusive:?}; lower --cutoff-margin-secs \
                 only if ingest is quiet, or retry later\n"
            ));
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct SourceRow {
    id: String,
    pubkey: String,
    created_at: u32,
    kind: u16,
    content: String,
    tags: Vec<Vec<String>>,
    indexed_at: u32,
}

impl SourceRow {
    fn event(&self) -> SourceEvent {
        SourceEvent {
            id: self.id.clone(),
            pubkey: self.pubkey.clone(),
            created_at: self.created_at,
            kind: self.kind,
            content: self.content.clone(),
            tags: self.tags.clone(),
        }
    }
}

/// Run validation. `client` should carry resource limits.
pub async fn run(
    client: &Client,
    database: &str,
    options: &ValidateOptions,
) -> Result<ValidationReport> {
    let now = ops::server_now(client).await?;
    let cutoff = now.saturating_sub(options.cutoff_margin.as_secs() as u32);
    let source = projection_client(client, options.require_projection);
    let ctx = Ctx {
        db: database,
        source: &source,
        compact: client,
        legacy: client,
        options,
        cutoff,
    };

    let mut targets = Vec::new();
    for target in &options.targets {
        let report = match target {
            Target::Profiles => validate_profiles(&ctx).await?,
            Target::Zaps => validate_zaps(&ctx).await?,
            Target::Addressable => validate_addressable(&ctx).await?,
        };
        targets.push(report);
    }
    Ok(ValidationReport {
        cutoff,
        seed: options.seed,
        targets,
    })
}

struct Ctx<'a> {
    db: &'a str,
    source: &'a Client,
    compact: &'a Client,
    legacy: &'a Client,
    options: &'a ValidateOptions,
    cutoff: u32,
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn merge_keys<K: Ord + Clone>(random: Vec<K>, hot: Vec<K>) -> Vec<K> {
    let mut seen = BTreeSet::new();
    random
        .into_iter()
        .chain(hot)
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

async fn validate_profiles(ctx: &Ctx<'_>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let random = ctx
        .source
        .query(&format!(
            "SELECT pubkey FROM {db}.events_local \
             WHERE kind = 0 AND cityHash64(pubkey, toUInt64({seed})) % 10000 < {p} \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64({seed})) LIMIT {n}",
            seed = o.seed,
            p = o.sample_permyriad,
            n = o.sample
        ))
        .fetch_all::<String>()
        .await
        .context("sampling profile keys")?;
    let hot = ctx
        .source
        .query(&format!(
            "SELECT pubkey FROM {db}.events_local WHERE kind = 0 \
             GROUP BY pubkey ORDER BY count() DESC, pubkey LIMIT {}",
            o.hot
        ))
        .fetch_all::<String>()
        .await
        .context("sampling hot profile keys")?;
    let keys = merge_keys(random, hot);
    let mut report = TargetReport::new(Target::Profiles, keys.len());

    for batch in keys.chunks(o.batch.max(1)) {
        let sql = format!(
            "SELECT id, pubkey, toUInt32(created_at) AS created_at, kind, content, tags, \
             toUInt32(indexed_at) AS indexed_at \
             FROM {db}.events_local WHERE kind = 0 AND pubkey IN ({})",
            placeholders(batch.len())
        );
        let mut q = ctx.source.query(&sql);
        for k in batch {
            q = q.bind(k);
        }
        let rows = q
            .fetch_all::<SourceRow>()
            .await
            .context("reading profile sources")?;
        let mut by_key: HashMap<&str, Vec<&SourceRow>> = HashMap::new();
        for row in &rows {
            by_key.entry(row.pubkey.as_str()).or_default().push(row);
        }

        let compact_rows = compact::fetch_profiles(ctx.compact, batch).await?;
        let legacy_rows = query::fetch_profiles(ctx.legacy, batch).await?;
        let changed = profiles_changed_after(ctx.source, db, batch, ctx.cutoff).await?;

        for key in batch {
            let versions = by_key.get(key.as_str()).cloned().unwrap_or_default();
            if changed.contains(key) || versions.iter().any(|r| r.indexed_at > ctx.cutoff) {
                report.skipped_after_cutoff += 1;
                continue;
            }
            let events: Vec<SourceEvent> = versions.iter().map(|r| r.event()).collect();
            let expected = oracle::replaceable_winner(&events);

            let compact_outcome = match (expected, compact_rows.get(key)) {
                (None, None) => Outcome::Match,
                (Some(e), Some(c)) if c.event_id == e.id && c.content == e.content => {
                    Outcome::Match
                }
                _ => Outcome::Unexplained,
            };
            report.record(
                "compact",
                key,
                compact_outcome,
                || {
                    format!(
                        "expected {:?}, compact {:?}",
                        expected.map(|e| &e.id),
                        compact_rows.get(key).map(|c| &c.event_id)
                    )
                },
                o.max_examples,
            );

            let legacy = legacy_rows.get(key);
            let legacy_outcome = match (expected, legacy) {
                (None, None) => Outcome::Match,
                (Some(e), Some(l)) if l.event_id == e.id && l.content == e.content => {
                    Outcome::Match
                }
                (Some(_), Some(l)) => {
                    let id_event = events.iter().find(|e| e.id == l.event_id);
                    if id_event.is_some_and(|e| e.content != l.content) {
                        Outcome::LegacyIdContentMix
                    } else if oracle::tie_width(&events) > 1 {
                        Outcome::LegacyTieBreak
                    } else {
                        Outcome::Unexplained
                    }
                }
                _ => Outcome::Unexplained,
            };
            report.record(
                "legacy",
                key,
                legacy_outcome,
                || {
                    format!(
                        "expected {:?}, legacy {:?}, tie width {}",
                        expected.map(|e| &e.id),
                        legacy.map(|l| &l.event_id),
                        oracle::tie_width(&events)
                    )
                },
                o.max_examples,
            );
        }
    }
    Ok(report)
}

async fn validate_zaps(ctx: &Ctx<'_>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let random = ctx
        .source
        .query(&format!(
            "SELECT target FROM (SELECT tags FROM {db}.events_local WHERE kind = 9735) \
             ARRAY JOIN {ZAP_TARGETS_EXPR} AS target \
             WHERE cityHash64(target, toUInt64({seed})) % 10000 < {p} \
             GROUP BY target ORDER BY cityHash64(target, toUInt64({seed})) LIMIT {n}",
            seed = o.seed,
            p = o.sample_permyriad,
            n = o.sample
        ))
        .fetch_all::<String>()
        .await
        .context("sampling zap targets")?;
    let hot = ctx
        .source
        .query(&format!(
            "SELECT target FROM (SELECT tags FROM {db}.events_local WHERE kind = 9735) \
             ARRAY JOIN {ZAP_TARGETS_EXPR} AS target \
             GROUP BY target ORDER BY count() DESC, target LIMIT {}",
            o.hot
        ))
        .fetch_all::<String>()
        .await
        .context("sampling hot zap targets")?;
    let keys = merge_keys(random, hot);
    let mut report = TargetReport::new(Target::Zaps, keys.len());
    let mut legacy_checked = 0usize;

    for batch in keys.chunks(o.batch.max(1)) {
        let sql = format!(
            "SELECT id, pubkey, toUInt32(created_at) AS created_at, kind, '' AS content, tags, \
             toUInt32(indexed_at) AS indexed_at \
             FROM {db}.events_local \
             WHERE kind = 9735 AND hasAny({ZAP_TARGETS_EXPR}, [{}])",
            placeholders(batch.len())
        );
        let mut q = ctx.source.query(&sql);
        for k in batch {
            q = q.bind(k);
        }
        let rows = q
            .fetch_all::<SourceRow>()
            .await
            .context("reading zap receipts")?;
        let mut by_target: HashMap<String, Vec<&SourceRow>> = HashMap::new();
        for row in &rows {
            for target in oracle::zap_targets(&row.tags) {
                by_target.entry(target).or_default().push(row);
            }
        }

        // Read compact (and some legacy) results first, then re-check source
        // freshness so keys changed during the reads are skipped.
        let mut reads = Vec::new();
        for key in batch {
            let receipts = by_target.get(key).cloned().unwrap_or_default();
            if receipts.iter().any(|r| r.indexed_at > ctx.cutoff) {
                report.skipped_after_cutoff += 1;
                continue;
            }
            let compact_totals = compact::fetch_zap_totals(ctx.compact, key).await?;
            let legacy = if legacy_checked < o.legacy_sample {
                legacy_checked += 1;
                Some(query::fetch_zap_totals(ctx.legacy, key).await?)
            } else {
                None
            };
            reads.push((key, receipts, compact_totals, legacy));
        }
        let changed = zap_targets_changed_after(ctx.source, db, batch, ctx.cutoff).await?;

        for (key, receipts, compact_totals, legacy) in reads {
            if changed.contains(key) {
                report.skipped_after_cutoff += 1;
                continue;
            }
            let events: Vec<SourceEvent> = receipts.iter().map(|r| r.event()).collect();
            let expected = oracle::zap_totals_by_target(&events)
                .remove(key.as_str())
                .unwrap_or_default();

            let outcome = if compact_totals == expected {
                Outcome::Match
            } else {
                Outcome::Unexplained
            };
            report.record(
                "compact",
                key,
                outcome,
                || format!("expected {expected:?}, compact {compact_totals:?}"),
                o.max_examples,
            );

            let Some(legacy) = legacy else {
                continue;
            };
            let outcome = if legacy == expected {
                Outcome::Match
            } else if legacy.zap_count > expected.zap_count && zap_duplicates(ctx, key).await? > 0 {
                Outcome::LegacyDuplicateInflation
            } else if legacy.zap_count < expected.zap_count {
                Outcome::LegacyAmountRowMissing
            } else {
                Outcome::Unexplained
            };
            report.record(
                "legacy",
                key,
                outcome,
                || format!("expected {expected:?}, legacy {legacy:?}"),
                o.max_examples,
            );
        }
    }
    Ok(report)
}

/// Unmerged duplicate rows among a target's receipts in `events_local` and
/// `zap_amounts_data`.
async fn zap_duplicates(ctx: &Ctx<'_>, target: &str) -> Result<u64> {
    let db = ctx.db;
    ctx.source
        .query(&format!(
            "WITH receipts AS ( \
                 SELECT id FROM {db}.events_local \
                 WHERE kind = 9735 AND has({ZAP_TARGETS_EXPR}, ?) \
             ) \
             SELECT (SELECT count() - uniqExact(id) FROM receipts) \
             + (SELECT count() - uniqExact(event_id) FROM {db}.zap_amounts_data \
                WHERE event_id IN (SELECT id FROM receipts))"
        ))
        .bind(target)
        .fetch_one::<u64>()
        .await
        .context("counting zap duplicates")
}

async fn validate_addressable(ctx: &Ctx<'_>) -> Result<TargetReport> {
    let (db, o) = (ctx.db, ctx.options);
    let kinds = Target::Addressable.kind_predicate();
    let random = ctx
        .source
        .query(&format!(
            "SELECT pubkey, kind, {D_TAG_EXPR} AS d FROM {db}.events_local \
             WHERE {kinds} AND cityHash64(pubkey, kind, d, toUInt64({seed})) % 10000 < {p} \
             GROUP BY pubkey, kind, d \
             ORDER BY cityHash64(pubkey, kind, d, toUInt64({seed})) LIMIT {n}",
            seed = o.seed,
            p = o.sample_permyriad,
            n = o.sample
        ))
        .fetch_all::<(String, u16, String)>()
        .await
        .context("sampling naddr coordinates")?;
    let hot = ctx
        .source
        .query(&format!(
            "SELECT pubkey, kind, {D_TAG_EXPR} AS d FROM {db}.events_local WHERE {kinds} \
             GROUP BY pubkey, kind, d ORDER BY count() DESC, pubkey, kind, d LIMIT {}",
            o.hot
        ))
        .fetch_all::<(String, u16, String)>()
        .await
        .context("sampling hot naddr coordinates")?;
    let keys = merge_keys(random, hot);
    let mut report = TargetReport::new(Target::Addressable, keys.len());
    let mut legacy_checked = 0usize;

    for batch in keys.chunks(o.batch.max(1)) {
        let pubkeys: BTreeSet<&str> = batch.iter().map(|(p, _, _)| p.as_str()).collect();
        let kind_list: BTreeSet<u16> = batch.iter().map(|(_, k, _)| *k).collect();
        let kind_list = kind_list
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, pubkey, toUInt32(created_at) AS created_at, kind, '' AS content, tags, \
             toUInt32(indexed_at) AS indexed_at \
             FROM {db}.events_local \
             WHERE {kinds} AND kind IN ({kind_list}) AND pubkey IN ({})",
            placeholders(pubkeys.len())
        );
        let mut q = ctx.source.query(&sql);
        for p in &pubkeys {
            q = q.bind(*p);
        }
        let rows = q
            .fetch_all::<SourceRow>()
            .await
            .context("reading naddr sources")?;
        let mut by_key: HashMap<(&str, u16, &str), Vec<&SourceRow>> = HashMap::new();
        for row in &rows {
            by_key
                .entry((row.pubkey.as_str(), row.kind, oracle::d_tag(&row.tags)))
                .or_default()
                .push(row);
        }

        // Read compact (and some legacy) results first, then re-check source
        // freshness so coordinates changed during the reads are skipped.
        let mut reads = Vec::new();
        for coordinate in batch {
            let (pubkey, kind, d) = coordinate;
            let versions = by_key
                .get(&(pubkey.as_str(), *kind, d.as_str()))
                .cloned()
                .unwrap_or_default();
            if versions.iter().any(|r| r.indexed_at > ctx.cutoff) {
                report.skipped_after_cutoff += 1;
                continue;
            }
            let compact_id = compact::fetch_replaceable(ctx.compact, *kind, pubkey, d)
                .await?
                .map(|e| e.id);
            let legacy_id = if legacy_checked < o.legacy_sample {
                legacy_checked += 1;
                Some(
                    query::fetch_replaceable(ctx.legacy, *kind, pubkey, d)
                        .await?
                        .map(|e| e.id),
                )
            } else {
                None
            };
            reads.push((coordinate, versions, compact_id, legacy_id));
        }
        let changed = coordinates_changed_after(ctx.source, db, batch, ctx.cutoff).await?;

        for (coordinate, versions, compact_id, legacy_id) in reads {
            if changed.contains(coordinate) {
                report.skipped_after_cutoff += 1;
                continue;
            }
            let (pubkey, kind, d) = coordinate;
            let label = format!("{kind}:{pubkey}:{d}");
            let events: Vec<SourceEvent> = versions.iter().map(|r| r.event()).collect();
            let expected = oracle::replaceable_winner(&events).map(|e| e.id.clone());

            let outcome = if compact_id == expected {
                Outcome::Match
            } else {
                Outcome::Unexplained
            };
            report.record(
                "compact",
                &label,
                outcome,
                || format!("expected {expected:?}, compact {compact_id:?}"),
                o.max_examples,
            );

            let Some(legacy_id) = legacy_id else {
                continue;
            };
            let outcome = if legacy_id == expected {
                Outcome::Match
            } else if oracle::tie_width(&events) > 1 {
                Outcome::LegacyTieBreak
            } else {
                Outcome::Unexplained
            };
            report.record(
                "legacy",
                &label,
                outcome,
                || {
                    format!(
                        "expected {expected:?}, legacy {legacy_id:?}, tie width {}",
                        oracle::tie_width(&events)
                    )
                },
                o.max_examples,
            );
        }
    }
    Ok(report)
}

/// Pubkeys with a kind 0 row indexed after `cutoff`.
///
/// Validation calls this after the compact and legacy reads, so an insert
/// that landed between the source read and those reads is skipped rather
/// than reported as a difference.
pub async fn profiles_changed_after(
    source: &Client,
    database: &str,
    pubkeys: &[String],
    cutoff: u32,
) -> Result<HashSet<String>> {
    if pubkeys.is_empty() {
        return Ok(HashSet::new());
    }
    let mut q = source.query(&format!(
        "SELECT DISTINCT pubkey FROM {database}.events_local \
         WHERE kind = 0 AND indexed_at > toDateTime({cutoff}) AND pubkey IN ({})",
        placeholders(pubkeys.len())
    ));
    for pk in pubkeys {
        q = q.bind(pk);
    }
    let rows = q
        .fetch_all::<String>()
        .await
        .context("re-checking profile freshness")?;
    Ok(rows.into_iter().collect())
}

/// Zap targets with a receipt indexed after `cutoff`.
pub async fn zap_targets_changed_after(
    source: &Client,
    database: &str,
    targets: &[String],
    cutoff: u32,
) -> Result<HashSet<String>> {
    if targets.is_empty() {
        return Ok(HashSet::new());
    }
    let list = placeholders(targets.len());
    let mut q = source.query(&format!(
        "SELECT DISTINCT target FROM ( \
             SELECT tags FROM {database}.events_local \
             WHERE kind = 9735 AND indexed_at > toDateTime({cutoff}) \
             AND hasAny({ZAP_TARGETS_EXPR}, [{list}]) \
         ) ARRAY JOIN {ZAP_TARGETS_EXPR} AS target WHERE target IN ({list})"
    ));
    for _ in 0..2 {
        for target in targets {
            q = q.bind(target);
        }
    }
    let rows = q
        .fetch_all::<String>()
        .await
        .context("re-checking zap freshness")?;
    Ok(rows.into_iter().collect())
}

/// naddr coordinates with a version indexed after `cutoff`.
pub async fn coordinates_changed_after(
    source: &Client,
    database: &str,
    coordinates: &[(String, u16, String)],
    cutoff: u32,
) -> Result<HashSet<(String, u16, String)>> {
    if coordinates.is_empty() {
        return Ok(HashSet::new());
    }
    let pubkeys: BTreeSet<&str> = coordinates.iter().map(|(p, _, _)| p.as_str()).collect();
    let kinds: BTreeSet<u16> = coordinates.iter().map(|(_, k, _)| *k).collect();
    let kind_list = kinds
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let mut q = source.query(&format!(
        "SELECT DISTINCT pubkey, kind, {D_TAG_EXPR} AS d FROM {database}.events_local \
         WHERE {} AND kind IN ({kind_list}) AND indexed_at > toDateTime({cutoff}) \
         AND pubkey IN ({})",
        Target::Addressable.kind_predicate(),
        placeholders(pubkeys.len())
    ));
    for pk in &pubkeys {
        q = q.bind(*pk);
    }
    let wanted: HashSet<&(String, u16, String)> = coordinates.iter().collect();
    let rows = q
        .fetch_all::<(String, u16, String)>()
        .await
        .context("re-checking naddr freshness")?;
    Ok(rows.into_iter().filter(|c| wanted.contains(c)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keys_dedupes_and_keeps_order() {
        let merged = merge_keys(vec!["b", "a"], vec!["a", "c", "b"]);
        assert_eq!(merged, vec!["b", "a", "c"]);
    }

    #[test]
    fn report_counts_unexplained_on_both_sides() {
        let mut report = TargetReport::new(Target::Profiles, 3);
        report.record("compact", "k1", Outcome::Match, String::new, 5);
        report.record("compact", "k2", Outcome::Unexplained, || "x".into(), 5);
        report.record("legacy", "k1", Outcome::LegacyTieBreak, || "y".into(), 5);
        report.record("legacy", "k2", Outcome::Unexplained, || "z".into(), 1);
        assert_eq!(report.unexplained(), 2);
        // The last record's cap of 1 is already reached, so it is not kept.
        assert_eq!(report.examples.len(), 2);
    }

    #[test]
    fn all_skipped_target_is_inconclusive() {
        let mut skipped = TargetReport::new(Target::Zaps, 4);
        skipped.skipped_after_cutoff = 4;
        assert_eq!(skipped.compared(), 0);
        assert!(skipped.inconclusive());

        let empty = TargetReport::new(Target::Addressable, 0);
        assert!(!empty.inconclusive(), "an empty source is vacuously fine");

        let mut compared = TargetReport::new(Target::Profiles, 2);
        compared.skipped_after_cutoff = 1;
        compared.record("compact", "k", Outcome::Match, String::new, 5);
        assert!(!compared.inconclusive());

        let report = ValidationReport {
            cutoff: 0,
            seed: 0,
            targets: vec![compared.clone(), skipped],
        };
        assert_eq!(report.unexplained(), 0);
        assert_eq!(report.inconclusive_targets(), vec![Target::Zaps]);
        assert!(!report.passed());
        assert!(report.render().contains("INCONCLUSIVE"));

        let ok = ValidationReport {
            cutoff: 0,
            seed: 0,
            targets: vec![compared],
        };
        assert!(ok.passed());
    }

    #[test]
    fn placeholders_format() {
        assert_eq!(placeholders(3), "?, ?, ?");
        assert_eq!(placeholders(1), "?");
    }
}
