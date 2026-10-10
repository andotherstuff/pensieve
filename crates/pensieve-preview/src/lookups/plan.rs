//! Backfill shard plans, disk estimates and on-disk checkpoint layout.
//!
//! A plan splits one target's source rows into contiguous shards. The first
//! and last `created_at` shards are open-ended, so every row is covered
//! regardless of timestamp (including `created_at = 0` and future values).
//! Plans and per-shard checkpoints are JSON files under
//! `<checkpoint_dir>/<target>/`, which makes backfills resumable.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::ops::{self, GIB};
use super::target::Target;

/// Plan file format version.
pub const PLAN_FORMAT_VERSION: u32 = 1;

/// Assumed compression ratio for random hex and fixed-width columns.
const HEX_COMPRESSION_RATIO: f64 = 0.6;

/// Fallback compression ratio for free text when `system.columns` has none.
const TEXT_COMPRESSION_RATIO: f64 = 0.5;

/// How shards partition the source rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ShardBy {
    /// `created_at` windows; reads only the target kinds through the
    /// `events_by_kind` projection. Use while the projection exists.
    CreatedAt,
    /// Event id prefixes; each shard reads a primary-key range of the full
    /// table. Only for rebuilds after the projection has been removed.
    IdPrefix,
}

/// The source-row range a shard covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum ShardRange {
    /// `start <= created_at < end`; `None` is unbounded.
    CreatedAt {
        /// Inclusive lower bound (Unix seconds).
        start: Option<u32>,
        /// Exclusive upper bound (Unix seconds).
        end: Option<u32>,
    },
    /// `start <= id < end`; `None` is unbounded. Bounds are lowercase hex.
    IdPrefix {
        /// Inclusive lower bound.
        start: Option<String>,
        /// Exclusive upper bound.
        end: Option<String>,
    },
    /// `start <= indexed_at <= end`, used by the gap-closure pass.
    IndexedAt {
        /// Inclusive lower bound (Unix seconds).
        start: u32,
        /// Inclusive upper bound (Unix seconds).
        end: u32,
    },
}

impl ShardRange {
    /// SQL predicate for this range. Values are integers or validated hex, so
    /// the result is safe to embed.
    pub fn predicate(&self) -> String {
        let mut parts = Vec::new();
        match self {
            Self::CreatedAt { start, end } => {
                if let Some(start) = start {
                    parts.push(format!("created_at >= toDateTime({start})"));
                }
                if let Some(end) = end {
                    parts.push(format!("created_at < toDateTime({end})"));
                }
            }
            Self::IdPrefix { start, end } => {
                if let Some(start) = start {
                    assert!(is_hex(start), "id prefix bound must be lowercase hex");
                    parts.push(format!("id >= '{start}'"));
                }
                if let Some(end) = end {
                    assert!(is_hex(end), "id prefix bound must be lowercase hex");
                    parts.push(format!("id < '{end}'"));
                }
            }
            Self::IndexedAt { start, end } => {
                parts.push(format!("indexed_at >= toDateTime({start})"));
                parts.push(format!("indexed_at <= toDateTime({end})"));
            }
        }
        if parts.is_empty() {
            "1".to_string()
        } else {
            parts.join(" AND ")
        }
    }
}

fn is_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One unit of backfill work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    /// Position in the plan.
    pub index: usize,
    /// Source rows covered.
    pub range: ShardRange,
    /// Source rows expected in the range at planning time (0 if unknown).
    pub estimated_rows: u64,
}

/// Split a per-hour source histogram into contiguous `created_at` shards of
/// at most `max_rows` rows (a single hour larger than `max_rows` is its own
/// shard). The first shard has no lower bound and the last no upper bound.
pub fn shards_from_histogram(histogram: &[(u32, u64)], max_rows: u64) -> Vec<Shard> {
    let max_rows = max_rows.max(1);
    let mut boundaries: Vec<u32> = Vec::new();
    let mut rows_per_shard: Vec<u64> = vec![0];

    for &(bucket, rows) in histogram {
        let current = *rows_per_shard.last().expect("non-empty");
        if current > 0 && current.saturating_add(rows) > max_rows {
            boundaries.push(bucket);
            rows_per_shard.push(0);
        }
        *rows_per_shard.last_mut().expect("non-empty") += rows;
    }

    let mut shards = Vec::with_capacity(rows_per_shard.len());
    for (index, estimated_rows) in rows_per_shard.into_iter().enumerate() {
        let start = index.checked_sub(1).map(|i| boundaries[i]);
        let end = boundaries.get(index).copied();
        shards.push(Shard {
            index,
            range: ShardRange::CreatedAt { start, end },
            estimated_rows,
        });
    }
    shards
}

