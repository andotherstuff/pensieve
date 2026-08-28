//! Atomically publish dormant Slice 9.5 serving facts without moving pointers.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    COHORT_RETENTION_QUERY_VERSION, ServingFactsPublishOutcome, load_bounded_serving_facts,
    publish_serving_facts,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Publish versioned Slice 9.5 serving facts without changing current_run")]
struct Args {
    /// Validated serving-facts completion evidence.
    #[arg(long)]
    evidence: PathBuf,
    /// Explicitly authorized SHA-256 of completion evidence.
    #[arg(long)]
    evidence_sha256: String,
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
    logical_events: u64,
    hourly_rows: u64,
    kind_rows: u64,
    evidence_sha256: &'a str,
    current_pointer_changed: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("serving-facts publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    connect_postgres(&args)
        .context("preflight Postgres connection")?
        .simple_query("SELECT 1")
        .context("preflight Postgres query")?;
    let product =
        load_bounded_serving_facts(&args.evidence).context("load and validate serving facts")?;
    if product.evidence_sha256 != args.evidence_sha256 {
        bail!("serving evidence SHA-256 differs from the authorized gate");
    }
    let mut client = connect_postgres(&args).context("connect to Postgres for publication")?;
    let current = client.query_one(
        "SELECT run_id,snapshot_id,query_version,as_of_epoch,
                validation ->> 'fixed_activity_evidence_sha256'
           FROM pensieve_analytics.current_run_metadata",
        &[],
    )?;
    if current.get::<_, String>(0) != args.baseline_run_id
        || current.get::<_, String>(1) != product.evidence.snapshot_id
        || current.get::<_, String>(2) != COHORT_RETENTION_QUERY_VERSION
        || current.get::<_, i64>(3) != i64::try_from(product.evidence.as_of_epoch)?
        || current.get::<_, Option<String>>(4).as_deref()
            != Some(product.evidence.activity_evidence_sha256.as_str())
    {
        bail!("current Postgres run is not the exact corrected B3 Slice 9.5 baseline");
    }
    let (product_id, publication_status) = if args.dry_run {
        (None, "not_published")
    } else {
        match publish_serving_facts(&mut client, &args.baseline_run_id, &product)
            .context("atomically publish dormant serving facts")?
        {
            ServingFactsPublishOutcome::Published { product_id } => (Some(product_id), "published"),
            ServingFactsPublishOutcome::AlreadyPublished { product_id } => {
                (Some(product_id), "already_published")
            }
        }
    };
    let current_after: String = client
        .query_one(
            "SELECT run_id FROM pensieve_analytics.current_run WHERE singleton=true",
            &[],
        )?
        .get(0);
    if current_after != args.baseline_run_id {
        bail!("analytics current pointer changed during dormant serving publication");
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
            logical_events: product.evidence.logical_events,
            hourly_rows: product.evidence.hourly_artifact.row_count,
            kind_rows: product.evidence.kind_artifact.row_count,
            evidence_sha256: &product.evidence_sha256,
            current_pointer_changed: false,
        })?
    );
    Ok(())
}

fn connect_postgres(args: &Args) -> Result<postgres::Client> {
    let mut config: PostgresConfig = args.postgres_url.parse().context("parse Postgres URL")?;
    if let Some(password) = &args.postgres_password {
        config.password(password);
    }
    config.connect(NoTls).context("connect to Postgres")
}
