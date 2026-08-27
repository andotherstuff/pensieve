//! Build resumable bounded current NIP-65 relay distribution without publishing.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    BatchLimits, BuildConfig, RelayDistributionConfig, build_bounded_relay_distribution,
    resolve_snapshot,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded deterministic current NIP-65 relay distribution")]
struct Args {
    /// Canonical active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Resolve catalog keys below this verified local root instead of S3.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Durable resumable SQLite state database.
    #[arg(long)]
    state_database: PathBuf,
    /// Immutable completion evidence JSON.
    #[arg(long)]
    evidence: PathBuf,
    /// Fixed analytics Unix timestamp.
    #[arg(long)]
    as_of: u64,
    /// Operator or source revision recorded with the run.
    #[arg(long)]
    code_version: String,
    /// DuckDB memory limit for each bounded scan.
    #[arg(long, default_value = "4GB")]
    memory_limit: String,
    /// DuckDB workers used by one scan.
    #[arg(long, default_value_t = 1)]
    threads: usize,
    /// Maximum compressed catalog bytes in one batch.
    #[arg(long, default_value_t = 1_073_741_824)]
    batch_bytes: u64,
    /// Maximum physical rows in one batch.
    #[arg(long, default_value_t = 5_000_000)]
    batch_rows: u64,
    /// Maximum durable SQLite state bytes.
    #[arg(long, default_value_t = 107_374_182_400)]
    max_state_bytes: u64,
    /// SQLite page-cache bound in bytes.
    #[arg(long, default_value_t = 268_435_456)]
    sqlite_cache_bytes: u64,
    /// Minimum winning users retained in the final relation.
    #[arg(long, default_value_t = 10)]
    minimum_users: u64,
    /// Free work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    disk_reserve_bytes: u64,
    /// AWS region used by DuckDB's environment credential chain.
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    s3_region: String,
    /// Use path-style S3 addressing.
    #[arg(long)]
    s3_force_path_style: bool,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    candidate_events: u64,
    winning_pubkeys: u64,
    candidate_memberships: u64,
    relay_rows: usize,
    rows_sha256: &'a str,
    evidence_sha256: &'a str,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
    let completed = build_bounded_relay_distribution(
        &args.evidence,
        snapshot,
        BuildConfig {
            as_of_epoch: args.as_of,
            code_version: args.code_version,
            s3_region: args.s3_region,
            s3_force_path_style: args.s3_force_path_style,
            memory_limit: args.memory_limit,
            threads: args.threads,
        },
        RelayDistributionConfig {
            state_database: args.state_database,
            batch_limits: BatchLimits {
                max_bytes: args.batch_bytes,
                max_rows: args.batch_rows,
            },
            max_state_bytes: args.max_state_bytes,
            sqlite_cache_bytes: args.sqlite_cache_bytes,
            minimum_users: args.minimum_users,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build bounded relay distribution")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &completed.evidence.snapshot_id,
            as_of_epoch: completed.evidence.as_of_epoch,
            candidate_events: completed.evidence.candidate_events,
            winning_pubkeys: completed.evidence.winning_pubkeys,
            candidate_memberships: completed.evidence.candidate_memberships,
            relay_rows: completed.evidence.rows.len(),
            rows_sha256: &completed.evidence.rows_sha256,
            evidence_sha256: &completed.evidence_sha256,
        })?
    );
    Ok(())
}
