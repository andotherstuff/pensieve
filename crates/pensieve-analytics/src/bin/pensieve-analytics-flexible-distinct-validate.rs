//! Validate production flexible-distinct leaves against exact daily metrics.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    FLEXIBLE_DISTINCT_TOLERANCE_PPM, build_flexible_distinct_validation,
    load_bounded_fixed_activity,
};

#[derive(Debug, Parser)]
#[command(about = "Compare bounded flexible distinct sketches with exact daily products")]
struct Args {
    /// Validated Slice 5 fixed-activity evidence.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Validated Slice 6 flexible-distinct evidence.
    #[arg(long)]
    flexible_evidence: PathBuf,
    /// Canonical immutable validation evidence to create.
    #[arg(long)]
    evidence: PathBuf,
    /// Maximum accepted relative error in parts per million.
    #[arg(long, default_value_t = FLEXIBLE_DISTINCT_TOLERANCE_PPM)]
    tolerance_ppm: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("flexible-distinct validation failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load validated fixed-activity evidence")?;
    let evidence = build_flexible_distinct_validation(
        &args.evidence,
        &activity,
        &args.flexible_evidence,
        args.tolerance_ppm,
    )
    .context("build flexible-distinct production tolerance evidence")?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}