/// Contiguous id-prefix shards over `16^prefix_len` buckets, with open-ended
/// first and last shards.
pub fn id_prefix_shards(prefix_len: usize, total_rows: u64) -> Vec<Shard> {
    assert!((1..=3).contains(&prefix_len), "prefix length must be 1..=3");
    let buckets = 16usize.pow(prefix_len as u32);
    let per_shard = total_rows / buckets as u64;
    (0..buckets)
        .map(|index| {
            let bound = |i: usize| format!("{i:0width$x}", width = prefix_len);
            Shard {
                index,
                range: ShardRange::IdPrefix {
                    start: (index > 0).then(|| bound(index)),
                    end: (index + 1 < buckets).then(|| bound(index + 1)),
                },
                estimated_rows: per_shard,
            }
        })
        .collect()
}

/// Source statistics for a target.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, clickhouse::Row)]
pub struct SourceStats {
    /// Source rows in `events_local` (pre-merge duplicates included).
    pub source_rows: u64,
    /// Approximate distinct keys (pubkeys, receipts or coordinates).
    pub distinct_keys: u64,
    /// Approximate rows in the merged target table.
    pub output_rows: u64,
    /// Mean free-text payload bytes per output row (content or d tag).
    pub avg_payload_bytes: f64,
}

/// Disk estimate for a backfill.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskEstimate {
    /// Estimated compressed bytes per target row.
    pub bytes_per_row: u64,
    /// Rows written by the backfill before merges (states/rows across shards).
    pub pre_merge_rows: u64,
    /// Estimated size after merges.
    pub final_bytes: u64,
    /// Estimated size written by the backfill before merges.
    pub pre_merge_bytes: u64,
    /// Peak disk: pre-merge bytes plus equal merge headroom (a merge writes
    /// its output while its inputs, then outdated parts, still exist).
    pub peak_bytes: u64,
    /// Compression ratio used for free text.
    pub text_compression_ratio: f64,
}

/// Estimate disk use for backfilling `target`.
pub fn estimate_disk(
    target: Target,
    stats: &SourceStats,
    shards: &[Shard],
    text_compression_ratio: f64,
) -> DiskEstimate {
    let (fixed, payload) = target.row_bytes(stats.avg_payload_bytes);
    let bytes_per_row =
        (fixed * HEX_COMPRESSION_RATIO + payload * text_compression_ratio).ceil() as u64;

    let pre_merge_rows = if target.aggregates_per_key() {
        // A shard writes at most one state per key it contains.
        shards
            .iter()
            .map(|s| s.estimated_rows.min(stats.distinct_keys))
            .sum::<u64>()
            .max(stats.output_rows)
    } else {
        stats.output_rows
    };

    let final_bytes = stats.output_rows.saturating_mul(bytes_per_row);
    let pre_merge_bytes = pre_merge_rows.saturating_mul(bytes_per_row);
    DiskEstimate {
        bytes_per_row,
        pre_merge_rows,
        final_bytes,
        pre_merge_bytes,
        peak_bytes: pre_merge_bytes.saturating_mul(2),
        text_compression_ratio,
    }
}

/// A persisted backfill plan for one target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// File format version.
    pub format_version: u32,
    /// Database containing `events_local` and the target.
    pub database: String,
    /// Target table.
    pub target: Target,
    /// Shard dimension.
    pub shard_by: ShardBy,
    /// Whether every shard must be served by the `events_by_kind` projection.
    pub require_projection: bool,
    /// Creation time (Unix seconds) of the target's materialized view. Rows
    /// inserted after this are captured by the view.
    pub view_created_at: u32,
    /// Server time when the plan was made.
    pub planned_at: u32,
    /// Source statistics at planning time.
    pub source: SourceStats,
    /// Disk estimate.
    pub estimate: DiskEstimate,
    /// Shards in execution order.
    pub shards: Vec<Shard>,
}

