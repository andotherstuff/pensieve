//! Backfill plans, estimates and the on-disk checkpoint layout.
//!
//! `events_local` has no projections, so every plan shards the table by event
//! id prefix: shard `i` reads one primary-key range of the full table and
//! filters it. (The user-flags seed shards its two small lookup tables by
//! pubkey prefix instead.) Plans and checkpoints are JSON files under
//! `<checkpoint_dir>/<target>/`, which makes backfills resumable:
//!
//! - `plan.json`: the plan;
//! - `pending/<shard>.json`: a shard whose INSERT was started (with its query
//!   id) but whose outcome is not yet recorded;
//! - `shards/<shard>.json`: completed shards.
//!
//! For additive targets (publishers, kinds, kind activity) the plan also fixes
//! the view
//! boundary (see [`capture_boundary`]): the first second the materialized view
//! captured.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::ops::{self, GIB};
use super::{BACKFILL_SENTINEL_INDEXED_AT, PublisherBounds, SourceBounds, Target};

/// Plan file format version.
pub const PLAN_FORMAT_VERSION: u32 = 1;

/// Default free-space reserve kept untouched during backfill.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 100 * GIB;

/// Assumed compressed bytes per `publisher_activity` row before any rows
/// exist to measure (64-hex pubkey plus ~35 bytes of fixed-width columns at
/// ~0.6 compression).
const DEFAULT_PUBLISHER_ROW_BYTES: u64 = 60;

/// Assumed compressed bytes per `longform_latest` row (pubkey, d tag, the
/// winning id and fixed-width state).
const DEFAULT_LONGFORM_ROW_BYTES: u64 = 120;

/// Assumed compressed bytes per `pubkey_flags` row (64-hex pubkey, three
/// timestamps that are mostly constant).
const DEFAULT_USER_FLAGS_ROW_BYTES: u64 = 45;

/// Assumed compressed bytes per `relay_list_latest` row (pubkey, winning id
/// and a handful of relay URLs).
const DEFAULT_RELAY_LISTS_ROW_BYTES: u64 = 300;

/// Assumed compressed bytes per `kind_totals` row: dominated by the
/// `uniqCombined64` sketch (up to ~96 KiB dense, tiny for rare kinds).
const DEFAULT_KINDS_ROW_BYTES: u64 = 16 * 1024;

/// Assumed compressed bytes per pre-merge `kind_activity` row: a shard sees
/// 1/256 of each period's authors, so most sketches are small hash sets (a
/// few hundred bytes); merged day rows of large kinds grow to ~96 KiB.
const DEFAULT_KIND_ACTIVITY_ROW_BYTES: u64 = 512;

/// Assumed compressed bytes per pre-merge `hourly_pubkeys` row: a shard sees
/// 1/256 of each hour's authors, so nearly every sketch is a small hash set;
/// merged rows of busy hours grow to ~96 KiB.
const DEFAULT_HOURLY_PUBKEYS_ROW_BYTES: u64 = 256;

/// Seconds a shard pauses after it by default (`backfill --delay-secs`).
const DEFAULT_DELAY_SECS: u64 = 5;

/// UTC minutes per hour in which no shard starts by default (:58–:07).
const DEFAULT_QUIET_MINUTES: u64 = 10;

/// A contiguous `start <= key < end` range over a hex-keyed column; `None` is
/// unbounded. Bounds are lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRange {
    /// Inclusive lower bound.
    pub start: Option<String>,
    /// Exclusive upper bound.
    pub end: Option<String>,
}

impl IdRange {
    /// SQL predicate for this range over `id`. Bounds are validated hex, so
    /// the result is safe to embed.
    pub fn predicate(&self) -> String {
        self.predicate_on("id")
    }

