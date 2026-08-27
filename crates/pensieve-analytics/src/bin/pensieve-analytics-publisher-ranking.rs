//! Build resumable exact publisher rankings for predefined rolling windows.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    PublisherRankingConfig, build_bounded_publisher_ranking, load_bounded_fixed_activity,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build exact bounded predefined-window publisher rankings")]
struct Args {
    /// Validated fixed-activity evidence for one frozen snapshot.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Durable resumable SQLite ranking ledger.
    #[arg(long)]
    state_database: PathBuf,
    /// Canonical fixed-width top-ranking artifact.
    #[arg(long)]
    artifact: PathBuf,
    /// Canonical completion evidence.
    #[arg(long)]
    evidence: PathBuf,
    /// Strictly increasing supported rolling windows.
    #[arg(long, value_delimiter = ',', default_value = "1,7,30,90,365")]
    windows_days: Vec<u32>,
    /// Maximum publishers retained per window/filter.
    #[arg(long, default_value_t = 1_000)]
    top_limit: usize,
    /// Pubkeys committed in one SQLite transaction.
    #[arg(long, default_value_t = 10_000)]
    publisher_batch_size: usize,
    /// Hard SQLite state ceiling.
    #[arg(long, default_value_t = 536_870_912_000_u64)]
    max_state_bytes: u64,
    /// Fixed SQLite page cache.
    #[arg(long, default_value_t = 536_870_912_u64)]
    sqlite_cache_bytes: u64,
    /// Free work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400_u64)]
    disk_reserve_bytes: u64,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    source_records: u64,
    ledger_rows: u64,
    ranking_groups: u64,
    ranking_rows: u64,
    ranking_bytes: u64,
    max_publisher_kinds_buffered: usize,
    evidence_sha256: &'a str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("publisher ranking failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load validated fixed-activity evidence")?;
    let product = build_bounded_publisher_ranking(
        &args.evidence,
        &activity,
        PublisherRankingConfig {
            state_database: args.state_database,
            artifact_path: args.artifact,
            windows_days: args.windows_days,
            top_limit: args.top_limit,
            publisher_batch_size: args.publisher_batch_size,
            max_state_bytes: args.max_state_bytes,
            sqlite_cache_bytes: args.sqlite_cache_bytes,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build exact bounded publisher rankings")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &product.evidence.snapshot_id,
            as_of_epoch: product.evidence.as_of_epoch,
            source_records: product.evidence.source_records,
            ledger_rows: product.evidence.ledger_rows,
            ranking_groups: product.evidence.ranking_groups,
            ranking_rows: product.evidence.ranking_artifact.row_count,
            ranking_bytes: product.evidence.ranking_artifact.byte_size,
            max_publisher_kinds_buffered: product.evidence.max_publisher_kinds_buffered,
            evidence_sha256: &product.evidence_sha256,
        })?
    );
    Ok(())
}
