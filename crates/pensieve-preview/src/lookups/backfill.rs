//! Bounded, resumable, idempotent backfill of a compact lookup table.
//!
//! # Why concurrent ingestion cannot leave gaps or double counts
//!
//! - The target's materialized view (created at `T0`) captures every
//!   `events_local` insert whose pipeline is built after `T0`, including
//!   async-insert flushes.
//! - Before the first shard runs, [`gap_preflight`] requires `now >= T0 +
//!   grace`, no INSERT into `events_local` older than `T0` still running, and
//!   no async-insert buffer first written before `T0`. So every row whose
//!   insert might have missed the view is already committed and visible to
//!   every shard, which all start later.
//! - Overlap (a row seen by both the view and a shard, or by a re-run shard)
//!   is harmless: profile and naddr states are `argMin` (idempotent), and zap
//!   rows are collapsed per receipt id on read.
//! - The optional gap-closure pass re-reads rows with `indexed_at` around
//!   `T0` as belt and braces.
//!
//! # Resource bounds
//!
//! Each shard is one `INSERT ... SELECT` with `max_threads`,
//! `max_memory_usage`, external aggregation and `max_execution_time` set by
//! the caller's client. Before each shard the free space on the table's disk
//! must cover the remaining estimate plus a reserve, and the target must not
//! have too many unmerged parts. `max_runtime` and `max_shards` stop between
//! shards; completed shards are checkpointed so a re-run resumes.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clickhouse::Client;
use serde::Serialize;

use super::ops::{self, QueryLogEntry};
use super::plan::{CheckpointDir, Plan, ShardCheckpoint, ShardRange, projection_client};

/// How long to wait for a shard's `system.query_log` entry.
const QUERY_LOG_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting for target merges to catch up.
const PARTS_POLL: Duration = Duration::from_secs(10);

/// Backfill options.
#[derive(Debug, Clone)]
pub struct BackfillOptions {
    /// Stop (resumably) once this much time has elapsed.
    pub max_runtime: Option<Duration>,
    /// Stop (resumably) after running this many shards.
    pub max_shards: Option<usize>,
    /// Pause between shards to leave room for live traffic.
    pub delay_between_shards: Duration,
    /// Minimum time after the view's creation before the first shard.
    pub grace: Duration,
    /// Skip the gap preflight. Only for tests that control all inserts.
    pub skip_gap_preflight: bool,
    /// Free space that must remain on the table's disk.
    pub min_free_bytes: u64,
    /// Wait for merges while the target has more active parts than this.
    pub max_target_parts: u64,
    /// Give up (resumably) after waiting this long for merges.
    pub parts_wait_timeout: Duration,
    /// Abort a projection shard that reads more than this multiple of its
    /// planned rows (plus one million rows of slack): a sign that some parts
    /// lack the projection and were scanned in full.
    pub max_read_amplification: f64,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            max_runtime: None,
            max_shards: None,
            delay_between_shards: Duration::from_secs(5),
            grace: Duration::from_secs(600),
            skip_gap_preflight: false,
            min_free_bytes: super::plan::DEFAULT_MIN_FREE_BYTES,
            max_target_parts: 300,
            parts_wait_timeout: Duration::from_secs(1800),
            max_read_amplification: 4.0,
        }
    }
}

/// Why a backfill run stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub enum StopReason {
    /// Every shard is complete.
    Complete,
    /// `max_runtime` elapsed.
    MaxRuntime,
    /// `max_shards` shards ran.
    MaxShards,
    /// The disk guard refused the next shard.
    DiskGuard(String),
    /// Target merges did not catch up in time.
    PartsBacklog(String),
}

/// Summary of one backfill run.
#[derive(Debug, Clone, Serialize)]
pub struct BackfillReport {
    /// Shards in the plan.
    pub shards_total: usize,
    /// Shards complete after this run.
    pub shards_done: usize,
    /// Shards executed by this run.
    pub shards_run: usize,
    /// Rows written by this run (from `system.query_log`, when available).
    pub rows_written: u64,
    /// Why the run stopped.
    pub stopped: StopReason,
}

/// Verify that no `events_local` insert can have missed the view.
pub async fn gap_preflight(client: &Client, plan: &Plan, grace: Duration) -> Result<()> {
    let view = plan.target.materialized_view();
    let t0 = ops::require_view(client, &plan.database, view).await?;
    if t0 != plan.view_created_at {
        bail!(
            "{}.{view} was (re)created at {t0}, after the plan ({}); re-run `plan`",
            plan.database,
            plan.view_created_at
        );
    }

    let now = ops::server_now(client).await?;
    let ready_at = t0.saturating_add(grace.as_secs() as u32);
    if now < ready_at {
        bail!(
            "{view} was created {}s ago; wait until {}s after creation before backfilling",
            now.saturating_sub(t0),
            grace.as_secs()
        );
    }

    let running = ops::inserts_started_before(client, &plan.database, t0).await?;
    if running > 0 {
        bail!(
            "{running} INSERT(s) into events_local started before {view} existed and are still running; retry later"
        );
    }

    let queued = ops::async_inserts_queued_before(client, &plan.database, t0).await?;
    if queued > 0 {
        bail!(
            "{queued} async-insert buffer(s) for events_local predate {view}; run \
             SYSTEM FLUSH ASYNC INSERT QUEUE or wait, then retry"
        );
    }
    Ok(())
}

