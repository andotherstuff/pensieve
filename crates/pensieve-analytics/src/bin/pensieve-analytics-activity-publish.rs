//! Build bounded fixed-activity state and atomically upgrade one Slice B1 publication.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use pensieve_analytics::{
    AnalyticsBuild, BatchLimits, BuildConfig, FixedActivityConfig, IDENTITY_QUERY_VERSION,
    PublishOutcome, acquire_publication_lock, build_bounded_fixed_activity,
    load_bounded_pubkey_first_seen, publish_incremental_with_identity_and_activity,
    resolve_snapshot,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Atomically add bounded exact activity products to a Slice B1 publication")]
struct Args {
    /// Canonical active-file snapshot matching the current Slice B1 run.
    #[arg(long)]
    catalog: PathBuf,
    /// Verified local object root; omit to read immutable objects from the catalog store.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Existing completed Slice A DuckDB checkpoint.
    #[arg(long)]
    work_database: PathBuf,
    /// Verified first-seen evidence matching the current Slice B1 run.
    #[arg(long)]
    identity_evidence: PathBuf,
    /// Dedicated immutable fixed-activity batch and merge workspace.
    #[arg(long)]
    activity_work_root: PathBuf,
    /// Immutable fixed-activity completion evidence.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Fixed as-of from the current Slice B1 run.
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
    /// Build and validate immutable activity evidence without changing Postgres.
    #[arg(long)]
    dry_run: bool,
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
    run_id: Option<String>,
    publication_status: &'static str,
    dry_run: bool,
    activity_records: u64,
    pubkey_flag_records: u64,
    distinct_period_rows: u64,
    active_period_rows: u64,
    activity_evidence_sha256: String,
    activity_artifact_sha256: String,
    flags_artifact_sha256: String,
    max_merge_buffered_bytes: usize,
    max_week_kinds_buffered: usize,
    max_month_kinds_buffered: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("activity publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let started_at = Utc::now();
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
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
    let identity = load_bounded_pubkey_first_seen(&args.identity_evidence)
        .context("load bounded first-seen evidence")?;
    identity
        .validate_for_publication(&snapshot.catalog.snapshot_id, args.as_of)
        .context("validate first-seen evidence")?;
    let activity = build_bounded_fixed_activity(
        &args.activity_evidence,
        snapshot,
        build_config,
        FixedActivityConfig {
            work_root: args.activity_work_root,
            batch_limits: BatchLimits {
                max_bytes: args.batch_bytes,
                max_rows: args.batch_rows,
            },
            merge_fan_in: args.merge_fan_in,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build bounded fixed-activity state")?;

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
        .context("load current Slice B1 baseline")?;
    let previous_run_id: String = current.get(0);
    let current_snapshot: String = current.get(1);
    let current_query_version: String = current.get(2);
    let current_as_of: i64 = current.get(3);
    if current_snapshot != build.snapshot.catalog.snapshot_id
        || current_query_version != IDENTITY_QUERY_VERSION
        || current_as_of != i64::try_from(args.as_of).context("as-of exceeds i64")?
    {
        bail!(
            "current Postgres run is not the exact Slice B1 snapshot/as-of selected for activity upgrade"
        );
    }
    let completed_at = Utc::now();
    let (run_id, publication_status) = if args.dry_run {
        (None, "not_published")
    } else {
        let outcome = publish_incremental_with_identity_and_activity(
            &mut client,
            &build,
            &identity,
            &activity,
            &previous_run_id,
            started_at,
            completed_at,
        )
        .context("atomically publish fixed-activity products")?;
        match outcome {
            PublishOutcome::Published { run_id, .. } => (Some(run_id), "published"),
            PublishOutcome::AlreadyCurrent { run_id } => (Some(run_id), "already_current"),
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: build.snapshot.catalog.snapshot_id.clone(),
            previous_run_id,
            run_id,
            publication_status,
            dry_run: args.dry_run,
            activity_records: activity.evidence.activity_artifact.row_count,
            pubkey_flag_records: activity.evidence.flags_artifact.row_count,
            distinct_period_rows: activity.evidence.distinct_period_rows,
            active_period_rows: activity.evidence.active_period_rows,
            activity_evidence_sha256: activity.evidence_sha256,
            activity_artifact_sha256: activity.evidence.activity_artifact.sha256,
            flags_artifact_sha256: activity.evidence.flags_artifact.sha256,
            max_merge_buffered_bytes: activity.evidence.max_merge_buffered_bytes,
            max_week_kinds_buffered: activity.evidence.max_week_kinds_buffered,
            max_month_kinds_buffered: activity.evidence.max_month_kinds_buffered,
        })?
    );
    Ok(())
}
