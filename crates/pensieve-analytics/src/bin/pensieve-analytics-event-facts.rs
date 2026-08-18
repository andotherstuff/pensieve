//! Build and compare the bounded canonical event-fact Slice A canary.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    AnalyticsBuild, BatchLimits, BuildConfig, EventFactsConfig, build_bounded_event_facts,
    publish_canonical_json, resolve_snapshot,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
#[command(about = "Build bounded canonical event facts without changing live analytics")]
struct Args {
    /// Canonical active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Resolve catalog keys below this verified local root instead of reading S3.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Dedicated immutable batch/merge workspace.
    #[arg(long)]
    work_root: PathBuf,
    /// Dedicated canary DuckDB output.
    #[arg(long)]
    work_database: PathBuf,
    /// Immutable bounded-build evidence JSON.
    #[arg(long)]
    evidence: PathBuf,
    /// Optional accepted Slice A DuckDB checkpoint for byte comparison.
    #[arg(long)]
    reference_database: Option<PathBuf>,
    /// Immutable comparison evidence; required with `--reference-database`.
    #[arg(long, requires = "reference_database")]
    comparison_evidence: Option<PathBuf>,
    /// Fixed analytics Unix timestamp.
    #[arg(long)]
    as_of: u64,
    /// Operator or source revision recorded with the canary database.
    #[arg(long)]
    code_version: String,
    /// DuckDB memory limit applied to each bounded batch scan.
    #[arg(long, default_value = "4GB")]
    memory_limit: String,
    /// DuckDB workers used by one bounded scan.
    #[arg(long, default_value_t = 1)]
    threads: usize,
    /// Maximum compressed catalog bytes assigned to one batch.
    #[arg(long, default_value_t = 1_073_741_824)]
    batch_bytes: u64,
    /// Maximum physical rows assigned to one batch.
    #[arg(long, default_value_t = 5_000_000)]
    batch_rows: u64,
    /// Maximum immutable runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    merge_fan_in: usize,
    /// Free work-filesystem bytes that the preflight must leave untouched.
    #[arg(long, default_value_t = 53_687_091_200)]
    disk_reserve_bytes: u64,
    /// AWS region used by DuckDB's environment credential chain.
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    s3_region: String,
    /// Use path-style S3 addressing.
    #[arg(long)]
    s3_force_path_style: bool,
}

#[derive(Serialize)]
struct ComparisonEvidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    snapshot_id: String,
    as_of_epoch: u64,
    candidate_evidence_sha256: String,
    candidate_metric_sha256: String,
    reference_metric_sha256: String,
    byte_identical: bool,
}

#[derive(Serialize)]
struct Output {
    snapshot_id: String,
    physical_rows: u64,
    logical_events: u64,
    duplicate_rows: u64,
    batch_count: u64,
    merge_count: u64,
    final_artifact_sha256: String,
    metric_sha256: String,
    evidence_sha256: String,
    reference_byte_identical: Option<bool>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.reference_database.is_some() && args.comparison_evidence.is_none() {
        bail!("--comparison-evidence is required with --reference-database");
    }
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
    let build_config = BuildConfig {
        as_of_epoch: args.as_of,
        code_version: args.code_version,
        s3_region: args.s3_region,
        s3_force_path_style: args.s3_force_path_style,
        memory_limit: args.memory_limit,
        threads: args.threads,
    };
    let reference_bytes = args
        .reference_database
        .as_ref()
        .map(|path| {
            AnalyticsBuild::open_completed(path, snapshot.clone(), build_config.clone())
                .context("open accepted Slice A reference")?
                .canonical_metric_bytes()
                .context("serialize accepted Slice A metrics")
        })
        .transpose()?;
    let completed = build_bounded_event_facts(
        &args.work_database,
        &args.evidence,
        snapshot,
        build_config,
        EventFactsConfig {
            work_root: args.work_root,
            batch_limits: BatchLimits {
                max_bytes: args.batch_bytes,
                max_rows: args.batch_rows,
            },
            merge_fan_in: args.merge_fan_in,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build bounded canonical event facts")?;

    let candidate_bytes = completed
        .analytics
        .canonical_metric_bytes()
        .context("serialize bounded Slice A metrics")?;
    let reference_byte_identical = reference_bytes
        .as_ref()
        .map(|reference| reference == &candidate_bytes);
    if let (Some(reference), Some(path)) =
        (reference_bytes.as_ref(), args.comparison_evidence.as_ref())
    {
        let comparison = ComparisonEvidence {
            schema_version: 1,
            runner_version: "pensieve-analytics-event-facts-compare-v1",
            status: if reference == &candidate_bytes {
                "passed"
            } else {
                "failed"
            },
            snapshot_id: completed.evidence.snapshot_id.clone(),
            as_of_epoch: completed.evidence.as_of_epoch,
            candidate_evidence_sha256: completed.evidence_sha256.clone(),
            candidate_metric_sha256: hex::encode(Sha256::digest(&candidate_bytes)),
            reference_metric_sha256: hex::encode(Sha256::digest(reference)),
            byte_identical: reference == &candidate_bytes,
        };
        publish_canonical_json(path, &comparison).context("publish comparison evidence")?;
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: completed.evidence.snapshot_id.clone(),
            physical_rows: completed.evidence.physical_rows,
            logical_events: completed.evidence.logical_events,
            duplicate_rows: completed.evidence.duplicate_rows,
            batch_count: completed.evidence.batch_count,
            merge_count: completed.evidence.merge_count,
            final_artifact_sha256: completed.evidence.final_artifact.sha256.clone(),
            metric_sha256: completed.evidence.metric_sha256.clone(),
            evidence_sha256: completed.evidence_sha256.clone(),
            reference_byte_identical,
        })?
    );
    if reference_byte_identical == Some(false) {
        bail!("bounded metric bytes differ from the accepted Slice A checkpoint");
    }
    Ok(())
}
