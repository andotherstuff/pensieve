//! Bounded, resumable backfill of the API rollup tables.
//!
//! # No gaps, no double counts
//!
//! - **Longform** (`argMin`, idempotent) follows migration 017: once the view
//!   exists (`T0`), the grace period has passed and no insert that started
//!   before `T0` is still running, every row the view missed is committed and
//!   visible to every shard. Overlap with the view, or a re-run shard, is
//!   harmless.
//! - **Publishers** (sums, additive) split the source by `indexed_at` at the
//!   view boundary `C` fixed by the plan ([`super::plan::capture_boundary`]):
//!   shards read only `indexed_at < C`, the view owns everything else. Before
//!   the first shard the boundary is re-checked: no view row may lie below
//!   `C` and no insert started before `C` may still be running.
//! - **Exactly once.** Each shard is a single `INSERT ... SELECT` squashed into
//!   one block, so it writes one part into the unpartitioned table and commits
//!   atomically. A pending marker with the query id is written before the
//!   INSERT is sent. If the outcome is lost (crash, network error), the next
//!   run resolves it from `system.query_log` (and `system.part_log`): a
//!   finished query is recorded as done, a failed one is retried. An
//!   unresolvable additive shard stops the run instead of risking a double
//!   count.
//!
//! # Resource bounds
//!
//! Every query carries the caller's `max_threads`/`max_memory_usage`/
//! `max_execution_time`. Before each shard: no start inside the quiet minutes
//! (by default :58–:07 UTC, when refreshable views rescan `events_local`);
//! the table's disk must hold the remaining estimate plus a reserve; and the
//! target must not have a merge backlog. `max_runtime` is an absolute session
//! deadline re-checked right before every INSERT; `max_shards` and
//! `delay_between_shards` further bound a session.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clickhouse::Client;
use serde::Serialize;

use super::PUBLISHER_TABLE;
use super::ops;
use super::plan::{
    CheckpointDir, PendingShard, Plan, Shard, ShardCheckpoint, first_captured_second,
};

/// How long to wait for a shard's `system.query_log` entry when logs cannot
/// be flushed on demand.
const QUERY_LOG_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to keep looking for the log entry of a shard whose outcome is
/// unknown (its entry may be queued just after the client saw an error).
const PENDING_LOG_PATIENCE: Duration = Duration::from_secs(10);

/// Poll interval while waiting for merges or the quiet window.
const POLL: Duration = Duration::from_secs(10);

/// Minutes of the hour (UTC, inclusive, may wrap past :59) in which no shard
/// starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietMinutes {
    /// First quiet minute.
    pub start: u8,
    /// Last quiet minute.
    pub end: u8,
}

impl QuietMinutes {
    /// Whether `minute` (0–59) is quiet.
    pub fn contains(self, minute: u8) -> bool {
        if self.start <= self.end {
            (self.start..=self.end).contains(&minute)
        } else {
            minute >= self.start || minute <= self.end
        }
    }

    /// Seconds from `unix_seconds` until the window ends (0 if outside it).
    pub fn wait_secs(self, unix_seconds: u32) -> u32 {
        let minute = ((unix_seconds / 60) % 60) as u8;
        if !self.contains(minute) {
            return 0;
        }
        let into_hour = unix_seconds % 3600;
        let end = (u32::from(self.end) + 1) * 60;
        if end > into_hour {
            end - into_hour
        } else {
            3600 - into_hour + end
        }
    }
}

impl FromStr for QuietMinutes {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (start, end) = value
            .split_once('-')
            .ok_or_else(|| format!("expected START-END minutes, got {value:?}"))?;
        let parse = |s: &str| -> Result<u8, String> {
            let minute: u8 = s
                .trim()
                .parse()
                .map_err(|_| format!("invalid minute {s:?}"))?;
            if minute > 59 {
                return Err(format!("minute {minute} is out of range 0-59"));
            }
            Ok(minute)
        };
        Ok(Self {
            start: parse(start)?,
            end: parse(end)?,
        })
    }
}

