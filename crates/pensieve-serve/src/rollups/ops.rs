//! Shared ClickHouse plumbing for the `api-rollups` operator tool:
//! connection and resource-limit arguments, `system.*` probes and query-log
//! lookups. Mirrors `pensieve_preview::lookups::ops` (migration 017's tool)
//! so the two tools behave the same way; it is duplicated rather than shared
//! because the API must not depend on the preview crate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

/// Bytes per GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// ClickHouse connection arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct ConnectionArgs {
    /// ClickHouse HTTP URL.
    #[arg(long, env = "CLICKHOUSE_URL", default_value = "http://localhost:8123")]
    pub clickhouse_url: String,

    /// ClickHouse database holding events_local and the rollup tables.
    #[arg(long, env = "CLICKHOUSE_DATABASE", default_value = "nostr")]
    pub clickhouse_database: String,

    /// ClickHouse user.
    #[arg(long, env = "CLICKHOUSE_USER")]
    pub clickhouse_user: Option<String>,

    /// ClickHouse password.
    #[arg(long, env = "CLICKHOUSE_PASSWORD", hide_env_values = true)]
    pub clickhouse_password: Option<String>,
}

impl ConnectionArgs {
    /// Build a client for the configured database.
    pub fn client(&self) -> Client {
        let mut client = Client::default()
            .with_url(&self.clickhouse_url)
            .with_database(&self.clickhouse_database);
        if let Some(user) = self.clickhouse_user.as_deref() {
            client = client.with_user(user);
        }
        if let Some(password) = self.clickhouse_password.as_deref() {
            client = client.with_password(password);
        }
        client
    }
}

/// Per-query server-side resource limits for operator queries.
#[derive(Debug, Clone, clap::Args)]
pub struct LimitArgs {
    /// `max_threads` for each query.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=32))]
    pub max_threads: u32,

    /// `max_memory_usage` for each query, in GiB. Aggregation spills to disk
    /// at half of this.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=256))]
    pub max_memory_gib: u64,

    /// `max_execution_time` for each query, in seconds.
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub max_execution_secs: u64,
}

impl LimitArgs {
    /// Apply the limits (and safe insert settings) to a client.
    pub fn apply(&self, client: Client) -> Client {
        let memory = self.max_memory_gib * GIB;
        client
            .with_option("max_threads", self.max_threads.to_string())
            .with_option("max_memory_usage", memory.to_string())
            .with_option(
                "max_bytes_before_external_group_by",
                (memory / 2).to_string(),
            )
            .with_option("max_bytes_before_external_sort", (memory / 2).to_string())
            .with_option("max_execution_time", self.max_execution_secs.to_string())
            .with_option("timeout_overflow_mode", "throw")
            .with_option("max_insert_threads", "1")
            .with_option("async_insert", "0")
    }
}

/// Settings that make an `INSERT ... SELECT` write exactly one block, so a
/// shard into an unpartitioned table is one part committed atomically.
pub fn single_part_insert(client: &Client) -> Client {
    let unbounded = u64::MAX.to_string();
    client
        .clone()
        .with_option("max_insert_threads", "1")
        .with_option("min_insert_block_size_rows", unbounded.as_str())
        .with_option("min_insert_block_size_bytes", unbounded.as_str())
        // A retried shard must never be dropped as a duplicate block.
        .with_option("insert_deduplicate", "0")
}

/// A unique query id for correlating with `system.query_log`.
pub fn new_query_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos}-{n}")
}

/// Server clock as a Unix timestamp.
pub async fn server_now(client: &Client) -> Result<u32> {
    client
        .query("SELECT toUInt32(now())")
        .fetch_one::<u32>()
        .await
        .context("reading server clock")
}

/// Creation time of a table or view (`metadata_modification_time`), or
/// `None` when it does not exist.
pub async fn table_created_at(client: &Client, database: &str, name: &str) -> Result<Option<u32>> {
    client
        .query(
            "SELECT toUInt32(metadata_modification_time) FROM system.tables \
             WHERE database = ? AND name = ?",
        )
        .bind(database)
        .bind(name)
        .fetch_optional::<u32>()
        .await
        .with_context(|| format!("reading creation time of {database}.{name}"))
}

