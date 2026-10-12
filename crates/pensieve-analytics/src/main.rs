//! Build exact Slice A analytics rollups into a local DuckDB database.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::{AnalyticsBuild, BuildConfig, resolve_snapshot};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build exact Slice A analytics from one active-file snapshot")]
struct Args {
    /// Canonically encoded active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Persistent DuckDB work database used for scans and spill.
    #[arg(long)]
    work_database: PathBuf,
    /// Revalidate an already completed work database without rebuilding it.
    #[arg(long)]
    reuse_completed_build: bool,
    /// Resolve catalog keys below this local root instead of reading S3.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Fixed Unix timestamp for rolling metrics; defaults to process start.
    #[arg(long)]
    as_of: Option<u64>,
    /// Build/commit identity stored in the DuckDB build metadata.
    #[arg(
        long,
        env = "PENSIEVE_ANALYTICS_CODE_VERSION",
        default_value = concat!("pensieve-analytics/", env!("CARGO_PKG_VERSION"))
    )]
    code_version: String,
    /// AWS region used by DuckDB's environment credential chain.
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    s3_region: String,
    /// Use path-style S3 addressing.
    #[arg(long)]
    s3_force_path_style: bool,
    /// DuckDB buffer-manager limit. Keep below host capacity for colocated services.
    #[arg(
        long,
        env = "PENSIEVE_ANALYTICS_DUCKDB_MEMORY_LIMIT",
        default_value = "48GB"
    )]
    duckdb_memory_limit: String,
    /// DuckDB worker threads. Lower values reduce peak query memory.
    #[arg(long, env = "PENSIEVE_ANALYTICS_DUCKDB_THREADS", default_value_t = 4)]
    duckdb_threads: usize,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    code_version: &'a str,
    overview: pensieve_analytics::Overview,
    build: &'a pensieve_analytics::BuildSummary,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("analytics build failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let as_of_epoch = args.as_of.unwrap_or_else(unix_now);
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve active-file snapshot")?;
    let config = BuildConfig {
        as_of_epoch,
        code_version: args.code_version,
        s3_region: args.s3_region,
        s3_force_path_style: args.s3_force_path_style,
        memory_limit: args.duckdb_memory_limit,
        threads: args.duckdb_threads,
    };
    let build = if args.reuse_completed_build {
        AnalyticsBuild::open_completed(&args.work_database, snapshot, config)
            .context("open and revalidate completed DuckDB rollups")?
    } else {
        AnalyticsBuild::create(&args.work_database, snapshot, config)
            .context("materialize and validate DuckDB rollups")?
    };
    let output = Output {
        snapshot_id: &build.snapshot.catalog.snapshot_id,
        as_of_epoch,
        code_version: &build.config.code_version,
        overview: build.overview()?,
        build: &build.summary,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_secs()
}
