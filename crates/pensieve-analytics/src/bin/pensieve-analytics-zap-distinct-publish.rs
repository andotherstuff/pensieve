//! Atomically publish dormant Slice 7 zap sketches without moving pointers.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    ZapDistinctPublishOutcome, load_bounded_semantic_facts, load_bounded_zap_distinct,
    publish_zap_distinct,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Publish versioned Slice 7 zap sketches without changing current_run")]
struct Args {
    /// Validated semantic completion evidence.
    #[arg(long)]
    semantic_evidence: PathBuf,
    /// Final semantic fact artifact.
    #[arg(long)]
    semantic_artifact: PathBuf,
    /// Authorized semantic evidence SHA-256.
    #[arg(long)]
    semantic_evidence_sha256: String,
    /// Validated zap-distinct completion evidence.
    #[arg(long)]
    zap_evidence: PathBuf,
    /// Final exact zap participant identity artifact.
    #[arg(long)]
    zap_identity_artifact: PathBuf,
    /// Authorized zap-distinct evidence SHA-256.
    #[arg(long)]
    zap_evidence_sha256: String,
    /// Already-published dormant semantic product owning these sketches.
    #[arg(long)]
    semantic_product_id: String,
    /// Postgres connection string for atomic publication.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: String,
    /// Postgres password supplied separately from the connection string.
    #[arg(long, env = "POSTGRES_ANALYTICS_PASSWORD")]
    postgres_password: Option<String>,
    /// Validate artifacts and database baseline without writing rows.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    semantic_product_id: &'a str,
    product_id: Option<String>,
    publication_status: &'static str,
    dry_run: bool,
    logical_identities: u64,
    leaf_rows: usize,
    evidence_sha256: &'a str,
    current_pointer_changed: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zap distinct publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    connect_postgres(&args)
        .context("preflight Postgres connection")?
        .simple_query("SELECT 1")
        .context("preflight Postgres query")?;
    let semantic = load_bounded_semantic_facts(&args.semantic_evidence, &args.semantic_artifact)
        .context("load semantic product")?;
    if semantic.evidence_sha256 != args.semantic_evidence_sha256 {
        bail!("semantic evidence SHA-256 differs from the authorized gate");
    }
    let product =
        load_bounded_zap_distinct(&args.zap_evidence, &args.zap_identity_artifact, &semantic)
            .context("load zap distinct product")?;
    if product.evidence_sha256 != args.zap_evidence_sha256 {
        bail!("zap distinct evidence SHA-256 differs from the authorized gate");
    }

    let mut client = connect_postgres(&args).context("connect to Postgres for publication")?;
    let current_before: String = client
        .query_one(
            "SELECT run_id FROM pensieve_analytics.current_run WHERE singleton=true",
            &[],
        )?
        .get(0);
    let (product_id, publication_status) = if args.dry_run {
        let row = client.query_one(
            "SELECT run_id,snapshot_id,as_of_epoch,evidence_sha256
               FROM pensieve_analytics.semantic_products WHERE product_id=$1",
            &[&args.semantic_product_id],
        )?;
        if row.get::<_, String>(0) != current_before
            || row.get::<_, String>(1) != product.evidence.snapshot_id
            || row.get::<_, i64>(2) != i64::try_from(product.evidence.as_of_epoch)?
            || row.get::<_, String>(3) != product.evidence.semantic_evidence_sha256
        {
            bail!("semantic product is not the exact current Slice 7 baseline");
        }
        (None, "not_published")
    } else {
        match publish_zap_distinct(&mut client, &args.semantic_product_id, &semantic, &product)
            .context("atomically publish dormant zap distinct leaves")?
        {
            ZapDistinctPublishOutcome::Published { product_id } => (Some(product_id), "published"),
            ZapDistinctPublishOutcome::AlreadyPublished { product_id } => {
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
    if current_after != current_before {
        bail!("analytics current pointer changed during dormant zap publication");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &product.evidence.snapshot_id,
            as_of_epoch: product.evidence.as_of_epoch,
            semantic_product_id: &args.semantic_product_id,
            product_id,
            publication_status,
            dry_run: args.dry_run,
            logical_identities: product.evidence.logical_identities,
            leaf_rows: product.evidence.leaves.len(),
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
