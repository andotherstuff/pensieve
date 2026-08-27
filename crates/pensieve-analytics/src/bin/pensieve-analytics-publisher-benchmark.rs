//! Benchmark bounded exact publisher-serving contracts from fixed activity state.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{
    PublisherBenchmarkConfig, benchmark_publishers, load_bounded_fixed_activity,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Benchmark exact predefined publisher windows with fixed memory")]
struct Args {
    /// Validated fixed-activity evidence for one frozen snapshot.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Canonical benchmark evidence to create.
    #[arg(long)]
    evidence: PathBuf,
    /// Strictly increasing exact rolling windows, in days.
    #[arg(long, value_delimiter = ',', default_value = "1,7,30,90,365")]
    windows_days: Vec<u32>,
    /// Strictly increasing representative kinds retained in exact top rows.
    #[arg(long, value_delimiter = ',', default_value = "1,7,9735,30023")]
    sampled_kinds: Vec<u16>,
    /// Maximum publishers retained for every exact window and filter.
    #[arg(long, default_value_t = 1_000)]
    top_limit: usize,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    source_records: u64,
    publisher_daily_rows: u64,
    publisher_daily_kind_rows: u64,
    publisher_daily_compact_bytes: u64,
    publisher_daily_kind_compact_bytes: u64,
    materialized_top_rows: u64,
    materialized_top_compact_bytes: u64,
    scan_elapsed_millis: u64,
    scan_records_per_second: u64,
    evidence_sha256: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("publisher benchmark failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load validated fixed-activity evidence")?;
    let evidence = benchmark_publishers(
        &args.evidence,
        &activity,
        PublisherBenchmarkConfig {
            windows_days: args.windows_days,
            sampled_kinds: args.sampled_kinds,
            top_limit: args.top_limit,
        },
    )
    .context("run bounded publisher benchmark")?;
    let evidence_sha256 =
        pensieve_lake::sha256_file(&args.evidence).context("hash publisher benchmark evidence")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &evidence.snapshot_id,
            as_of_epoch: evidence.as_of_epoch,
            source_records: evidence.source_records,
            publisher_daily_rows: evidence.publisher_daily_rows,
            publisher_daily_kind_rows: evidence.publisher_daily_kind_rows,
            publisher_daily_compact_bytes: evidence.publisher_daily_compact_bytes,
            publisher_daily_kind_compact_bytes: evidence.publisher_daily_kind_compact_bytes,
            materialized_top_rows: evidence.materialized_top_rows,
            materialized_top_compact_bytes: evidence.materialized_top_compact_bytes,
            scan_elapsed_millis: evidence.scan_elapsed_millis,
            scan_records_per_second: evidence.scan_records_per_second,
            evidence_sha256,
        })?
    );
    Ok(())
}