    /// SQL predicate for this range over `column` (a trusted column name).
    pub fn predicate_on(&self, column: &str) -> String {
        let mut parts = Vec::new();
        if let Some(start) = &self.start {
            assert!(is_hex(start), "id prefix bound must be lowercase hex");
            parts.push(format!("{column} >= '{start}'"));
        }
        if let Some(end) = &self.end {
            assert!(is_hex(end), "id prefix bound must be lowercase hex");
            parts.push(format!("{column} < '{end}'"));
        }
        if parts.is_empty() {
            "1".to_string()
        } else {
            parts.join(" AND ")
        }
    }
}
fn is_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One unit of backfill work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    /// Position in the plan.
    pub index: usize,
    /// Source id range.
    pub range: IdRange,
    /// `events_local` rows expected in the range (all kinds).
    pub estimated_rows: u64,
}

/// Contiguous id-prefix shards over `16^prefix_len` buckets, with open-ended
/// first and last shards (so ids outside `[0-9a-f]*` are still covered).
pub fn id_prefix_shards(prefix_len: usize, total_rows: u64) -> Vec<Shard> {
    assert!((1..=4).contains(&prefix_len), "prefix length must be 1..=4");
    let buckets = 16usize.pow(prefix_len as u32);
    let per_shard = total_rows / buckets as u64;
    (0..buckets)
        .map(|index| {
            let bound = |i: usize| format!("{i:0width$x}", width = prefix_len);
            Shard {
                index,
                range: IdRange {
                    start: (index > 0).then(|| bound(index)),
                    end: (index + 1 < buckets).then(|| bound(index + 1)),
                },
                estimated_rows: per_shard,
            }
        })
        .collect()
}

/// Source statistics measured while planning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceStats {
    /// Rows in `events_local` (active parts, pre-merge duplicates included).
    pub table_rows: u64,
    /// Compressed bytes of the columns a backfill reads, across the table.
    pub read_compressed_bytes: u64,
    /// Uncompressed bytes of the columns a backfill reads.
    pub read_uncompressed_bytes: u64,
    /// `events_local` rows in the sample shard (shard 0).
    pub sample_rows: u64,
    /// Rows in the sample shard that can produce target rows (publishers:
    /// before the view boundary).
    pub sample_source_rows: u64,
    /// Target rows the sample shard's `SELECT` produced.
    pub sample_output_rows: u64,
    /// Final target rows, when known (longform: kind 30023 coordinates in
    /// `addressable_latest`), else 0.
    pub known_final_rows: u64,
    /// Client-observed duration of the sample shard's `SELECT`, in
    /// milliseconds (0 in plans made before it was recorded).
    #[serde(default)]
    pub sample_elapsed_ms: u64,
}

/// Disk estimate for a backfill.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskEstimate {
    /// Estimated compressed bytes per target row.
    pub bytes_per_row: u64,
    /// Rows written before merges: the sample shard's output times the
    /// number of shards.
    pub pre_merge_rows: u64,
    /// Estimated size written before merges.
    pub pre_merge_bytes: u64,
    /// Pre-merge bytes plus equal merge headroom (a merge writes its output
    /// while its inputs, then outdated parts, still exist).
    pub peak_bytes: u64,
}

/// Estimate disk use from the sample shard.
pub fn estimate_disk(stats: &SourceStats, shards: usize, bytes_per_row: u64) -> DiskEstimate {
    let pre_merge_rows = stats.sample_output_rows.saturating_mul(shards as u64);
    let pre_merge_bytes = pre_merge_rows.saturating_mul(bytes_per_row);
    DiskEstimate {
        bytes_per_row,
        pre_merge_rows,
        pre_merge_bytes,
        peak_bytes: pre_merge_bytes.saturating_mul(2),
    }
}

