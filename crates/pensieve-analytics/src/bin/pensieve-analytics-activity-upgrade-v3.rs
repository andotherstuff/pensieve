//! Re-finalize immutable fixed-activity v2 state under corrected v3 semantics.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::upgrade_bounded_fixed_activity_v2;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Upgrade fixed-activity v2 evidence to corrected v3 daily-kind semantics")]
struct Args {
    /// Canonical immutable fixed-activity v2 evidence.
    #[arg(long)]
    legacy_evidence: PathBuf,
    /// Canonical immutable fixed-activity v3 evidence to create.
    #[arg(long)]
    evidence: PathBuf,
}

#[derive(Debug, Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    activity_rows: u64,
    distinct_period_rows: u64,
    active_period_rows: u64,
    activity_artifact_sha256: &'a str,
    flags_artifact_sha256: &'a str,
    metric_sha256: &'a str,
    legacy_evidence_sha256: &'a str,
    evidence_sha256: &'a str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fixed-activity v3 upgrade failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let completed = upgrade_bounded_fixed_activity_v2(&args.evidence, &args.legacy_evidence)
        .context("re-finalize fixed-activity v2 evidence")?;
    let legacy_evidence_sha256 = completed
        .evidence
        .semantic_upgrade_evidence_sha256
        .as_deref()
        .context("upgraded evidence omitted its v2 parent identity")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &completed.evidence.snapshot_id,
            as_of_epoch: completed.evidence.as_of_epoch,
            activity_rows: completed.evidence.activity_artifact.row_count,
            distinct_period_rows: completed.evidence.distinct_period_rows,
            active_period_rows: completed.evidence.active_period_rows,
            activity_artifact_sha256: &completed.evidence.activity_artifact.sha256,
            flags_artifact_sha256: &completed.evidence.flags_artifact.sha256,
            metric_sha256: &completed.evidence.metric_sha256,
            legacy_evidence_sha256,
            evidence_sha256: &completed.evidence_sha256,
        })?
    );
    Ok(())
}