/// Fail unless the target's materialized view exists; returns its creation
/// time.
pub async fn require_view(client: &Client, database: &str, target: super::Target) -> Result<u32> {
    let view = target.materialized_view();
    match table_created_at(client, database, view).await? {
        Some(t0) => Ok(t0),
        None => bail!(
            "{database}.{view} does not exist; apply {} first",
            target.migration_file()
        ),
    }
}

/// Flush system logs; returns whether the flush ran. Ignored when the user
/// lacks the privilege; callers then poll for the periodic flush.
pub async fn flush_logs(client: &Client) -> bool {
    match client.query("SYSTEM FLUSH LOGS").execute().await {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %e, "SYSTEM FLUSH LOGS failed; waiting for periodic flush");
            false
        }
    }
}

/// Selected `system.query_log` fields for a finished query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
pub struct QueryLogEntry {
    /// `type` (`QueryFinish` or an exception type).
    pub kind: String,
    /// Server-side duration.
    pub query_duration_ms: u64,
    /// Rows read.
    pub read_rows: u64,
    /// Bytes read.
    pub read_bytes: u64,
    /// Rows written (INSERT).
    pub written_rows: u64,
    /// Peak memory.
    pub memory_usage: i64,
}

impl QueryLogEntry {
    /// Whether the query finished successfully.
    pub fn finished(&self) -> bool {
        self.kind == "QueryFinish"
    }
}