/// Estimated wall-clock seconds of a whole backfill at the plan's limits: the
/// sample shard's duration for every shard, the default 5 s pause after each,
/// and no shard starting in the default 10 quiet minutes of each hour.
pub fn estimate_runtime_secs(stats: &SourceStats, shards: usize) -> u64 {
    let work = (stats.sample_elapsed_ms / 1000 + DEFAULT_DELAY_SECS).saturating_mul(shards as u64);
    work.saturating_mul(60) / (60 - DEFAULT_QUIET_MINUTES)
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
    /// Creation time (Unix seconds) of the target's materialized view.
    pub view_created_at: u32,
    /// Server time when the plan was made.
    pub planned_at: u32,
    /// Publisher source bounds (view boundary and TTL window).
    pub publisher_bounds: Option<PublisherBounds>,
    /// Kinds: the view boundary (first second the view captured). Backfill
    /// shards read only rows indexed before it.
    #[serde(default)]
    pub view_boundary: Option<u32>,
    /// Hex prefix length of the id shards.
    pub id_prefix_len: usize,
    /// Source statistics at planning time.
    pub source: SourceStats,
    /// Disk estimate.
    pub estimate: DiskEstimate,
    /// Shards in execution order.
    pub shards: Vec<Shard>,
}

impl Plan {
    /// The source rows the shards read.
    pub fn source_bounds(&self) -> SourceBounds {
        SourceBounds {
            indexed_before: self
                .publisher_bounds
                .as_ref()
                .map(|b| b.indexed_before)
                .or(self.view_boundary),
            window_start: self.publisher_bounds.as_ref().map(|b| b.window_start),
        }
    }

    /// SQL predicate of a shard's range over the target's shard column.
    pub fn shard_predicate(&self, shard: &Shard) -> String {
        shard.range.predicate_on(self.target.shard_column())
    }
}

/// Options for [`build_plan`].
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Hex prefix length of the id shards (16^n shards).
    pub id_prefix_len: usize,
    /// Additive targets: the view's first captured second must be at least
    /// this many seconds after its creation time.
    pub boundary_margin_secs: u32,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            id_prefix_len: 2,
            boundary_margin_secs: 5,
        }
    }
}

/// The first second an additive target's view captured, made stable and
/// checked.
///
/// The view writes `live_min_indexed_at = min(indexed_at)`; the backfill
/// writes a sentinel. So `C = min(live_min_indexed_at)` over non-sentinel rows
/// is the earliest `indexed_at` of any row the view wrote. An `events_local`
/// insert gets `indexed_at = now()` at query start and is captured iff its
/// pipeline was built after the view was attached (within a second of `T0`,
/// the view's creation time). Therefore:
///
/// - every captured row has `indexed_at >= C` (definition of `C`, once no
///   insert that started before `C` is still running — checked here);
/// - every uncaptured row has `indexed_at <= T0 + 1`, so `C >= T0 + margin`
///   (margin >= 2) proves no uncaptured row has `indexed_at >= C`.
///
/// So the backfill reads exactly the rows with `indexed_at < C` and the view
/// owns the rest: no gap and no double count. (Publishers only see rows in
/// their TTL window, so `C` is the first in-window second; rows outside the
/// window are dropped by both paths.)
pub async fn capture_boundary(
    client: &Client,
    database: &str,
    target: Target,
    view_created_at: u32,
    margin_secs: u32,
) -> Result<u32> {
    let table = target.table();
    let view = target.materialized_view();
    let first = first_captured_second(client, database, table).await?;
    let Some(boundary) = first else {
        bail!(
            "{database}.{table} has no rows from the view yet; wait for the next ingest insert \
             and re-run plan"
        );
    };
    if boundary < view_created_at.saturating_add(margin_secs) {
        bail!(
            "{view}'s first captured insert was indexed at {boundary}, within {margin_secs}s of \
             the view's creation ({view_created_at}); an insert racing the view's creation could \
             be counted twice or not at all. Recreate the view: DROP VIEW {database}.{view}; \
             TRUNCATE TABLE {database}.{table}; re-apply {}, then plan again",
            target.migration_file()
        );
    }
    let running = ops::inserts_started_before(client, database, boundary).await?;
    if running > 0 {
        bail!(
            "{running} INSERT(s) into events_local started before the view boundary \
             ({boundary}) are still running; retry plan when they finish"
        );
    }
    let queued = ops::async_inserts_queued_before(client, database, boundary).await?;
    if queued > 0 {
        bail!(
            "{queued} async-insert buffer(s) for events_local predate the view boundary; \
             run SYSTEM FLUSH ASYNC INSERT QUEUE or wait, then retry plan"
        );
    }
    // Inserts that committed while we checked could only lower the minimum.
    let again = first_captured_second(client, database, table).await?;
    if again != Some(boundary) {
        bail!("the view boundary moved from {boundary} to {again:?} while planning; retry plan");
    }
    Ok(boundary)
}

