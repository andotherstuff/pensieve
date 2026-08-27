//! Atomically publish dormant Slice 8 relay rows without moving pointers.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    RelayDistributionPublishOutcome, load_bounded_relay_distribution, publish_relay_distribution,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Publish versioned Slice 8 relay rows without changing current_run")]
struct Args {
    /// Validated relay completion evidence.
    #[arg(long)]
    evidence: PathBuf,
    /// Resumable state database against which evidence is revalidated.
    #[arg(long)]
    state_database: PathBuf,
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
    baseline_run_id: &'a str,
    product_id: Option<String>,
    publication_status: &'static str,
    dry_run: bool,
    candidate_events: u64,
    winning_pubkeys: u64,
    relay_rows: usize,
    evidence_sha256: &'a str,
    current_pointer_changed: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("relay distribution publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let product = load_bounded_relay_distribution(&args.evidence, &args.state_database)
        .context("load and validate relay distribution")?;
    if product.evidence_sha256 != args.evidence_sha256 {
        bail!("relay evidence SHA-256 differs from the authorized gate");
    }
    let mut config: PostgresConfig = args.postgres_url.parse().context("parse Postgres URL")?;
    if let Some(password) = args.postgres_password {
        config.password(password);
    }
    let mut client = config.connect(NoTls).context("connect to Postgres")?;
    let current = client.query_one(
        "SELECT run_id,snapshot_id,query_version,as_of_epoch
           FROM pensieve_analytics.current_run_metadata",
        &[],
    )?;
    if current.get::<_, String>(0) != args.baseline_run_id
        || current.get::<_, String>(1) != product.evidence.snapshot_id
        || current.get::<_, String>(2) != pensieve_analytics::COHORT_RETENTION_QUERY_VERSION
        || current.get::<_, i64>(3) != i64::try_from(product.evidence.as_of_epoch)?
    {
        bail!("current Postgres run is not the exact corrected B3 Slice 8 baseline");
    }
    let (product_id, publication_status) = if args.dry_run {
        (None, "not_published")
    } else {
        match publish_relay_distribution(&mut client, &args.baseline_run_id, &product)
            .context("atomically publish dormant relay distribution")?
        {
            RelayDistributionPublishOutcome::Published { product_id } => {
                (Some(product_id), "published")
            }
            RelayDistributionPublishOutcome::AlreadyPublished { product_id } => {
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
        bail!("analytics current pointer changed during dormant relay publication");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &product.evidence.snapshot_id,
            as_of_epoch: product.evidence.as_of_epoch,
            baseline_run_id: &args.baseline_run_id,
            product_id,
            publication_status,
            dry_run: args.dry_run,
            candidate_events: product.evidence.candidate_events,
            winning_pubkeys: product.evidence.winning_pubkeys,
            relay_rows: product.evidence.rows.len(),
            evidence_sha256: &product.evidence_sha256,
            current_pointer_changed: false,
        })?
    );
    Ok(())
}