/// Options for [`build_plan`].
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Shard dimension.
    pub shard_by: ShardBy,
    /// Target source rows per `created_at` shard.
    pub max_rows_per_shard: u64,
    /// Hex prefix length for id-prefix shards.
    pub id_prefix_len: usize,
    /// Require the `events_by_kind` projection for planning and backfill.
    pub require_projection: bool,
}

/// Apply projection settings for queries that must read `events_by_kind`.
pub fn projection_client(client: &Client, require_projection: bool) -> Client {
    if require_projection {
        client
            .clone()
            .with_option("optimize_use_projections", "1")
            .with_option("force_optimize_projection", "1")
    } else {
        client.clone()
    }
}

/// Inspect the source and build a plan for `target`.
pub async fn build_plan(
    client: &Client,
    database: &str,
    target: Target,
    options: &PlanOptions,
) -> Result<Plan> {
    if options.shard_by == ShardBy::IdPrefix && options.require_projection {
        bail!("id-prefix shards read the base table; pass --allow-full-scan");
    }

    let view_created_at = ops::require_view(client, database, target.materialized_view()).await?;
    let planned_at = ops::server_now(client).await?;
    let source_client = projection_client(client, options.require_projection);

    let source = source_client
        .query(&target.source_stats_sql(database))
        .fetch_one::<SourceStats>()
        .await
        .with_context(|| format!("reading {target} source statistics"))?;

    let shards = match options.shard_by {
        ShardBy::CreatedAt => {
            let histogram = source_client
                .query(&target.hour_histogram_sql(database))
                .fetch_all::<(u32, u64)>()
                .await
                .with_context(|| format!("reading {target} hour histogram"))?;
            shards_from_histogram(&histogram, options.max_rows_per_shard)
        }
        ShardBy::IdPrefix => id_prefix_shards(options.id_prefix_len, source.source_rows),
    };

    let text_ratio = ops::column_compression_ratio(client, database, "events_local", "content")
        .await?
        .unwrap_or(TEXT_COMPRESSION_RATIO);
    let estimate = estimate_disk(target, &source, &shards, text_ratio);

    Ok(Plan {
        format_version: PLAN_FORMAT_VERSION,
        database: database.to_string(),
        target,
        shard_by: options.shard_by,
        require_projection: options.require_projection,
        view_created_at,
        planned_at,
        source,
        estimate,
        shards,
    })
}

/// Result of one completed shard, persisted as its checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShardCheckpoint {
    /// Shard index.
    pub index: usize,
    /// Shard range.
    pub range: ShardRange,
    /// ClickHouse query id of the INSERT.
    pub query_id: String,
    /// Client-observed duration.
    pub elapsed_ms: u64,
    /// `system.query_log` entry, when available.
    pub query_log: Option<ops::QueryLogEntry>,
    /// Server time when the shard finished.
    pub finished_at: u32,
}

/// Checkpoint directory layout for one target.
#[derive(Debug, Clone)]
pub struct CheckpointDir {
    root: PathBuf,
}

impl CheckpointDir {
    /// Layout rooted at `<base>/<target>`.
    pub fn new(base: &Path, target: Target) -> Self {
        Self {
            root: base.join(target.name()),
        }
    }

    /// Directory for this target.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Plan file path.
    pub fn plan_path(&self) -> PathBuf {
        self.root.join("plan.json")
    }

    fn shard_path(&self, index: usize) -> PathBuf {
        self.root.join("shards").join(format!("{index:06}.json"))
    }

    /// Gap-closure pass checkpoint path.
    pub fn gap_pass_path(&self) -> PathBuf {
        self.root.join("gap-pass.json")
    }

    /// Load the plan, if present.
    pub fn load_plan(&self) -> Result<Option<Plan>> {
        read_json(&self.plan_path())
    }

    /// Persist the plan. Refuses to replace a plan with completed shards
    /// unless `discard_progress` is set (which also removes them).
    pub fn save_plan(&self, plan: &Plan, discard_progress: bool) -> Result<()> {
        let done = self.completed_shards()?;
        if !done.is_empty() {
            if !discard_progress {
                bail!(
                    "{} already has {} completed shard(s); pass --discard-progress to replan",
                    self.root.display(),
                    done.len()
                );
            }
            std::fs::remove_dir_all(self.root.join("shards"))
                .with_context(|| format!("removing {}", self.root.display()))?;
        }
        write_json_atomic(&self.plan_path(), plan)
    }