/// Earliest `indexed_at` the view of an additive target (`table` has a
/// `live_min_indexed_at` column) wrote, or `None` if none yet.
pub async fn first_captured_second(
    client: &Client,
    database: &str,
    table: &str,
) -> Result<Option<u32>> {
    let (seconds, rows) = client
        .query(&format!(
            "SELECT toUInt32(min(live_min_indexed_at)), count() FROM {database}.{table} \
             WHERE live_min_indexed_at < toDateTime({BACKFILL_SENTINEL_INDEXED_AT})"
        ))
        .fetch_one::<(u32, u64)>()
        .await
        .with_context(|| format!("reading the view boundary of {table}"))?;
    Ok((rows > 0).then_some(seconds))
}

/// UTC calendar day of a Unix timestamp.
pub fn utc_day(unix_seconds: u32) -> NaiveDate {
    DateTime::from_timestamp(i64::from(unix_seconds), 0)
        .expect("u32 timestamps are in range")
        .date_naive()
}

/// Inspect the source and build a plan for `target`.
pub async fn build_plan(
    client: &Client,
    database: &str,
    target: Target,
    options: &PlanOptions,
) -> Result<Plan> {
    let view_created_at = ops::require_view(client, database, target).await?;
    let planned_at = ops::server_now(client).await?;

    let boundary = if target.is_additive() {
        Some(
            capture_boundary(
                client,
                database,
                target,
                view_created_at,
                options.boundary_margin_secs,
            )
            .await?,
        )
    } else {
        None
    };
    let (publisher_bounds, view_boundary) = match (target, boundary) {
        (Target::Publishers, Some(indexed_before)) => (
            Some(PublisherBounds {
                indexed_before,
                window_start: PublisherBounds::window_start_for(utc_day(planned_at)),
            }),
            None,
        ),
        (_, boundary) => (None, boundary),
    };
    let bounds = SourceBounds {
        indexed_before: boundary,
        window_start: publisher_bounds.as_ref().map(|b| b.window_start),
    };

    let mut table_rows = 0;
    let mut read = ops::ColumnBytes::default();
    for source in target.source_tables() {
        table_rows += ops::table_parts(client, database, source).await?.rows;
        let bytes = ops::column_bytes(client, database, source, target.source_columns()).await?;
        read.compressed += bytes.compressed;
        read.uncompressed += bytes.uncompressed;
    }
    let shards = id_prefix_shards(options.id_prefix_len, table_rows);

    // Measure shard 0 with the real backfill SELECT: one primary-key range.
    let sample = shards[0].range.predicate_on(target.shard_column());
    let (sample_rows, sample_source_rows) = client
        .query(&target.sample_count_sql(database, &sample, &bounds))
        .fetch_one::<(u64, u64)>()
        .await
        .with_context(|| format!("sizing the {target} sample shard"))?;
    let started = Instant::now();
    let sample_output_rows = client
        .query(&format!(
            "SELECT count() FROM ({})",
            target.backfill_select_sql(database, &sample, &bounds)
        ))
        .fetch_one::<u64>()
        .await
        .with_context(|| format!("running the {target} sample shard"))?;
    let sample_elapsed_ms = started.elapsed().as_millis() as u64;

    let known_final_rows = match target {
        Target::Longform => longform_coordinates(client, database).await?,
        _ => 0,
    };

    let bytes_per_row = measured_bytes_per_row(client, database, target).await?;
    let source = SourceStats {
        table_rows,
        read_compressed_bytes: read.compressed,
        read_uncompressed_bytes: read.uncompressed,
        sample_rows,
        sample_source_rows,
        sample_output_rows,
        known_final_rows,
        sample_elapsed_ms,
    };
    let estimate = estimate_disk(&source, shards.len(), bytes_per_row);

    Ok(Plan {
        format_version: PLAN_FORMAT_VERSION,
        database: database.to_string(),
        target,
        view_created_at,
        planned_at,
        publisher_bounds,
        view_boundary,
        id_prefix_len: options.id_prefix_len,
        source,
        estimate,
        shards,
    })
}

