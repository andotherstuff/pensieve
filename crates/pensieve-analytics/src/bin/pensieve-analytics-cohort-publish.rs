//! Build exact cohort retention and atomically upgrade one Slice B2 publication.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use pensieve_analytics::{
    AllBoundedProducts, AnalyticsBuild, FIXED_ACTIVITY_QUERY_VERSION, PublishOutcome,
    acquire_publication_lock, build_bounded_cohort_retention, load_bounded_fixed_activity,
    load_bounded_pubkey_first_seen, publish_incremental_with_all_bounded_products,
    resolve_snapshot,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Atomically publish exact cohort retention from an authorized baseline")]
struct Args {
    /// Canonical active-file snapshot matching the current authorized run.
    #[arg(long)]
    catalog: PathBuf,
    /// Verified local object root; omit when the DuckDB checkpoint is complete.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Existing completed Slice A DuckDB checkpoint.
    #[arg(long)]
    work_database: PathBuf,
    /// Verified first-seen evidence matching the current run.
    #[arg(long)]
    identity_evidence: PathBuf,
    /// Verified fixed-activity evidence matching the target semantics.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Canonical immutable cohort-retention evidence to create.
    #[arg(long)]
    cohort_evidence: PathBuf,
    /// Fixed as-of from the current run.
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
    /// Exact query version required on the current Postgres baseline.
    ///
    /// The default is the normal B2-to-B3 publication path. A deliberate
    /// semantic republish may name an older B3 version explicitly while still
    /// requiring the exact snapshot, as-of, and locked run identity.
    #[arg(long, default_value = FIXED_ACTIVITY_QUERY_VERSION)]
    expected_current_query_version: String,
    /// Hard ceiling for compact cohort-retention matrix rows.
    #[arg(long, default_value_t = 2_000_000)]
    matrix_row_limit: usize,
    /// Build and validate evidence without changing Postgres.
    #[arg(long)]
    dry_run: bool,
    /// DuckDB memory setting used to open the completed Slice A checkpoint.
    #[arg(long, default_value = "4GB")]
    memory_limit: String,
    /// DuckDB workers used to open the completed Slice A checkpoint.
    #[arg(long, default_value_t = 1)]
    threads: usize,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    previous_run_id: String,
    previous_query_version: String,
    run_id: Option<String>,
    publication_status: &'static str,
    dry_run: bool,
    cohort_retention_rows: u64,
    active_pubkeys_sum: u64,
    matrix_row_limit: usize,
    max_pubkey_periods_buffered: usize,
    cohort_evidence_sha256: &'a str,
    cohort_metric_sha256: &'a str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("cohort publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let started_at = Utc::now();
    // Fail fast on missing or invalid credentials before revalidating the
    // multi-billion-row immutable activity inputs. The publication connection
    // is opened again after the bounded build so it cannot go stale while the
    // evidence pass runs.
    connect_postgres(&args)
        .context("preflight Postgres connection")?
        .simple_query("SELECT 1")
        .context("preflight Postgres query")?;
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
    let build = AnalyticsBuild::open_completed(
        &args.work_database,
        snapshot,
        pensieve_analytics::BuildConfig {
            as_of_epoch: args.as_of,
            code_version: args.code_version.clone(),
            s3_region: "us-east-1".to_owned(),
            s3_force_path_style: false,
            memory_limit: args.memory_limit.clone(),
            threads: args.threads,
        },
    )
    .context("open completed Slice A checkpoint")?;
    let identity = load_bounded_pubkey_first_seen(&args.identity_evidence)
        .context("load bounded first-seen evidence")?;
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load bounded fixed-activity evidence")?;
    let cohort = build_bounded_cohort_retention(
        &args.cohort_evidence,
        &identity,
        &activity,
        args.matrix_row_limit,
    )
    .context("build bounded cohort retention")?;

    let mut client = connect_postgres(&args).context("connect to Postgres for publication")?;
    acquire_publication_lock(&mut client).context("acquire analytics publication lock")?;
    let current = client
        .query_one(
            "SELECT run_id, snapshot_id, query_version, as_of_epoch FROM pensieve_analytics.current_run_metadata",
            &[],
        )
        .context("load current authorized baseline")?;
    let previous_run_id: String = current.get(0);
    let current_snapshot: String = current.get(1);
    let current_query_version: String = current.get(2);
    let current_as_of: i64 = current.get(3);
    validate_current_baseline(
        &current_snapshot,
        &current_query_version,
        current_as_of,
        &build.snapshot.catalog.snapshot_id,
        &args.expected_current_query_version,
        args.as_of,
    )?;
    let completed_at = Utc::now();
    let (run_id, publication_status) = if args.dry_run {
        (None, "not_published")
    } else {
        let outcome = publish_incremental_with_all_bounded_products(
            &mut client,
            &build,
            AllBoundedProducts {
                identity: &identity,
                activity: &activity,
                cohort: &cohort,
            },
            &previous_run_id,
            started_at,
            completed_at,
        )
        .context("atomically publish cohort-retention products")?;
        match outcome {
            PublishOutcome::Published { run_id, .. } => (Some(run_id), "published"),
            PublishOutcome::AlreadyCurrent { run_id } => (Some(run_id), "already_current"),
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &build.snapshot.catalog.snapshot_id,
            as_of_epoch: build.config.as_of_epoch,
            previous_run_id,
            previous_query_version: current_query_version,
            run_id,
            publication_status,
            dry_run: args.dry_run,
            cohort_retention_rows: cohort.evidence.period_rows,
            active_pubkeys_sum: cohort.evidence.active_pubkeys_sum,
            matrix_row_limit: cohort.evidence.matrix_row_limit,
            max_pubkey_periods_buffered: cohort.evidence.max_pubkey_periods_buffered,
            cohort_evidence_sha256: &cohort.evidence_sha256,
            cohort_metric_sha256: &cohort.evidence.metric_sha256,
        })?
    );
    Ok(())
}

fn connect_postgres(args: &Args) -> Result<postgres::Client> {
    let mut postgres_config: PostgresConfig = args
        .postgres_url
        .parse()
        .context("parse Postgres connection")?;
    if let Some(password) = &args.postgres_password {
        postgres_config.password(password);
    }
    postgres_config
        .connect(NoTls)
        .context("connect to Postgres without TLS")
}

fn validate_current_baseline(
    current_snapshot: &str,
    current_query_version: &str,
    current_as_of: i64,
    expected_snapshot: &str,
    expected_query_version: &str,
    expected_as_of: u64,
) -> Result<()> {
    if current_snapshot != expected_snapshot
        || current_query_version != expected_query_version
        || current_as_of != i64::try_from(expected_as_of).context("as-of exceeds i64")?
    {
        bail!(
            "current Postgres run is not the exact authorized snapshot/as-of/query-version baseline"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_republish_requires_the_exact_explicit_baseline() {
        assert!(validate_current_baseline("s", "slice-b3-v1", 10, "s", "slice-b3-v1", 10).is_ok());
        assert!(
            validate_current_baseline("other", "slice-b3-v1", 10, "s", "slice-b3-v1", 10).is_err()
        );
        assert!(validate_current_baseline("s", "slice-b3-v2", 10, "s", "slice-b3-v1", 10).is_err());
        assert!(validate_current_baseline("s", "slice-b3-v1", 11, "s", "slice-b3-v1", 10).is_err());
    }
}
