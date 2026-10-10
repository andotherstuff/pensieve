//! Backfill progress, table sizes and freshness for the compact tables.

use std::path::Path;

use anyhow::Result;
use clickhouse::Client;
use serde::Serialize;

use super::ops::{self, TableParts};
use super::plan::CheckpointDir;
use super::target::Target;

/// Status of one compact table.
#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    /// Target.
    pub target: Target,
    /// Whether the materialized view exists.
    pub view_exists: bool,
    /// Plan shards (0 when no plan).
    pub shards_total: usize,
    /// Completed shards.
    pub shards_done: usize,
    /// Whether the gap-closure pass has run.
    pub gap_pass_done: bool,
    /// Active-part statistics.
    pub parts: TableParts,
    /// Seconds since the newest `indexed_at` captured in the table.
    pub freshness_lag_secs: Option<u64>,
}

/// Collect status for every target.
pub async fn collect(
    client: &Client,
    database: &str,
    checkpoint_dir: &Path,
) -> Result<Vec<TargetStatus>> {
    let now = ops::server_now(client).await?;
    let mut statuses = Vec::new();
    for target in Target::ALL {
        let view_exists = ops::view_created_at(client, database, target.materialized_view())
            .await?
            .is_some();
        let checkpoints = CheckpointDir::new(checkpoint_dir, target);
        let shards_total = checkpoints.load_plan()?.map_or(0, |p| p.shards.len());
        let shards_done = checkpoints.completed_shards()?.len();
        let gap_pass_done = checkpoints.gap_pass_path().exists();

        let (parts, freshness_lag_secs) = if view_exists {
            let parts = ops::table_parts(client, database, target.table()).await?;
            let column = if target == Target::Zaps {
                "indexed_at"
            } else {
                "last_indexed_at"
            };
            let newest = client
                .query(&format!(
                    "SELECT toUInt32(max({column})) FROM {database}.{}",
                    target.table()
                ))
                .fetch_one::<u32>()
                .await?;
            let lag = (newest > 0).then(|| u64::from(now.saturating_sub(newest)));
            (parts, lag)
        } else {
            (TableParts::default(), None)
        };

        statuses.push(TargetStatus {
            target,
            view_exists,
            shards_total,
            shards_done,
            gap_pass_done,
            parts,
            freshness_lag_secs,
        });
    }
    Ok(statuses)
}

/// Render statuses as a text table.
pub fn render(statuses: &[TargetStatus]) -> String {
    let mut out = String::from(
        "target       view  shards        gap  parts  rows          on disk       lag\n",
    );
    for s in statuses {
        out.push_str(&format!(
            "{:<12} {:<5} {:<13} {:<4} {:<6} {:<13} {:<13} {}\n",
            s.target.name(),
            if s.view_exists { "yes" } else { "no" },
            format!("{}/{}", s.shards_done, s.shards_total),
            if s.gap_pass_done { "yes" } else { "no" },
            s.parts.parts,
            s.parts.rows,
            ops::human_bytes(s.parts.bytes_on_disk),
            s.freshness_lag_secs
                .map_or_else(|| "-".to_string(), |lag| format!("{lag}s")),
        ));
    }
    out
}
