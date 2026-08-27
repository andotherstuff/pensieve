//! Build resumable bounded Slice 7 semantic facts without publishing Postgres.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    BatchLimits, BuildConfig, SemanticFactsConfig, build_bounded_semantic_facts, resolve_snapshot,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded canonical engagement, long-form, and zap facts")]
struct Args {
    /// Canonical active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Resolve catalog keys below this verified local root instead of S3.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Dedicated immutable batch/merge workspace.
    #[arg(long)]
    work_root: PathBuf,
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
    /// Maximum immutable runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    merge_fan_in: usize,
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
struct Output {
    snapshot_id: String,
    physical_rows: u64,
    physical_relevant_rows: u64,
    retained_relevant_events: u64,
    logical_relevant_events: u64,
    duplicate_relevant_rows: u64,
    batch_count: u64,
    merge_count: u64,
    final_artifact_sha256: String,
    rollup_sha256: String,
    evidence_sha256: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
    let completed = build_bounded_semantic_facts(
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
        SemanticFactsConfig {
            work_root: args.work_root,
            batch_limits: BatchLimits {
                max_bytes: args.batch_bytes,
                max_rows: args.batch_rows,
            },
            merge_fan_in: args.merge_fan_in,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build bounded semantic facts")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: completed.evidence.snapshot_id.clone(),
            physical_rows: completed.evidence.physical_rows,
            physical_relevant_rows: completed.evidence.physical_relevant_rows,
            retained_relevant_events: completed.evidence.retained_relevant_events,
            logical_relevant_events: completed.evidence.logical_relevant_events,
            duplicate_relevant_rows: completed.evidence.duplicate_relevant_rows,
            batch_count: completed.evidence.batch_count,
            merge_count: completed.evidence.merge_count,
            final_artifact_sha256: completed.evidence.final_artifact.sha256.clone(),
            rollup_sha256: completed.evidence.rollup_sha256.clone(),
            evidence_sha256: completed.evidence_sha256,
        })?
    );
    Ok(())
}