/// Execute the remaining shards of `plan`.
///
/// `client` should already carry resource limits ([`ops::LimitArgs::apply`]).
pub async fn run_backfill(
    client: &Client,
    plan: &Plan,
    checkpoints: &CheckpointDir,
    options: &BackfillOptions,
) -> Result<BackfillReport> {
    if !options.skip_gap_preflight {
        gap_preflight(client, plan, options.grace).await?;
    }

    let started = Instant::now();
    let done: std::collections::BTreeSet<usize> = checkpoints
        .completed_shards()?
        .into_iter()
        .map(|c| c.index)
        .collect();
    let total = plan.shards.len();
    let mut shards_done = plan
        .shards
        .iter()
        .filter(|s| done.contains(&s.index))
        .count();
    let mut shards_run = 0usize;
    let mut rows_written = 0u64;
    let insert_client = projection_client(client, plan.require_projection);

    tracing::info!(
        target = %plan.target,
        shards_total = total,
        shards_done,
        "starting backfill"
    );

    for shard in &plan.shards {
        if done.contains(&shard.index) {
            continue;
        }

        if let Some(max) = options.max_runtime
            && started.elapsed() >= max
        {
            return Ok(report(
                total,
                shards_done,
                shards_run,
                rows_written,
                StopReason::MaxRuntime,
            ));
        }
        if let Some(max) = options.max_shards
            && shards_run >= max
        {
            return Ok(report(
                total,
                shards_done,
                shards_run,
                rows_written,
                StopReason::MaxShards,
            ));
        }

        let remaining_rows: u64 = plan
            .shards
            .iter()
            .filter(|s| s.index >= shard.index && !done.contains(&s.index))
            .map(|s| s.estimated_rows)
            .sum();
        if let Some(refusal) = disk_guard(client, plan, remaining_rows, options).await? {
            return Ok(report(
                total,
                shards_done,
                shards_run,
                rows_written,
                StopReason::DiskGuard(refusal),
            ));
        }
        if let Some(backlog) = wait_for_merges(client, plan, options).await? {
            return Ok(report(
                total,
                shards_done,
                shards_run,
                rows_written,
                StopReason::PartsBacklog(backlog),
            ));
        }

        let checkpoint = run_shard(
            &insert_client,
            client,
            plan,
            shard.index,
            &shard.range,
            shard.estimated_rows,
            options,
        )
        .await?;
        let written = checkpoint.query_log.as_ref().map_or(0, |q| q.written_rows);
        rows_written += written;
        checkpoints.save_shard(&checkpoint)?;
        shards_done += 1;
        shards_run += 1;

        let elapsed = started.elapsed();
        let eta = if shards_run > 0 && shards_done < total {
            let per_shard = elapsed / shards_run as u32;
            format!("{:?}", per_shard * (total - shards_done) as u32)
        } else {
            "-".to_string()
        };
        tracing::info!(
            target = %plan.target,
            shard = shard.index,
            progress = format!("{shards_done}/{total}"),
            percent = format!("{:.1}", shards_done as f64 * 100.0 / total.max(1) as f64),
            read_rows = checkpoint.query_log.as_ref().map_or(0, |q| q.read_rows),
            written_rows = written,
            shard_ms = checkpoint.elapsed_ms,
            elapsed = ?elapsed,
            eta = %eta,
            "shard complete"
        );

        if shards_done < total && !options.delay_between_shards.is_zero() {
            tokio::time::sleep(options.delay_between_shards).await;
        }
    }

    Ok(report(
        total,
        shards_done,
        shards_run,
        rows_written,
        StopReason::Complete,
    ))
}

/// Re-insert target-kind rows whose `indexed_at` lies within `margin` of the
/// view's creation time. Idempotent; reads the whole target-kind slice.
pub async fn run_gap_pass(
    client: &Client,
    plan: &Plan,
    checkpoints: &CheckpointDir,
    margin: Duration,
    options: &BackfillOptions,
) -> Result<ShardCheckpoint> {
    let margin = margin.as_secs() as u32;
    let range = ShardRange::IndexedAt {
        start: plan.view_created_at.saturating_sub(margin),
        end: plan.view_created_at.saturating_add(margin),
    };
    let insert_client = projection_client(client, plan.require_projection);
    // Read amplification is expected here (indexed_at is not a key column).
    let options = BackfillOptions {
        max_read_amplification: f64::INFINITY,
        ..options.clone()
    };
    let checkpoint = run_shard(
        &insert_client,
        client,
        plan,
        usize::MAX,
        &range,
        0,
        &options,
    )
    .await?;
    checkpoints.save_gap_pass(&checkpoint)?;
    tracing::info!(
        target = %plan.target,
        written_rows = checkpoint.query_log.as_ref().map_or(0, |q| q.written_rows),
        "gap-closure pass complete"
    );
    Ok(checkpoint)
}