/// Kind 30023 coordinates in migration 017's `addressable_latest`, if that
/// table exists (0 otherwise).
async fn longform_coordinates(client: &Client, database: &str) -> Result<u64> {
    if ops::table_created_at(client, database, "addressable_latest")
        .await?
        .is_none()
    {
        return Ok(0);
    }
    client
        .query(&format!(
            "SELECT count() FROM (SELECT 1 FROM {database}.addressable_latest \
             WHERE kind = 30023 GROUP BY pubkey, d_tag)"
        ))
        .fetch_one::<u64>()
        .await
        .context("counting kind 30023 coordinates in addressable_latest")
}

/// Bytes per row of the target as written so far by its view, or a default.
async fn measured_bytes_per_row(client: &Client, database: &str, target: Target) -> Result<u64> {
    let parts = ops::table_parts(client, database, target.table()).await?;
    let default = match target {
        Target::Publishers => DEFAULT_PUBLISHER_ROW_BYTES,
        Target::Longform => DEFAULT_LONGFORM_ROW_BYTES,
        Target::UserFlags => DEFAULT_USER_FLAGS_ROW_BYTES,
        Target::RelayLists => DEFAULT_RELAY_LISTS_ROW_BYTES,
        // Few rows, each a sketch: the view's rows say little about a shard's.
        Target::Kinds => return Ok(DEFAULT_KINDS_ROW_BYTES),
        // Per-insert sketches say little about a shard's rows either.
        Target::KindActivity => return Ok(DEFAULT_KIND_ACTIVITY_ROW_BYTES),
        Target::HourlyPubkeys => return Ok(DEFAULT_HOURLY_PUBKEYS_ROW_BYTES),
    };
    // Tiny parts are dominated by per-part overhead; only trust real volume.
    if parts.rows >= 100_000 {
        Ok((parts.bytes_on_disk / parts.rows).max(1))
    } else {
        Ok(default)
    }
}
/// Result of one completed shard, persisted as its checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShardCheckpoint {
    /// Shard index.
    pub index: usize,
    /// Shard range.
    pub range: IdRange,
    /// ClickHouse query id of the INSERT.
    pub query_id: String,
    /// Client-observed duration (0 when recovered from a pending marker).
    pub elapsed_ms: u64,
    /// `system.query_log` entry, when available.
    pub query_log: Option<ops::QueryLogEntry>,
    /// Server time when the shard was recorded.
    pub finished_at: u32,
}