impl fmt::Display for QuietMinutes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ":{:02}-:{:02}", self.start, self.end)
    }
}

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
    /// Free space that must remain on the table's disk.
    pub min_free_bytes: u64,
    /// Wait for merges while the target has more active parts than this.
    pub max_target_parts: u64,
    /// Give up (resumably) after waiting this long for merges.
    pub parts_wait_timeout: Duration,
    /// Do not start shards in these minutes of the hour (UTC).
    pub quiet_minutes: Option<QuietMinutes>,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            max_runtime: None,
            max_shards: None,
            delay_between_shards: Duration::from_secs(5),
            grace: Duration::from_secs(600),
            min_free_bytes: super::plan::DEFAULT_MIN_FREE_BYTES,
            max_target_parts: 300,
            parts_wait_timeout: Duration::from_secs(1800),
            quiet_minutes: Some(QuietMinutes { start: 58, end: 7 }),
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
    /// Pending shards resolved at start (as committed or as not committed).
    pub pending_resolved: usize,
    /// Rows written by this run (from `system.query_log`, when available).
    pub rows_written: u64,
    /// Why the run stopped.
    pub stopped: StopReason,
}

/// Verify that the backfill cannot leave a gap or double count.
pub async fn preflight(client: &Client, plan: &Plan, grace: Duration) -> Result<()> {
    let view = plan.target.materialized_view();
    let t0 = ops::require_view(client, &plan.database, view).await?;
    if t0 != plan.view_created_at {
        bail!(
            "{}.{view} was (re)created at {t0}, after the plan ({}); re-run `plan` \
             (for publishers, truncate the table first)",
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

    // Rows the view did not capture must be committed before shards read.
    let before = match &plan.publisher_bounds {
        Some(bounds) => bounds.indexed_before.max(t0),
        None => t0,
    };
    let running = ops::inserts_started_before(client, &plan.database, before).await?;
    if running > 0 {
        bail!(
            "{running} INSERT(s) into events_local started before {before} and are still \
             running; retry later"
        );
    }
    let queued = ops::async_inserts_queued_before(client, &plan.database, before).await?;
    if queued > 0 {
        bail!(
            "{queued} async-insert buffer(s) for events_local predate {before}; run \
             SYSTEM FLUSH ASYNC INSERT QUEUE or wait, then retry"
        );
    }

    if let Some(bounds) = &plan.publisher_bounds {
        // No view row may lie below the boundary (TTL can only raise the
        // minimum, so `>=` rather than `==`).
        let first = first_captured_second(client, &plan.database).await?;
        if let Some(first) = first
            && first < bounds.indexed_before
        {
            bail!(
                "{}.{PUBLISHER_TABLE} has view rows indexed at {first}, before the planned \
                 boundary {}; the table is not the one planned. Truncate it, recreate the \
                 view and re-plan",
                plan.database,
                bounds.indexed_before
            );
        }
    }
    Ok(())
}

/// Outcome of a pending shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingOutcome {
    /// The INSERT committed; record it as done.
    Committed(Option<ops::QueryLogEntry>),
    /// The INSERT did not commit; the shard can run again.
    NotCommitted,
}

/// Decide whether a pending shard's INSERT committed.
pub async fn resolve_pending(
    client: &Client,
    plan: &Plan,
    pending: &PendingShard,
) -> Result<PendingOutcome> {
    if ops::query_running(client, &pending.query_id).await? {
        bail!(
            "{} shard {} (query id {}) is still running; retry when it finishes",
            plan.target,
            pending.index,
            pending.query_id
        );
    }
    let entry = settled_query_log_entry(client, &pending.query_id).await?;
    if let Some(entry) = entry {
        if entry.finished() {
            return Ok(PendingOutcome::Committed(Some(entry)));
        }
        // A shard is one part, so a failed query almost never left data; the
        // part log settles it when present.
        let parts = ops::parts_written_by(
            client,
            &plan.database,
            plan.target.table(),
            &pending.query_id,
        )
        .await?;
        return Ok(match parts {
            Some(n) if n > 0 => PendingOutcome::Committed(Some(entry)),
            _ => PendingOutcome::NotCommitted,
        });
    }
    if !plan.target.is_additive() {
        // Re-running an idempotent shard is harmless.
        return Ok(PendingOutcome::NotCommitted);
    }
    match ops::parts_written_by(
        client,
        &plan.database,
        plan.target.table(),
        &pending.query_id,
    )
    .await?
    {
        Some(n) if n > 0 => Ok(PendingOutcome::Committed(None)),
        _ => bail!(
            "cannot tell whether {} shard {} (query id {}) committed: it is not in \
             system.query_log and system.part_log has no part for it. Check by hand; if \
             it committed, run `api-rollups resolve --target {} --shard {} --committed`, \
             otherwise `... --not-committed`, then re-run the backfill",
            plan.target,
            pending.index,
            pending.query_id,
            plan.target,
            pending.index
        ),
    }
}

/// The query's log entry, looking again for [`PENDING_LOG_PATIENCE`] if it is
/// not there yet.
async fn settled_query_log_entry(
    client: &Client,
    query_id: &str,
) -> Result<Option<ops::QueryLogEntry>> {
    let deadline = Instant::now() + PENDING_LOG_PATIENCE;
    loop {
        let entry = ops::query_log_entry(client, query_id, QUERY_LOG_TIMEOUT).await?;
        if entry.is_some() || Instant::now() >= deadline {
            return Ok(entry);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
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
    preflight(client, plan, options.grace).await?;

    let mut pending_resolved = 0;
    for pending in checkpoints.pending_shards()? {
        match resolve_pending(client, plan, &pending).await? {
            PendingOutcome::Committed(entry) => {
                tracing::warn!(target = %plan.target, shard = pending.index, "pending shard had committed; recording it");
                checkpoints.save_shard(&ShardCheckpoint {
                    index: pending.index,
                    range: pending.range.clone(),
                    query_id: pending.query_id.clone(),
                    elapsed_ms: 0,
                    query_log: entry,
                    finished_at: ops::server_now(client).await?,
                })?;
            }
            PendingOutcome::NotCommitted => {
                tracing::warn!(target = %plan.target, shard = pending.index, "pending shard had not committed; it will run again");
                checkpoints.clear_pending(pending.index)?;
            }
        }
        pending_resolved += 1;
    }

    let started = Instant::now();
    let deadline = options.max_runtime.map(|max| Instant::now() + max);
    let done: BTreeSet<usize> = checkpoints
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
    let report = |shards_done, shards_run, rows_written, stopped| BackfillReport {
        shards_total: total,
        shards_done,
        shards_run,
        pending_resolved,
        rows_written,
        stopped,
    };

    tracing::info!(target = %plan.target, shards_total = total, shards_done, "starting backfill");

    for shard in &plan.shards {
        if done.contains(&shard.index) {
            continue;
        }
        if expired(deadline) {
            return Ok(report(
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
                shards_done,
                shards_run,
                rows_written,
                StopReason::MaxShards,
            ));
        }
        if !wait_for_quiet_window(client, options, deadline).await? {
            return Ok(report(
                shards_done,
                shards_run,
                rows_written,
                StopReason::MaxRuntime,
            ));
        }
        let remaining_shards = plan
            .shards
            .iter()
            .filter(|s| s.index >= shard.index && !done.contains(&s.index))
            .count();
        if let Some(refusal) = disk_guard(client, plan, remaining_shards, options).await? {
            return Ok(report(
                shards_done,
                shards_run,
                rows_written,
                StopReason::DiskGuard(refusal),
            ));
        }
        if let Some(stop) = wait_for_merges(client, plan, options, deadline).await? {
            return Ok(report(shards_done, shards_run, rows_written, stop));
        }
        // Guards may have taken a while; never start an INSERT past the deadline.
        if expired(deadline) {
            return Ok(report(
                shards_done,
                shards_run,
                rows_written,
                StopReason::MaxRuntime,
            ));
        }

        let checkpoint = run_shard(client, plan, checkpoints, shard).await?;
        let written = checkpoint.query_log.as_ref().map_or(0, |q| q.written_rows);
        rows_written += written;
        shards_done += 1;
        shards_run += 1;

        let elapsed = started.elapsed();
        let eta = if shards_done < total {
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
        shards_done,
        shards_run,
        rows_written,
        StopReason::Complete,
    ))
}

async fn run_shard(
    client: &Client,
    plan: &Plan,
    checkpoints: &CheckpointDir,
    shard: &Shard,
) -> Result<ShardCheckpoint> {
    let query_id = ops::new_query_id(&format!("api-rollups-backfill-{}", plan.target));
    let sql = plan.target.backfill_sql(
        &plan.database,
        &shard.range.predicate(),
        plan.publisher_bounds.as_ref(),
    );
    let pending = PendingShard {
        index: shard.index,
        range: shard.range.clone(),
        query_id: query_id.clone(),
        started_at: ops::server_now(client).await?,
    };
    checkpoints.save_pending(&pending)?;

    let started = Instant::now();
    let result = ops::single_part_insert(client)
        .query(&sql)
        .with_option("query_id", query_id.as_str())
        .with_option(
            "log_comment",
            format!("api-rollups:backfill:{}:{}", plan.target, shard.index),
        )
        .execute()
        .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let query_log = match result {
        Ok(()) => ops::query_log_entry(client, &query_id, QUERY_LOG_TIMEOUT).await?,
        Err(error) => {
            let error = anyhow::Error::from(error).context(format!(
                "{} shard {} ({query_id})",
                plan.target, shard.index
            ));
            // The response may be lost after a commit; settle it now.
            match resolve_pending(client, plan, &pending).await {
                Ok(PendingOutcome::Committed(entry)) => {
                    tracing::warn!(error = %format!("{error:#}"), "shard reported an error but committed");
                    entry
                }
                Ok(PendingOutcome::NotCommitted) => {
                    checkpoints.clear_pending(shard.index)?;
                    return Err(error);
                }
                Err(unresolved) => return Err(error.context(format!("{unresolved:#}"))),
            }
        }
    };
    if query_log.is_none() {
        tracing::warn!(target = %plan.target, shard = shard.index, "no system.query_log entry for shard");
    }

    let checkpoint = ShardCheckpoint {
        index: shard.index,
        range: shard.range.clone(),
        query_id,
        elapsed_ms,
        query_log,
        finished_at: ops::server_now(client).await?,
    };
    checkpoints.save_shard(&checkpoint)?;
    Ok(checkpoint)
}

async fn disk_guard(
    client: &Client,
    plan: &Plan,
    remaining_shards: usize,
    options: &BackfillOptions,
) -> Result<Option<String>> {
    let disk = ops::table_disk(client, &plan.database, plan.target.table()).await?;
    let fraction = remaining_shards as f64 / plan.shards.len().max(1) as f64;
    let needed = ((plan.estimate.peak_bytes as f64 * fraction) as u64)
        .saturating_add(options.min_free_bytes);
    if disk.unreserved_space < needed {
        return Ok(Some(format!(
            "disk {} has {} unreserved; remaining backfill needs ~{} plus {} reserve",
            disk.name,
            ops::human_bytes(disk.unreserved_space),
            ops::human_bytes(needed.saturating_sub(options.min_free_bytes)),
            ops::human_bytes(options.min_free_bytes)
        )));
    }
    Ok(None)
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Sleep through the quiet minutes. Returns `false` if the session deadline
/// passes first.
async fn wait_for_quiet_window(
    client: &Client,
    options: &BackfillOptions,
    deadline: Option<Instant>,
) -> Result<bool> {
    let Some(quiet) = options.quiet_minutes else {
        return Ok(true);
    };
    loop {
        let wait = quiet.wait_secs(ops::server_now(client).await?);
        if wait == 0 {
            return Ok(true);
        }
        if expired(deadline) {
            return Ok(false);
        }
        tracing::info!(quiet = %quiet, wait_secs = wait, "waiting for the quiet minutes to pass");
        let sleep = Duration::from_secs(u64::from(wait)).min(POLL * 6);
        let sleep = deadline.map_or(sleep, |d| {
            sleep.min(d.saturating_duration_since(Instant::now()))
        });
        tokio::time::sleep(sleep).await;
    }
}

/// Wait while the target has too many active parts. Stops with
/// `PartsBacklog` after `parts_wait_timeout`, or `MaxRuntime` when the
/// session deadline passes first.
async fn wait_for_merges(
    client: &Client,
    plan: &Plan,
    options: &BackfillOptions,
    session: Option<Instant>,
) -> Result<Option<StopReason>> {
    let parts_deadline = Instant::now() + options.parts_wait_timeout;
    loop {
        let parts = ops::table_parts(client, &plan.database, plan.target.table()).await?;
        if parts.parts <= options.max_target_parts {
            return Ok(None);
        }
        if expired(session) {
            return Ok(Some(StopReason::MaxRuntime));
        }
        let now = Instant::now();
        if now >= parts_deadline {
            return Ok(Some(StopReason::PartsBacklog(format!(
                "{} still has {} active parts (limit {})",
                plan.target.table(),
                parts.parts,
                options.max_target_parts
            ))));
        }
        let merges = ops::running_merges(client, &plan.database, plan.target.table()).await?;
        tracing::info!(
            table = plan.target.table(),
            parts = parts.parts,
            running_merges = merges,
            limit = options.max_target_parts,
            "waiting for merges"
        );
        let wake = session.map_or(parts_deadline, |d| d.min(parts_deadline));
        tokio::time::sleep(POLL.min(wake.saturating_duration_since(now))).await;
    }
}

/// Settle a pending shard by hand after checking `system.part_log` or the
/// table yourself: record it as done (`committed`) or drop the marker so the
/// shard runs again.
pub fn resolve_by_hand(
    checkpoints: &CheckpointDir,
    index: usize,
    committed: bool,
    now: u32,
) -> Result<()> {
    let pending = checkpoints
        .pending_shards()?
        .into_iter()
        .find(|p| p.index == index)
        .with_context(|| format!("shard {index} has no pending marker"))?;
    if committed {
        checkpoints.save_shard(&ShardCheckpoint {
            index,
            range: pending.range,
            query_id: pending.query_id,
            elapsed_ms: 0,
            query_log: None,
            finished_at: now,
        })
    } else {
        checkpoints.clear_pending(index)
    }
}

/// Whether `target`'s plan exists and every shard is done.
pub fn is_complete(checkpoints: &CheckpointDir) -> Result<bool> {
    let Some(plan) = checkpoints.load_plan()? else {
        return Ok(false);
    };
    let done: BTreeSet<usize> = checkpoints
        .completed_shards()?
        .into_iter()
        .map(|c| c.index)
        .collect();
    Ok(plan.shards.iter().all(|s| done.contains(&s.index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_minutes_parse_and_wrap() {
        let q: QuietMinutes = "58-7".parse().unwrap();
        assert_eq!(q, QuietMinutes { start: 58, end: 7 });
        assert!(q.contains(58) && q.contains(59) && q.contains(0) && q.contains(7));
        assert!(!q.contains(8) && !q.contains(30) && !q.contains(57));
        assert_eq!(q.to_string(), ":58-:07");

        let plain: QuietMinutes = "0-7".parse().unwrap();
        assert!(plain.contains(0) && plain.contains(7) && !plain.contains(59));

        assert!("7".parse::<QuietMinutes>().is_err());
        assert!("0-60".parse::<QuietMinutes>().is_err());
        assert!("a-3".parse::<QuietMinutes>().is_err());
    }

    #[test]
    fn quiet_minutes_wait_until_the_window_ends() {
        let q = QuietMinutes { start: 58, end: 7 };
        let hour = 1_800_000_000 / 3600 * 3600;
        // :30 is outside.
        assert_eq!(q.wait_secs(hour + 30 * 60), 0);
        // :00:00 waits until :08:00.
        assert_eq!(q.wait_secs(hour), 8 * 60);
        // :58:30 waits 90 s to the hour plus 8 minutes.
        assert_eq!(q.wait_secs(hour + 58 * 60 + 30), 90 + 8 * 60);
        // :07:59 waits one second.
        assert_eq!(q.wait_secs(hour + 7 * 60 + 59), 1);
        let plain = QuietMinutes { start: 0, end: 7 };
        assert_eq!(plain.wait_secs(hour + 5 * 60), 3 * 60);
    }
}