/// Fetch the final `system.query_log` entry for `query_id`. When logs can be
/// flushed on demand, the answer is immediate; otherwise poll for up to
/// `timeout` for the periodic flush. Returns `None` if it never appears.
pub async fn query_log_entry(
    client: &Client,
    query_id: &str,
    timeout: Duration,
) -> Result<Option<QueryLogEntry>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let flushed = flush_logs(client).await;
        let entry = client
            .query(
                "SELECT toString(type) AS kind, query_duration_ms, read_rows, read_bytes, \
                 written_rows, memory_usage \
                 FROM system.query_log \
                 WHERE event_date >= today() - 7 AND query_id = ? AND type != 'QueryStart' \
                 ORDER BY event_time_microseconds DESC LIMIT 1",
            )
            .bind(query_id)
            .fetch_optional::<QueryLogEntry>()
            .await
            .context("reading system.query_log")?;
        if entry.is_some() || flushed || tokio::time::Instant::now() >= deadline {
            return Ok(entry);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Whether a query with this id is still running.
pub async fn query_running(client: &Client, query_id: &str) -> Result<bool> {
    let n = client
        .query("SELECT count() FROM system.processes WHERE query_id = ?")
        .bind(query_id)
        .fetch_one::<u64>()
        .await
        .context("reading system.processes")?;
    Ok(n > 0)
}

/// Parts a query created according to `system.part_log`, or `None` when the
/// server has no part log.
pub async fn parts_written_by(
    client: &Client,
    database: &str,
    table: &str,
    query_id: &str,
) -> Result<Option<u64>> {
    let has_part_log = client
        .query("SELECT count() FROM system.tables WHERE database = 'system' AND name = 'part_log'")
        .fetch_one::<u64>()
        .await
        .context("checking for system.part_log")?
        > 0;
    if !has_part_log {
        return Ok(None);
    }
    flush_logs(client).await;
    let parts = client
        .query(
            "SELECT count() FROM system.part_log \
             WHERE event_date >= today() - 7 AND event_type = 'NewPart' \
             AND database = ? AND table = ? AND query_id = ?",
        )
        .bind(database)
        .bind(table)
        .bind(query_id)
        .fetch_one::<u64>()
        .await
        .context("reading system.part_log")?;
    Ok(Some(parts))
}

/// Space on the disk that stores a table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
pub struct DiskSpace {
    /// Disk name.
    pub name: String,
    /// Free bytes.
    pub free_space: u64,
    /// Free bytes not reserved by running merges/inserts.
    pub unreserved_space: u64,
    /// Total bytes.
    pub total_space: u64,
}

/// Space on the disk holding `database.table` (falls back to `default`).
pub async fn table_disk(client: &Client, database: &str, table: &str) -> Result<DiskSpace> {
    client
        .query(
            "SELECT name, free_space, unreserved_space, total_space FROM system.disks \
             WHERE name = ifNull(nullIf(( \
                 SELECT any(disk_name) FROM system.parts \
                 WHERE database = ? AND table = ? AND active \
             ), ''), 'default') \
             LIMIT 1",
        )
        .bind(database)
        .bind(table)
        .fetch_optional::<DiskSpace>()
        .await
        .context("reading system.disks")?
        .context("no disk found in system.disks")
}

/// Active-part statistics for a table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
pub struct TableParts {
    /// Active parts.
    pub parts: u64,
    /// Rows across active parts (pre-merge duplicates included).
    pub rows: u64,
    /// Bytes on disk across active parts.
    pub bytes_on_disk: u64,
    /// Uncompressed data bytes.
    pub data_uncompressed_bytes: u64,
}

/// Active-part statistics for `database.table`.
pub async fn table_parts(client: &Client, database: &str, table: &str) -> Result<TableParts> {
    client
        .query(
            "SELECT count() AS parts, sum(rows) AS rows, sum(bytes_on_disk) AS bytes_on_disk, \
             sum(data_uncompressed_bytes) AS data_uncompressed_bytes \
             FROM system.parts WHERE database = ? AND table = ? AND active",
        )
        .bind(database)
        .bind(table)
        .fetch_one::<TableParts>()
        .await
        .with_context(|| format!("reading system.parts for {database}.{table}"))
}

/// Merges currently running on a table.
pub async fn running_merges(client: &Client, database: &str, table: &str) -> Result<u64> {
    client
        .query("SELECT count() FROM system.merges WHERE database = ? AND table = ?")
        .bind(database)
        .bind(table)
        .fetch_one::<u64>()
        .await
        .with_context(|| format!("reading system.merges for {database}.{table}"))
}

/// Compressed and uncompressed bytes of some columns of a table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
pub struct ColumnBytes {
    /// Compressed bytes on disk.
    pub compressed: u64,
    /// Uncompressed bytes.
    pub uncompressed: u64,
}

/// Total compressed/uncompressed size of `columns` of `database.table`.
pub async fn column_bytes(
    client: &Client,
    database: &str,
    table: &str,
    columns: &[&str],
) -> Result<ColumnBytes> {
    client
        .query(
            "SELECT sum(data_compressed_bytes) AS compressed, \
             sum(data_uncompressed_bytes) AS uncompressed \
             FROM system.columns WHERE database = ? AND table = ? AND has(?, name)",
        )
        .bind(database)
        .bind(table)
        .bind(columns)
        .fetch_one::<ColumnBytes>()
        .await
        .context("reading system.columns")
}

/// Inserts into `events_local` that started before `before` (Unix seconds)
/// and are still running.
pub async fn inserts_started_before(client: &Client, database: &str, before: u32) -> Result<u64> {
    let now = server_now(client).await?;
    let started_before_secs = now.saturating_sub(before);
    client
        .query(
            "SELECT count() FROM system.processes \
             WHERE query_kind = 'Insert' AND positionCaseInsensitive(query, 'events_local') > 0 \
             AND (current_database = ? OR positionCaseInsensitive(query, concat(?, '.')) > 0) \
             AND elapsed >= ?",
        )
        .bind(database)
        .bind(database)
        .bind(started_before_secs as f64)
        .fetch_one::<u64>()
        .await
        .context("reading system.processes")
}

/// Pending async-insert buffers for `events_local` first written before
/// `before` (Unix seconds).
pub async fn async_inserts_queued_before(
    client: &Client,
    database: &str,
    before: u32,
) -> Result<u64> {
    client
        .query(
            "SELECT count() FROM system.asynchronous_inserts \
             WHERE database = ? AND table = 'events_local' \
             AND toUInt32(first_update) < ?",
        )
        .bind(database)
        .bind(before)
        .fetch_one::<u64>()
        .await
        .context("reading system.asynchronous_inserts")
}

/// Human-readable byte size.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(GIB * 3 / 2), "1.50 GiB");
    }

    #[test]
    fn query_ids_are_unique() {
        let a = new_query_id("x");
        let b = new_query_id("x");
        assert_ne!(a, b);
        assert!(a.starts_with("x-"));
    }
}