    /// Record a completed shard.
    pub fn save_shard(&self, checkpoint: &ShardCheckpoint) -> Result<()> {
        write_json_atomic(&self.shard_path(checkpoint.index), checkpoint)
    }

    /// Record the gap-closure pass.
    pub fn save_gap_pass(&self, checkpoint: &ShardCheckpoint) -> Result<()> {
        write_json_atomic(&self.gap_pass_path(), checkpoint)
    }

    /// Completed shard checkpoints, ordered by index.
    pub fn completed_shards(&self) -> Result<Vec<ShardCheckpoint>> {
        let dir = self.root.join("shards");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut done = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && let Some(checkpoint) = read_json::<ShardCheckpoint>(&path)?
            {
                done.push(checkpoint);
            }
        }
        done.sort_by_key(|c| c.index);
        Ok(done)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("checkpoint path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Print a plan summary for operators.
pub fn describe_plan(plan: &Plan, min_free_bytes: u64) -> String {
    let e = &plan.estimate;
    let required = e.peak_bytes.saturating_add(min_free_bytes);
    format!(
        "target {target} ({table}) in {db}\n\
         \x20 source rows:          {source_rows}\n\
         \x20 distinct keys:        {keys}\n\
         \x20 target rows (merged): {output}\n\
         \x20 shards:               {shards} by {by:?} (projection required: {proj})\n\
         \x20 bytes per row (est):  {bpr}\n\
         \x20 final size (est):     {final_size}\n\
         \x20 pre-merge size (est): {pre} ({pre_rows} rows)\n\
         \x20 peak disk (est):      {peak} = pre-merge x2 for merge headroom\n\
         \x20 free space needed:    {required} including {reserve} reserve\n\
         \x20 view created at:      {t0}",
        target = plan.target,
        table = plan.target.table(),
        db = plan.database,
        source_rows = plan.source.source_rows,
        keys = plan.source.distinct_keys,
        output = plan.source.output_rows,
        shards = plan.shards.len(),
        by = plan.shard_by,
        proj = plan.require_projection,
        bpr = e.bytes_per_row,
        final_size = ops::human_bytes(e.final_bytes),
        pre = ops::human_bytes(e.pre_merge_bytes),
        pre_rows = e.pre_merge_rows,
        peak = ops::human_bytes(e.peak_bytes),
        required = ops::human_bytes(required),
        reserve = ops::human_bytes(min_free_bytes),
        t0 = plan.view_created_at,
    )
}

/// Default free-space reserve kept untouched during backfill.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 100 * GIB;

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(shards: &[Shard]) -> Vec<(Option<u32>, Option<u32>)> {
        shards
            .iter()
            .map(|s| match &s.range {
                ShardRange::CreatedAt { start, end } => (*start, *end),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn empty_histogram_is_one_unbounded_shard() {
        let shards = shards_from_histogram(&[], 10);
        assert_eq!(bounds(&shards), vec![(None, None)]);
        assert_eq!(shards[0].range.predicate(), "1");
    }

    #[test]
    fn histogram_shards_are_contiguous_and_open_ended() {
        let histogram = [(0, 3), (100, 4), (200, 5), (300, 1), (400, 9), (500, 2)];
        let shards = shards_from_histogram(&histogram, 8);
        let b = bounds(&shards);
        assert_eq!(b.first().unwrap().0, None);
        assert_eq!(b.last().unwrap().1, None);
        for pair in b.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "shards must be contiguous");
        }
        let total: u64 = shards.iter().map(|s| s.estimated_rows).sum();
        assert_eq!(total, 24);
        assert!(shards.iter().all(|s| s.estimated_rows <= 9));
        assert_eq!(
            b,
            vec![
                (None, Some(200)),
                (Some(200), Some(400)),
                (Some(400), Some(500)),
                (Some(500), None)
            ]
        );
        for (i, s) in shards.iter().enumerate() {
            assert_eq!(s.index, i);
        }
    }

    #[test]
    fn oversized_bucket_is_its_own_shard() {
        let shards = shards_from_histogram(&[(1, 2), (2, 50), (3, 2)], 10);
        assert_eq!(
            shards.iter().map(|s| s.estimated_rows).collect::<Vec<_>>(),
            vec![2, 50, 2]
        );
    }

    #[test]
    fn created_at_predicates() {
        let r = ShardRange::CreatedAt {
            start: Some(5),
            end: Some(9),
        };
        assert_eq!(
            r.predicate(),
            "created_at >= toDateTime(5) AND created_at < toDateTime(9)"
        );
        let r = ShardRange::CreatedAt {
            start: None,
            end: Some(9),
        };
        assert_eq!(r.predicate(), "created_at < toDateTime(9)");
        let r = ShardRange::IndexedAt { start: 1, end: 2 };
        assert_eq!(
            r.predicate(),
            "indexed_at >= toDateTime(1) AND indexed_at <= toDateTime(2)"
        );
    }

    #[test]
    fn id_prefix_shards_cover_keyspace() {
        let shards = id_prefix_shards(1, 160);
        assert_eq!(shards.len(), 16);
        assert_eq!(shards[0].range.predicate(), "id < '1'");
        assert_eq!(shards[10].range.predicate(), "id >= 'a' AND id < 'b'");
        assert_eq!(shards[15].range.predicate(), "id >= 'f'");
        assert!(shards.iter().all(|s| s.estimated_rows == 10));

        let shards = id_prefix_shards(2, 0);
        assert_eq!(shards.len(), 256);
        assert_eq!(shards[255].range.predicate(), "id >= 'ff'");
        for pair in shards.windows(2) {
            let (ShardRange::IdPrefix { end, .. }, ShardRange::IdPrefix { start, .. }) =
                (&pair[0].range, &pair[1].range)
            else {
                panic!("unexpected range");
            };
            assert_eq!(end, start);
        }
    }

    #[test]
    fn disk_estimate_counts_pre_merge_states_and_headroom() {
        let stats = SourceStats {
            source_rows: 1_000,
            distinct_keys: 100,
            output_rows: 100,
            avg_payload_bytes: 200.0,
        };
        let shards = shards_from_histogram(&[(1, 400), (2, 30), (3, 570)], 450);
        assert_eq!(shards.len(), 2);
        let e = estimate_disk(Target::Profiles, &stats, &shards, 0.5);
        // Each shard holds at most one state per key: 100 + 100.
        assert_eq!(e.pre_merge_rows, 200);
        assert_eq!(e.final_bytes, 100 * e.bytes_per_row);
        assert_eq!(e.pre_merge_bytes, 200 * e.bytes_per_row);
        assert_eq!(e.peak_bytes, 2 * e.pre_merge_bytes);

        let zap_stats = SourceStats {
            source_rows: 50,
            distinct_keys: 50,
            output_rows: 60,
            avg_payload_bytes: 0.0,
        };
        let e = estimate_disk(Target::Zaps, &zap_stats, &shards, 0.5);
        assert_eq!(e.pre_merge_rows, 60);
    }

    #[test]
    fn checkpoint_dir_round_trip_and_replan_guard() {
        let dir = tempfile::tempdir().unwrap();
        let cp = CheckpointDir::new(dir.path(), Target::Zaps);
        assert!(cp.load_plan().unwrap().is_none());
        assert!(cp.completed_shards().unwrap().is_empty());

        let plan = Plan {
            format_version: PLAN_FORMAT_VERSION,
            database: "db".into(),
            target: Target::Zaps,
            shard_by: ShardBy::CreatedAt,
            require_projection: true,
            view_created_at: 7,
            planned_at: 8,
            source: SourceStats::default(),
            estimate: DiskEstimate::default(),
            shards: shards_from_histogram(&[(1, 1), (2, 1)], 1),
        };
        cp.save_plan(&plan, false).unwrap();
        assert_eq!(cp.load_plan().unwrap().unwrap(), plan);

        let checkpoint = ShardCheckpoint {
            index: 1,
            range: plan.shards[1].range.clone(),
            query_id: "q".into(),
            elapsed_ms: 3,
            query_log: None,
            finished_at: 9,
        };
        cp.save_shard(&checkpoint).unwrap();
        assert_eq!(cp.completed_shards().unwrap(), vec![checkpoint]);

        assert!(cp.save_plan(&plan, false).is_err());
        cp.save_plan(&plan, true).unwrap();
        assert!(cp.completed_shards().unwrap().is_empty());
    }
}