/// A shard whose INSERT was started but whose outcome is not yet recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingShard {
    /// Shard index.
    pub index: usize,
    /// Shard range.
    pub range: IdRange,
    /// ClickHouse query id of the INSERT.
    pub query_id: String,
    /// Server time just before the INSERT was sent.
    pub started_at: u32,
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

    /// Path of a shard's pending marker.
    pub fn pending_path(&self, index: usize) -> PathBuf {
        self.root.join("pending").join(format!("{index:06}.json"))
    }

    /// Load the plan, if present.
    pub fn load_plan(&self) -> Result<Option<Plan>> {
        read_json(&self.plan_path())
    }

    /// Persist the plan. Refuses to replace a plan with completed or pending
    /// shards unless `discard_progress` is set (which also removes them).
    pub fn save_plan(&self, plan: &Plan, discard_progress: bool) -> Result<()> {
        let done = self.completed_shards()?.len();
        let pending = self.pending_shards()?.len();
        if done + pending > 0 {
            if !discard_progress {
                bail!(
                    "{} already has {done} completed and {pending} pending shard(s); pass \
                     --discard-progress to replan (for additive targets: publishers, kinds, \
                     kind-activity, only after truncating the table and recreating its view)",
                    self.root.display()
                );
            }
            for dir in ["shards", "pending"] {
                let path = self.root.join(dir);
                if path.exists() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                }
            }
        }
        write_json_atomic(&self.plan_path(), plan)
    }

    /// Record a completed shard and clear its pending marker.
    pub fn save_shard(&self, checkpoint: &ShardCheckpoint) -> Result<()> {
        write_json_atomic(&self.shard_path(checkpoint.index), checkpoint)?;
        self.clear_pending(checkpoint.index)
    }

    /// Record that a shard's INSERT is about to start.
    pub fn save_pending(&self, pending: &PendingShard) -> Result<()> {
        write_json_atomic(&self.pending_path(pending.index), pending)
    }

    /// Remove a shard's pending marker (no-op if absent).
    pub fn clear_pending(&self, index: usize) -> Result<()> {
        let path = self.pending_path(index);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Completed shard checkpoints, ordered by index.
    pub fn completed_shards(&self) -> Result<Vec<ShardCheckpoint>> {
        read_dir_json(&self.root.join("shards"), |c: &ShardCheckpoint| c.index)
    }

    /// Pending shard markers, ordered by index.
    pub fn pending_shards(&self) -> Result<Vec<PendingShard>> {
        read_dir_json(&self.root.join("pending"), |p: &PendingShard| p.index)
    }
}

