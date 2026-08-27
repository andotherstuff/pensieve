//! Build bounded complete-hour distinct-author sketch evidence.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    FlexibleDistinctConfig, advance_bounded_flexible_distinct, build_bounded_flexible_distinct,
    load_bounded_fixed_activity, load_bounded_flexible_distinct,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded complete-hour distinct-author sketches")]
struct Args {
    /// Validated Slice 5 fixed-activity evidence.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Prior flexible-distinct evidence; enables an append-only successor build.
    #[arg(long, requires = "baseline_activity_evidence")]
    baseline_flexible_evidence: Option<PathBuf>,
    /// Prior fixed-activity evidence consumed by the baseline flexible product.
    #[arg(long, requires = "baseline_flexible_evidence")]
    baseline_activity_evidence: Option<PathBuf>,
    /// Dedicated immutable batch, merge, and leaf workspace.
    #[arg(long)]
    work_root: PathBuf,
    /// Canonical immutable flexible-distinct evidence to create.
    #[arg(long)]
    evidence: PathBuf,
    /// Exact activity records transformed by one bounded batch sort.
    #[arg(long, default_value_t = 1_000_000)]
    source_records_per_batch: u64,
    /// Maximum immutable runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    merge_fan_in: usize,
    /// Free work-filesystem bytes that preflight must leave untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    disk_reserve_bytes: u64,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    complete_through_epoch: u64,
    baseline_evidence_sha256: Option<&'a str>,
    source_activity_rows: u64,
    batch_count: u64,
    merge_count: u64,
    identity_rows: u64,
    leaf_rows: u64,
    max_batch_buffered_bytes: u64,
    max_merge_buffered_bytes: usize,
    max_leaf_bytes: usize,
    identity_sha256: &'a str,
    leaf_sha256: &'a str,
    evidence_sha256: &'a str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("flexible-distinct build failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load validated fixed-activity evidence")?;
    let config = FlexibleDistinctConfig {
        work_root: args.work_root,
        source_records_per_batch: args.source_records_per_batch,
        merge_fan_in: args.merge_fan_in,
        disk_reserve_bytes: args.disk_reserve_bytes,
    };
    let completed = match (
        args.baseline_flexible_evidence.as_ref(),
        args.baseline_activity_evidence.as_ref(),
    ) {
        (Some(flexible_path), Some(activity_path)) => {
            let baseline = load_bounded_flexible_distinct(flexible_path)
                .context("load validated baseline flexible-distinct evidence")?;
            let baseline_activity = load_bounded_fixed_activity(activity_path)
                .context("load validated baseline fixed-activity evidence")?;
            advance_bounded_flexible_distinct(
                &args.evidence,
                &baseline,
                &baseline_activity,
                &activity,
                config,
            )
            .context("advance bounded flexible distinct sketches")?
        }
        (None, None) => build_bounded_flexible_distinct(&args.evidence, &activity, config)
            .context("build bounded flexible distinct sketches")?,
        _ => unreachable!("clap requires complete baseline arguments"),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &completed.evidence.snapshot_id,
            as_of_epoch: completed.evidence.as_of_epoch,
            complete_through_epoch: completed.evidence.complete_through_epoch,
            baseline_evidence_sha256: completed.evidence.baseline_evidence_sha256.as_deref(),
            source_activity_rows: completed.evidence.source_activity_rows,
            batch_count: completed.evidence.batch_count,
            merge_count: completed.evidence.merge_count,
            identity_rows: completed.evidence.identity_artifact.row_count,
            leaf_rows: completed.evidence.leaf_artifact.row_count,
            max_batch_buffered_bytes: completed.evidence.max_batch_buffered_bytes,
            max_merge_buffered_bytes: completed.evidence.max_merge_buffered_bytes,
            max_leaf_bytes: completed.evidence.max_leaf_bytes,
            identity_sha256: &completed.evidence.identity_artifact.sha256,
            leaf_sha256: &completed.evidence.leaf_artifact.sha256,
            evidence_sha256: &completed.evidence_sha256,
        })?
    );
    Ok(())
}
