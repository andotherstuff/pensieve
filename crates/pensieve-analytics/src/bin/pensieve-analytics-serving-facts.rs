//! Build resumable bounded Slice 9.5 serving facts without publishing Postgres.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    BatchLimits, BuildConfig, ServingFactsConfig, build_bounded_serving_facts, resolve_snapshot,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded canonical hourly and per-kind serving facts")]
struct Args {
    /// Canonical active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Resolve catalog keys below this verified local root instead of S3.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Validated event-facts evidence for the same frozen generation.
    #[arg(long)]
    event_facts_evidence: PathBuf,
    /// Validated corrected fixed-activity evidence for the same generation.
    #[arg(long)]
    activity_evidence: PathBuf,
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
    logical_events: u64,
    duplicate_rows: u64,
    batch_count: u64,
    merge_count: u64,
    hourly_rows: u64,
    kind_rows: u64,
    complete_hour_events: u64,
    content_artifact_sha256: String,
    hourly_artifact_sha256: String,
    kind_artifact_sha256: String,
    evidence_sha256: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
    let completed = build_bounded_serving_facts(
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
        ServingFactsConfig {
            work_root: args.work_root,
            batch_limits: BatchLimits {
                max_bytes: args.batch_bytes,
                max_rows: args.batch_rows,
            },
            merge_fan_in: args.merge_fan_in,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
        &args.event_facts_evidence,
        &args.activity_evidence,
    )
    .context("build bounded serving facts")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: completed.evidence.snapshot_id.clone(),
            physical_rows: completed.evidence.physical_rows,
            logical_events: completed.evidence.logical_events,
            duplicate_rows: completed.evidence.duplicate_rows,
            batch_count: completed.evidence.batch_count,
            merge_count: completed.evidence.merge_count,
            hourly_rows: completed.evidence.hourly_artifact.row_count,
            kind_rows: completed.evidence.kind_artifact.row_count,
            complete_hour_events: completed.evidence.complete_hour_events,
            content_artifact_sha256: completed.evidence.content_artifact.sha256.clone(),
            hourly_artifact_sha256: completed.evidence.hourly_artifact.sha256.clone(),
            kind_artifact_sha256: completed.evidence.kind_artifact.sha256.clone(),
            evidence_sha256: completed.evidence_sha256,
        })?
    );
    Ok(())
}