fn read_dir_json<T: serde::de::DeserializeOwned>(
    dir: &Path,
    index: impl Fn(&T) -> usize,
) -> Result<Vec<T>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut items = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json")
            && let Some(item) = read_json::<T>(&path)?
        {
            items.push(item);
        }
    }
    items.sort_by_key(|item| index(item));
    Ok(items)
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
    // Durable before the INSERT it guards (pending) or the next shard (done).
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("syncing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Print a plan summary for operators.
pub fn describe_plan(plan: &Plan, min_free_bytes: u64) -> String {
    let e = &plan.estimate;
    let s = &plan.source;
    let shards = plan.shards.len().max(1) as u64;
    let required = e.peak_bytes.saturating_add(min_free_bytes);
    let bounds = match (&plan.publisher_bounds, plan.view_boundary, plan.target) {
        (Some(b), _, _) => format!(
            "  view boundary:        indexed_at < {} (view created at {})\n\
             \x20 activity window:      day >= {} (TTL window at planning)\n",
            b.indexed_before, plan.view_created_at, b.window_start
        ),
        (None, Some(c), Target::KindActivity) => format!(
            "  view boundary:        indexed_at < {c} (view created at {})\n\
             \x20 hour rows:            hours newer than the boundary - {} days\n",
            plan.view_created_at,
            super::kind_activity::HOUR_ROWS_DAYS
        ),
        (None, Some(c), _) => format!(
            "  view boundary:        indexed_at < {c} (view created at {})\n",
            plan.view_created_at
        ),
        (None, None, Target::UserFlags) => "  source bounds:        every pubkey of \
             pubkeys_with_profile_data / pubkeys_with_follows_data (idempotent min)\n"
            .to_string(),
        (None, None, Target::RelayLists) => {
            "  source bounds:        all kind 10002 rows (idempotent argMin)\n".to_string()
        }
        (None, None, Target::HourlyPubkeys) => format!(
            "  source bounds:        rows whose created_at hour is newer than now() - {} days \
             at each shard (idempotent uniqCombined64 sketches)\n",
            super::kind_activity::HOUR_ROWS_DAYS
        ),
        (None, None, _) => {
            "  source bounds:        all kind 30023 rows (idempotent argMin)\n".to_string()
        }
    };
    let final_rows = if s.known_final_rows > 0 {
        format!("{} (from addressable_latest)", s.known_final_rows)
    } else {
        "unknown; at most the pre-merge rows".to_string()
    };
    let sources = plan.target.source_tables().join(" + ");
    let runtime = estimate_runtime_secs(s, plan.shards.len());
    format!(
        "target {target} ({table}) in {db}\n\
         {bounds}\
         \x20 shards:               {n} by {column} prefix of length {len}\n\
         \x20 source rows:          {table_rows} in {sources} (~{per_shard} per shard)\n\
         \x20 source read (est):    {read_c} compressed, {read_u} uncompressed in total \
         (~{shard_c} per shard; every shard reads its {column} range of the full table)\n\
         \x20 sample shard 0:       {sample_rows} rows, {sample_source} usable, {sample_out} target \
         rows in {sample_s:.1} s\n\
         \x20 runtime (est):        ~{hours:.1} h wall-clock = {n} x (sample shard + {delay} s pause), \
         no starts in {quiet} quiet minutes per hour\n\
         \x20 target rows (merged): {final_rows}\n\
         \x20 bytes per row (est):  {bpr}\n\
         \x20 pre-merge size (est): {pre} ({pre_rows} rows)\n\
         \x20 peak disk (est):      {peak} = pre-merge x2 for merge headroom\n\
         \x20 free space needed:    {required} including {reserve} reserve",
        target = plan.target,
        table = plan.target.table(),
        db = plan.database,
        n = plan.shards.len(),
        column = plan.target.shard_column(),
        len = plan.id_prefix_len,
        table_rows = s.table_rows,
        per_shard = s.table_rows / shards,
        read_c = ops::human_bytes(s.read_compressed_bytes),
        read_u = ops::human_bytes(s.read_uncompressed_bytes),
        shard_c = ops::human_bytes(s.read_compressed_bytes / shards),
        sample_rows = s.sample_rows,
        sample_source = s.sample_source_rows,
        sample_out = s.sample_output_rows,
        sample_s = s.sample_elapsed_ms as f64 / 1000.0,
        hours = runtime as f64 / 3600.0,
        delay = DEFAULT_DELAY_SECS,
        quiet = DEFAULT_QUIET_MINUTES,
        bpr = e.bytes_per_row,
        pre = ops::human_bytes(e.pre_merge_bytes),
        pre_rows = e.pre_merge_rows,
        peak = ops::human_bytes(e.peak_bytes),
        required = ops::human_bytes(required),
        reserve = ops::human_bytes(min_free_bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_prefix_shards_cover_keyspace() {
        let shards = id_prefix_shards(1, 160);
        assert_eq!(shards.len(), 16);
        assert_eq!(shards[0].range.predicate(), "id < '1'");
        assert_eq!(shards[10].range.predicate(), "id >= 'a' AND id < 'b'");
        assert_eq!(shards[15].range.predicate(), "id >= 'f'");
        assert!(shards.iter().all(|s| s.estimated_rows == 10));

        let shards = id_prefix_shards(3, 0);
        assert_eq!(shards.len(), 4096);
        assert_eq!(shards[0].range.predicate(), "id < '001'");
        assert_eq!(shards[4095].range.predicate(), "id >= 'fff'");
        for pair in shards.windows(2) {
            assert_eq!(pair[0].range.end, pair[1].range.start);
        }
        assert_eq!(
            IdRange {
                start: None,
                end: None
            }
            .predicate(),
            "1"
        );
    }

    #[test]
    fn estimate_scales_the_sample_shard() {
        let stats = SourceStats {
            sample_output_rows: 1_000,
            ..SourceStats::default()
        };
        let e = estimate_disk(&stats, 4096, 60);
        assert_eq!(e.pre_merge_rows, 4_096_000);
        assert_eq!(e.pre_merge_bytes, 4_096_000 * 60);
        assert_eq!(e.peak_bytes, 2 * e.pre_merge_bytes);

        // 256 shards of 25 s plus 5 s pauses, started in 50 of 60 minutes.
        let stats = SourceStats {
            sample_elapsed_ms: 25_000,
            ..SourceStats::default()
        };
        assert_eq!(estimate_runtime_secs(&stats, 256), 256 * 30 * 60 / 50);
    }

    #[test]
    fn plans_without_new_fields_still_load_and_bounds_follow_the_target() {
        let mut plan = plan();
        let mut json = serde_json::to_value(&plan).unwrap();
        json.as_object_mut().unwrap().remove("view_boundary");
        json["source"]
            .as_object_mut()
            .unwrap()
            .remove("sample_elapsed_ms");
        let loaded: Plan = serde_json::from_value(json).unwrap();
        assert_eq!(loaded, plan);
        assert_eq!(
            plan.source_bounds(),
            SourceBounds {
                indexed_before: Some(20),
                window_start: NaiveDate::from_ymd_opt(2025, 9, 8),
            }
        );
        plan.target = Target::Kinds;
        plan.publisher_bounds = None;
        plan.view_boundary = Some(30);
        assert_eq!(plan.source_bounds().indexed_before, Some(30));
        assert_eq!(
            plan.shard_predicate(&plan.shards[1]),
            "id >= '1' AND id < '2'"
        );
        plan.target = Target::UserFlags;
        assert_eq!(plan.shard_predicate(&plan.shards[15]), "pubkey >= 'f'");
        assert!(describe_plan(&plan, 0).contains("by pubkey prefix"));
        plan.target = Target::KindActivity;
        assert!(describe_plan(&plan, 0).contains("hour rows:            hours newer"));
        plan.target = Target::HourlyPubkeys;
        plan.view_boundary = None;
        assert_eq!(plan.source_bounds(), SourceBounds::default());
        assert!(describe_plan(&plan, 0).contains("idempotent uniqCombined64 sketches"));
    }

    #[test]
    fn utc_day_of_timestamp() {
        assert_eq!(utc_day(0), NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
        assert_eq!(
            utc_day(1_791_791_999),
            NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()
        );
    }

    fn plan() -> Plan {
        Plan {
            format_version: PLAN_FORMAT_VERSION,
            database: "db".into(),
            target: Target::Publishers,
            view_created_at: 7,
            planned_at: 8,
            publisher_bounds: Some(PublisherBounds {
                indexed_before: 20,
                window_start: NaiveDate::from_ymd_opt(2025, 9, 8).unwrap(),
            }),
            view_boundary: None,
            id_prefix_len: 1,
            source: SourceStats::default(),
            estimate: DiskEstimate::default(),
            shards: id_prefix_shards(1, 16),
        }
    }

    #[test]
    fn checkpoint_dir_round_trip_pending_and_replan_guard() {
        let dir = tempfile::tempdir().unwrap();
        let cp = CheckpointDir::new(dir.path(), Target::Publishers);
        assert!(cp.load_plan().unwrap().is_none());
        assert!(cp.completed_shards().unwrap().is_empty());

        let plan = plan();
        cp.save_plan(&plan, false).unwrap();
        assert_eq!(cp.load_plan().unwrap().unwrap(), plan);

        let pending = PendingShard {
            index: 2,
            range: plan.shards[2].range.clone(),
            query_id: "q2".into(),
            started_at: 9,
        };
        cp.save_pending(&pending).unwrap();
        assert_eq!(cp.pending_shards().unwrap(), vec![pending.clone()]);
        // A pending shard alone blocks a replan.
        assert!(cp.save_plan(&plan, false).is_err());

        let checkpoint = ShardCheckpoint {
            index: 2,
            range: pending.range.clone(),
            query_id: "q2".into(),
            elapsed_ms: 3,
            query_log: None,
            finished_at: 10,
        };
        cp.save_shard(&checkpoint).unwrap();
        assert!(cp.pending_shards().unwrap().is_empty());
        assert_eq!(cp.completed_shards().unwrap(), vec![checkpoint]);

        assert!(cp.save_plan(&plan, false).is_err());
        cp.save_pending(&pending).unwrap();
        cp.save_plan(&plan, true).unwrap();
        assert!(cp.completed_shards().unwrap().is_empty());
        assert!(cp.pending_shards().unwrap().is_empty());
    }
}
