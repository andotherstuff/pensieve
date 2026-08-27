//! Atomically publish dormant Slice 6 leaves without moving analytics pointers.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    COHORT_RETENTION_QUERY_VERSION, FlexibleDistinctPublishOutcome, load_bounded_flexible_distinct,
    publish_flexible_distinct_leaves,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Publish versioned flexible-distinct leaves without changing current_run")]
struct Args {
    /// Validated flexible-distinct builder evidence.
    #[arg(long)]
    flexible_evidence: PathBuf,
    /// Passed production tolerance evidence.
    #[arg(long)]
    validation_evidence: PathBuf,
    /// Explicitly authorized SHA-256 of the tolerance evidence.
    #[arg(long)]
    validation_evidence_sha256: String,
    /// Exact current corrected B3 run to which this product is bound.
    #[arg(long)]
    baseline_run_id: String,
    /// Postgres connection string for atomic publication.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: String,
    /// Postgres password supplied separately from the connection string.
    #[arg(long, env = "POSTGRES_ANALYTICS_PASSWORD")]
    postgres_password: Option<String>,
    /// Validate product and baseline without writing schema or rows.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    complete_through_epoch: u64,
    baseline_run_id: &'a str,
    product_id: Option<String>,
    publication_status: &'static str,
    dry_run: bool,
    leaf_rows: u64,
    leaf_bytes: u64,
    flexible_evidence_sha256: &'a str,
    validation_evidence_sha256: &'a str,
    current_pointer_changed: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("flexible-distinct publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    connect_postgres(&args)
        .context("preflight Postgres connection")?
        .simple_query("SELECT 1")
        .context("preflight Postgres query")?;
    let product = load_bounded_flexible_distinct(&args.flexible_evidence)
        .context("load and validate flexible-distinct evidence")?;
    let actual_validation_sha =
        pensieve_lake::sha256_file(&args.validation_evidence).context("hash tolerance evidence")?;
    if actual_validation_sha != args.validation_evidence_sha256 {
        bail!("tolerance evidence SHA-256 differs from the authorized gate");
    }

    let mut client = connect_postgres(&args).context("connect to Postgres for publication")?;
    let current = client
        .query_one(
            "SELECT run_id, snapshot_id, query_version, as_of_epoch
               FROM pensieve_analytics.current_run_metadata",
            &[],
        )
        .context("load current corrected B3 baseline")?;
    let current_run_id: String = current.get(0);
    let current_snapshot: String = current.get(1);
    let current_query_version: String = current.get(2);
    let current_as_of: i64 = current.get(3);
    if current_run_id != args.baseline_run_id
        || current_snapshot != product.evidence.snapshot_id
        || current_query_version != COHORT_RETENTION_QUERY_VERSION
        || current_as_of
            != i64::try_from(product.evidence.as_of_epoch).context("as-of exceeds i64")?
    {
        bail!("current Postgres run is not the exact corrected B3 Slice 6 baseline");
    }

    let (product_id, publication_status) = if args.dry_run {
        (None, "not_published")
    } else {
        match publish_flexible_distinct_leaves(
            &mut client,
            &args.baseline_run_id,
            &product,
            &args.validation_evidence,
            &args.validation_evidence_sha256,
        )
        .context("atomically publish dormant flexible-distinct leaves")?
        {
            FlexibleDistinctPublishOutcome::Published { product_id } => {
                (Some(product_id), "published")
            }
            FlexibleDistinctPublishOutcome::AlreadyPublished { product_id } => {
                (Some(product_id), "already_published")
            }
        }
    };
    let current_after: String = client
        .query_one(
            "SELECT run_id FROM pensieve_analytics.current_run WHERE singleton = true",
            &[],
        )?
        .get(0);
    if current_after != args.baseline_run_id {
        bail!("analytics current pointer changed during dormant Slice 6 publication");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &product.evidence.snapshot_id,
            as_of_epoch: product.evidence.as_of_epoch,
            complete_through_epoch: product.evidence.complete_through_epoch,
            baseline_run_id: &args.baseline_run_id,
            product_id,
            publication_status,
            dry_run: args.dry_run,
            leaf_rows: product.evidence.leaf_artifact.row_count,
            leaf_bytes: product.evidence.leaf_artifact.byte_size,
            flexible_evidence_sha256: &product.evidence_sha256,
            validation_evidence_sha256: &args.validation_evidence_sha256,
            current_pointer_changed: false,
        })?
    );
    Ok(())
}

fn connect_postgres(args: &Args) -> Result<postgres::Client> {
    let mut config: PostgresConfig = args
        .postgres_url
        .parse()
        .context("parse Postgres connection")?;
    if let Some(password) = &args.postgres_password {
        config.password(password);
    }
    config
        .connect(NoTls)
        .context("connect to Postgres without TLS")
}
