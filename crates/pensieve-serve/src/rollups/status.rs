//! Backfill progress, table sizes and freshness for the API rollup tables.

use std::path::Path;

use anyhow::Result;
use clickhouse::Client;
use serde::Serialize;

use super::Target;
use super::ops::{self, TableParts};
use super::plan::{CheckpointDir, first_captured_second};

/// Status of one rollup table.
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
    /// Shards started but not yet resolved.
    pub shards_pending: usize,
    /// Publishers: the planned view boundary, and the earliest second the view
    /// wrote now (must never be below the boundary).
    pub boundary: Option<(u32, Option<u32>)>,
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
        let view_exists = ops::table_created_at(client, database, target.materialized_view())
            .await?
            .is_some();
        let table_exists = ops::table_created_at(client, database, target.table())
            .await?
            .is_some();
        let checkpoints = CheckpointDir::new(checkpoint_dir, target);
        let plan = checkpoints.load_plan()?;
        let shards_total = plan.as_ref().map_or(0, |p| p.shards.len());
        let shards_done = checkpoints.completed_shards()?.len();
        let shards_pending = checkpoints.pending_shards()?.len();

        let (parts, freshness_lag_secs) = if table_exists {
            let parts = ops::table_parts(client, database, target.table()).await?;
            let newest = client
                .query(&format!(
                    "SELECT toUInt32(max(last_indexed_at)) FROM {database}.{}",
                    target.table()
                ))
                .fetch_one::<u32>()
                .await?;
            let lag = (newest > 0).then(|| u64::from(now.saturating_sub(newest)));
            (parts, lag)
        } else {
            (TableParts::default(), None)
        };

        let boundary = match plan.as_ref().and_then(|p| p.publisher_bounds.as_ref()) {
            Some(bounds) if table_exists => Some((
                bounds.indexed_before,
                first_captured_second(client, database).await?,
            )),
            _ => None,
        };

        statuses.push(TargetStatus {
            target,
            view_exists,
            shards_total,
            shards_done,
            shards_pending,
            boundary,
            parts,
            freshness_lag_secs,
        });
    }
    Ok(statuses)
}

/// Render statuses as a text table.
pub fn render(statuses: &[TargetStatus]) -> String {
    let mut out = String::from(
        "target       view  shards        pending  parts  rows          on disk       lag\n",
    );
    for s in statuses {
        out.push_str(&format!(
            "{:<12} {:<5} {:<13} {:<8} {:<6} {:<13} {:<13} {}\n",
            s.target.name(),
            if s.view_exists { "yes" } else { "no" },
            format!("{}/{}", s.shards_done, s.shards_total),
            s.shards_pending,
            s.parts.parts,
            s.parts.rows,
            ops::human_bytes(s.parts.bytes_on_disk),
            s.freshness_lag_secs
                .map_or_else(|| "-".to_string(), |lag| format!("{lag}s")),
        ));
        if let Some((planned, first)) = s.boundary {
            let verdict = match first {
                Some(first) if first < planned => "VIOLATED (view rows below the boundary)",
                _ => "ok",
            };
            out.push_str(&format!(
                "  view boundary: planned {planned}, earliest view row {}: {verdict}\n",
                first.map_or_else(|| "-".to_string(), |f| f.to_string())
            ));
        }
    }
    out
}
