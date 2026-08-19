//! Build bounded first-seen state and atomically upgrade one Slice A publication.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use pensieve_analytics::{
    AnalyticsBuild, BatchLimits, BuildConfig, PubkeyFirstSeenConfig, PublishOutcome,
    acquire_publication_lock, build_bounded_pubkey_first_seen, publish_incremental_with_identity,
    resolve_snapshot,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Atomically add bounded exact identity products to a Slice A publication")]
struct Args {
    /// Canonical active-file snapshot matching the current Slice A run.
    #[arg(long)]
    catalog: PathBuf,
    /// Verified local object root used for the initial bounded scan.
    #[arg(long)]
    local_object_root: PathBuf,
    /// Existing completed Slice A DuckDB checkpoint.
    #[arg(long)]
    work_database: PathBuf,
    /// Dedicated immutable first-seen batch and merge workspace.
    #[arg(long)]
    identity_work_root: PathBuf,
    /// Immutable first-seen completion evidence.
    #[arg(long)]
    identity_evidence: PathBuf,
    /// Fixed as-of from the current Slice A run.
    #[arg(long)]
    as_of: u64,
    /// Build/commit identity stored in analytics run metadata.
    #[arg(long)]
    code_version: String,
    /// Postgres connection string for atomic publication.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: String,
    /// Postgres password supplied separately from the connection string.
    #[arg(long, env = "POSTGRES_ANALYTICS_PASSWORD")]
    postgres_password: Option<String>,
    /// DuckDB memory limit for each bounded input scan.
    #[arg(long, default_value = "4GB")]
    memory_limit: String,
    /// DuckDB workers for each bounded input scan.
    #[arg(long, default_value_t = 1)]
    threads: usize,
    /// Maximum compressed catalog bytes in one scan.
    #[arg(long, default_value_t = 1_073_741_824)]
    batch_bytes: u64,
    /// Maximum physical catalog rows in one scan.
    #[arg(long, default_value_t = 5_000_000)]
    batch_rows: u64,
    /// Maximum immutable runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    merge_fan_in: usize,
    /// Free work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 53_687_091_200)]
    disk_reserve_bytes: u64,
}

#[derive(Serialize)]
struct Output {
    snapshot_id: String,
    previous_run_id: String,
    run_id: String,
    publication_status: &'static str,
    eligible_pubkeys: u64,
    new_users_daily_rows: usize,
    identity_evidence_sha256: String,
    identity_artifact_sha256: String,
    max_merge_buffered_bytes: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("identity publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let started_at = Utc::now();
    let snapshot = resolve_snapshot(&args.catalog, Some(&args.local_object_root))
        .context("resolve verified local snapshot")?;
    let build_config = BuildConfig {
        as_of_epoch: args.as_of,
        code_version: args.code_version,
        s3_region: String::new(),
        s3_force_path_style: false,
        memory_limit: args.memory_limit,
        threads: args.threads,
    };
    let build =
        AnalyticsBuild::open_completed(&args.work_database, snapshot.clone(), build_config.clone())
            .context("open completed Slice A checkpoint")?;
    let identity = build_bounded_pubkey_first_seen(
        &args.identity_evidence,
        snapshot,
        build_config,
        PubkeyFirstSeenConfig {
            work_root: args.identity_work_root,
            batch_limits: BatchLimits {
                max_bytes: args.batch_bytes,
                max_rows: args.batch_rows,
            },
            merge_fan_in: args.merge_fan_in,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build bounded first-seen state")?;

    let mut postgres_config: PostgresConfig = args
        .postgres_url
        .parse()
        .context("parse Postgres connection")?;
    if let Some(password) = args.postgres_password {
        postgres_config.password(password);
    }
    let mut client = postgres_config
        .connect(NoTls)
        .context("connect to Postgres without TLS")?;
    acquire_publication_lock(&mut client).context("acquire analytics publication lock")?;
    let current = client
        .query_one(
            "SELECT run_id, snapshot_id, query_version, as_of_epoch FROM pensieve_analytics.current_run_metadata",
            &[],
        )
        .context("load current Slice A baseline")?;
    let previous_run_id: String = current.get(0);
    let current_snapshot: String = current.get(1);
    let current_query_version: String = current.get(2);
    let current_as_of: i64 = current.get(3);
    if current_snapshot != build.snapshot.catalog.snapshot_id
        || current_query_version != pensieve_analytics::QUERY_VERSION
        || current_as_of != i64::try_from(args.as_of).context("as-of exceeds i64")?
    {
        bail!(
            "current Postgres run is not the exact Slice A snapshot/as-of selected for identity upgrade"
        );
    }
    let completed_at = Utc::now();
    let outcome = publish_incremental_with_identity(
        &mut client,
        &build,
        &identity,
        &previous_run_id,
        started_at,
        completed_at,
    )
    .context("atomically publish identity products")?;
    let (run_id, publication_status) = match outcome {
        PublishOutcome::Published { run_id, .. } => (run_id, "published"),
        PublishOutcome::AlreadyCurrent { run_id } => (run_id, "already_current"),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: build.snapshot.catalog.snapshot_id.clone(),
            previous_run_id,
            run_id,
            publication_status,
            eligible_pubkeys: identity.evidence.eligible_pubkeys,
            new_users_daily_rows: identity.evidence.new_users_daily.len(),
            identity_evidence_sha256: identity.evidence_sha256,
            identity_artifact_sha256: identity.evidence.final_artifact.sha256,
            max_merge_buffered_bytes: identity.evidence.max_merge_buffered_bytes,
        })?
    );
    Ok(())
}