fn report(
    shards_total: usize,
    shards_done: usize,
    shards_run: usize,
    rows_written: u64,
    stopped: StopReason,
) -> BackfillReport {
    BackfillReport {
        shards_total,
        shards_done,
        shards_run,
        rows_written,
        stopped,
    }
}

async fn run_shard(
    insert_client: &Client,
    log_client: &Client,
    plan: &Plan,
    index: usize,
    range: &ShardRange,
    estimated_rows: u64,
    options: &BackfillOptions,
) -> Result<ShardCheckpoint> {
    let query_id = ops::new_query_id(&format!("preview-lookups-backfill-{}", plan.target));
    let sql = plan.target.backfill_sql(&plan.database, &range.predicate());
    let started = Instant::now();

    insert_client
        .query(&sql)
        .with_option("query_id", query_id.as_str())
        .with_option(
            "log_comment",
            format!("preview-lookups:backfill:{}:{index}", plan.target),
        )
        .execute()
        .await
        .with_context(|| format!("{} shard {index} ({})", plan.target, range.predicate()))?;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let query_log = ops::query_log_entry(log_client, &query_id, QUERY_LOG_TIMEOUT).await?;
    if let Some(entry) = &query_log {
        check_projection_use(plan, index, estimated_rows, entry, options)?;
    } else {
        tracing::warn!(
            query_id,
            "no system.query_log entry; projection use not verified"
        );
    }

    Ok(ShardCheckpoint {
        index,
        range: range.clone(),
        query_id,
        elapsed_ms,
        query_log,
        finished_at: ops::server_now(log_client).await?,
    })
}

fn check_projection_use(
    plan: &Plan,
    index: usize,
    estimated_rows: u64,
    entry: &QueryLogEntry,
    options: &BackfillOptions,
) -> Result<()> {
    if !plan.require_projection {
        return Ok(());
    }
    let projection = format!("{}.events_local.events_by_kind", plan.database);
    if !entry.projections.iter().any(|p| p == &projection) {
        bail!(
            "{} shard {index} did not use {projection} (used {:?}); stopping before \
             full-table scans. Rows it wrote are harmless.",
            plan.target,
            entry.projections
        );
    }
    let limit = estimated_rows as f64 * options.max_read_amplification + 1_000_000.0;
    if entry.read_rows as f64 > limit {
        bail!(
            "{} shard {index} read {} rows for ~{estimated_rows} planned; some parts may \
             lack the projection. Investigate system.projection_parts before continuing.",
            plan.target,
            entry.read_rows
        );
    }
    Ok(())
}

async fn disk_guard(
    client: &Client,
    plan: &Plan,
    remaining_rows: u64,
    options: &BackfillOptions,
) -> Result<Option<String>> {
    let disk = ops::table_disk(client, &plan.database, plan.target.table()).await?;
    let e = &plan.estimate;
    let remaining_fraction = if plan.source.source_rows == 0 {
        1.0
    } else {
        (remaining_rows as f64 / plan.source.source_rows as f64).min(1.0)
    };
    let needed = (e.peak_bytes as f64 * remaining_fraction) as u64 + options.min_free_bytes;
    if disk.unreserved_space < needed {
        return Ok(Some(format!(
            "disk {} has {} unreserved; remaining backfill needs ~{} plus {} reserve",
            disk.name,
            ops::human_bytes(disk.unreserved_space),
            ops::human_bytes(needed - options.min_free_bytes),
            ops::human_bytes(options.min_free_bytes)
        )));
    }
    Ok(None)
}

async fn wait_for_merges(
    client: &Client,
    plan: &Plan,
    options: &BackfillOptions,
) -> Result<Option<String>> {
    let deadline = Instant::now() + options.parts_wait_timeout;
    loop {
        let parts = ops::table_parts(client, &plan.database, plan.target.table()).await?;
        if parts.parts <= options.max_target_parts {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(Some(format!(
                "{} still has {} active parts (limit {})",
                plan.target.table(),
                parts.parts,
                options.max_target_parts
            )));
        }
        tracing::info!(
            table = plan.target.table(),
            parts = parts.parts,
            limit = options.max_target_parts,
            "waiting for merges"
        );
        tokio::time::sleep(PARTS_POLL).await;
    }
}
