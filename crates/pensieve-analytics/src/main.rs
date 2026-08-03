//! Build and optionally publish the first lakehouse analytics slice.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use pensieve_analytics::{AnalyticsBuild, BuildConfig, PublishOutcome, publish, resolve_snapshot};
use postgres::{Client, NoTls};
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
    /// Resolve catalog keys below this local root instead of reading S3.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Fixed Unix timestamp for rolling metrics; defaults to process start.
    #[arg(long)]
    as_of: Option<u64>,
    /// Build/commit identity stored in analytics run metadata.
    #[arg(
        long,
        env = "PENSIEVE_ANALYTICS_CODE_VERSION",
        default_value = concat!("pensieve-analytics/", env!("CARGO_PKG_VERSION"))
    )]
    code_version: String,
    /// Postgres connection string. Omit to build and validate without publishing.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: Option<String>,
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
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    code_version: &'a str,
    overview: pensieve_analytics::Overview,
    build: &'a pensieve_analytics::BuildSummary,
    publication: Option<PublicationOutput>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PublicationOutput {
    Published {
        run_id: String,
        previous_run_id: Option<String>,
    },
    AlreadyCurrent {
        run_id: String,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("analytics build failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let started_at = Utc::now();
    let as_of_epoch = args.as_of.unwrap_or_else(unix_now);
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve active-file snapshot")?;
    let build = AnalyticsBuild::create(
        &args.work_database,
        snapshot,
        BuildConfig {
            as_of_epoch,
            code_version: args.code_version,
            s3_region: args.s3_region,
            s3_force_path_style: args.s3_force_path_style,
            memory_limit: args.duckdb_memory_limit,
        },
    )
    .context("materialize and validate DuckDB rollups")?;
    let completed_at = Utc::now();
    let publication = args
        .postgres_url
        .as_deref()
        .map(|url| publish_build(url, &build, started_at, completed_at))
        .transpose()?;
    let output = Output {
        snapshot_id: &build.snapshot.catalog.snapshot_id,
        as_of_epoch,
        code_version: &build.config.code_version,
        overview: build.overview()?,
        build: &build.summary,
        publication,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn publish_build(
    postgres_url: &str,
    build: &AnalyticsBuild,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublicationOutput> {
    let mut client =
        Client::connect(postgres_url, NoTls).context("connect to Postgres without TLS")?;
    Ok(
        match publish(&mut client, build, started_at, completed_at)? {
            PublishOutcome::Published {
                run_id,
                previous_run_id,
            } => PublicationOutput::Published {
                run_id,
                previous_run_id,
            },
            PublishOutcome::AlreadyCurrent { run_id } => {
                PublicationOutput::AlreadyCurrent { run_id }
            }
        },
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_secs()
}
