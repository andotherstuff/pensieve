//! Build and validate exact bounded cohort-retention evidence.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    build_bounded_cohort_retention, load_bounded_fixed_activity, load_bounded_pubkey_first_seen,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build exact bounded weekly and monthly cohort retention")]
struct Args {
    /// Validated first-seen evidence for the selected snapshot and as-of.
    #[arg(long)]
    identity_evidence: PathBuf,
    /// Validated fixed-activity evidence for the same snapshot and as-of.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Canonical immutable cohort-retention evidence to create.
    #[arg(long)]
    evidence: PathBuf,
    /// Hard ceiling for compact `(grain, cohort, activity period)` rows.
    #[arg(long, default_value_t = 2_000_000)]
    matrix_row_limit: usize,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    period_rows: u64,
    active_pubkeys_sum: u64,
    matrix_row_limit: usize,
    max_pubkey_periods_buffered: usize,
    identity_evidence_sha256: &'a str,
    activity_evidence_sha256: &'a str,
    metric_sha256: &'a str,
    evidence_sha256: &'a str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("cohort-retention build failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let identity = load_bounded_pubkey_first_seen(&args.identity_evidence)
        .context("load bounded first-seen evidence")?;
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load bounded fixed-activity evidence")?;
    let completed =
        build_bounded_cohort_retention(&args.evidence, &identity, &activity, args.matrix_row_limit)
            .context("build bounded cohort retention")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &completed.evidence.snapshot_id,
            as_of_epoch: completed.evidence.as_of_epoch,
            period_rows: completed.evidence.period_rows,
            active_pubkeys_sum: completed.evidence.active_pubkeys_sum,
            matrix_row_limit: completed.evidence.matrix_row_limit,
            max_pubkey_periods_buffered: completed.evidence.max_pubkey_periods_buffered,
            identity_evidence_sha256: &completed.evidence.identity_evidence_sha256,
            activity_evidence_sha256: &completed.evidence.activity_evidence_sha256,
            metric_sha256: &completed.evidence.metric_sha256,
            evidence_sha256: &completed.evidence_sha256,
        })?
    );
    Ok(())
}
