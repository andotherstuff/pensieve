//! Advance one DuckDB/Postgres analytics run from a verified object delta.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use pensieve_analytics::{
    BatchLimits, BuildConfig, CatalogDeltaPlan, IDENTITY_QUERY_VERSION, PubkeyFirstSeenConfig,
    PublishOutcome, acquire_publication_lock, advance_bounded_pubkey_first_seen, apply_incremental,
    load_bounded_pubkey_first_seen, plan_catalog_delta_for_query_version, publish_incremental,
    publish_incremental_with_identity, resolve_delta_locations, resolve_snapshot,
};
use postgres::{Config as PostgresConfig, NoTls};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Apply one exact append-only analytics delta")]
struct Args {
    /// Canonically encoded target active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Persisted planner output that was used for staging.
    #[arg(long)]
    plan: PathBuf,
    /// Existing persistent DuckDB checkpoint.
    #[arg(long)]
    work_database: PathBuf,
    /// Root containing only the plan's verified added objects.
    #[arg(long)]
    delta_object_root: PathBuf,
    /// Run exact delta scans and joins, then roll back without publishing.
    #[arg(long)]
    dry_run: bool,
    /// Fixed Unix timestamp for reproducible rolling metrics.
    #[arg(long)]
    as_of: u64,
    /// Build/commit identity stored in analytics run metadata.
    #[arg(
        long,
        env = "PENSIEVE_ANALYTICS_CODE_VERSION",
        default_value = concat!("pensieve-analytics/", env!("CARGO_PKG_VERSION"))
    )]
    code_version: String,
    /// Postgres connection string for planning and atomic publication.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: String,
    /// Postgres password supplied separately from the connection string.
    #[arg(long, env = "POSTGRES_ANALYTICS_PASSWORD")]
    postgres_password: Option<String>,
    /// DuckDB buffer-manager limit.
    #[arg(
        long,
        env = "PENSIEVE_ANALYTICS_DUCKDB_MEMORY_LIMIT",
        default_value = "16GB"
    )]
    duckdb_memory_limit: String,
    /// DuckDB worker threads.
    #[arg(long, env = "PENSIEVE_ANALYTICS_DUCKDB_THREADS", default_value_t = 2)]
    duckdb_threads: usize,
    /// Current immutable first-seen evidence; enables Slice B1 publication.
    #[arg(long, requires_all = ["identity_evidence", "identity_work_root"])]
    identity_baseline_evidence: Option<PathBuf>,
    /// Immutable first-seen successor evidence output.
    #[arg(long)]
    identity_evidence: Option<PathBuf>,
    /// Dedicated immutable first-seen successor workspace.
    #[arg(long)]
    identity_work_root: Option<PathBuf>,
    /// Maximum compressed delta bytes in one first-seen scan.
    #[arg(long, default_value_t = 1_073_741_824)]
    identity_batch_bytes: u64,
    /// Maximum physical delta rows in one first-seen scan.
    #[arg(long, default_value_t = 5_000_000)]
    identity_batch_rows: u64,
    /// Maximum first-seen runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    identity_merge_fan_in: usize,
    /// Free first-seen work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 53_687_091_200)]
    identity_disk_reserve_bytes: u64,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    previous_run_id: &'a str,
    as_of_epoch: u64,
    dry_run: bool,
    incremental: &'a pensieve_analytics::IncrementalSummary,
    build: &'a pensieve_analytics::BuildSummary,
    identity: Option<IdentityOutput<'a>>,
    publication: Option<PublicationOutput>,
}

#[derive(Serialize)]
struct IdentityOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: Option<&'a str>,
    first_seen_records: u64,
    eligible_pubkeys: u64,
    new_users_daily_rows: usize,
    delta_object_count: u64,
    max_merge_buffered_bytes: usize,
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
        eprintln!("incremental analytics failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let started_at = Utc::now();
    let as_of_epoch = args.as_of;
    let persisted_plan: CatalogDeltaPlan =
        serde_json::from_slice(&std::fs::read(&args.plan).context("read persisted delta plan")?)
            .context("decode persisted delta plan")?;
    let target = resolve_snapshot(&args.catalog, None).context("resolve target snapshot")?;
    let delta_locations = resolve_delta_locations(&persisted_plan, &args.delta_object_root)
        .context("resolve staged delta objects")?;

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
    let identity_enabled = args.identity_baseline_evidence.is_some();
    let desired_query_version = if identity_enabled {
        IDENTITY_QUERY_VERSION
    } else {
        pensieve_analytics::QUERY_VERSION
    };
    let live_plan =
        plan_catalog_delta_for_query_version(&mut client, &target.catalog, desired_query_version)
            .context("re-plan target against current Postgres run")?;
    if live_plan != persisted_plan && !target_is_already_current(&live_plan, &persisted_plan) {
        bail!("persisted staging plan no longer matches the live catalog plan");
    }
    let previous_run_id = persisted_plan
        .previous_run_id
        .as_deref()
        .expect("incremental plan validation requires a previous run");
    let baseline_as_of_epoch: i64 = client
        .query_one(
            "SELECT as_of_epoch FROM pensieve_analytics.runs WHERE run_id = $1",
            &[&previous_run_id],
        )
        .context("read baseline run time")?
        .get(0);
    let baseline_as_of_epoch =
        u64::try_from(baseline_as_of_epoch).context("baseline as_of_epoch must be non-negative")?;
    let config = BuildConfig {
        as_of_epoch,
        code_version: args.code_version,
        s3_region: String::new(),
        s3_force_path_style: false,
        memory_limit: args.duckdb_memory_limit,
        threads: args.duckdb_threads,
    };
    let (build, incremental) = apply_incremental(
        &args.work_database,
        target.clone(),
        &persisted_plan,
        &delta_locations,
        config,
        baseline_as_of_epoch,
        args.dry_run,
    )
    .context("advance DuckDB checkpoint")?;
    let identity = if let (Some(baseline_path), Some(evidence_path), Some(work_root)) = (
        args.identity_baseline_evidence.as_ref(),
        args.identity_evidence.as_ref(),
        args.identity_work_root.as_ref(),
    ) {
        let baseline = load_bounded_pubkey_first_seen(baseline_path)
            .context("load baseline first-seen evidence")?;
        Some(
            advance_bounded_pubkey_first_seen(
                evidence_path,
                &baseline,
                target,
                &persisted_plan,
                &delta_locations,
                build.config.clone(),
                PubkeyFirstSeenConfig {
                    work_root: work_root.clone(),
                    batch_limits: BatchLimits {
                        max_bytes: args.identity_batch_bytes,
                        max_rows: args.identity_batch_rows,
                    },
                    merge_fan_in: args.identity_merge_fan_in,
                    disk_reserve_bytes: args.identity_disk_reserve_bytes,
                },
            )
            .context("advance bounded first-seen state")?,
        )
    } else {
        None
    };
    let completed_at = Utc::now();
    let publication = if args.dry_run {
        None
    } else {
        Some(
            match if let Some(identity) = identity.as_ref() {
                publish_incremental_with_identity(
                    &mut client,
                    &build,
                    identity,
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else {
                publish_incremental(
                    &mut client,
                    &build,
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            }? {
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
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &build.snapshot.catalog.snapshot_id,
            previous_run_id,
            as_of_epoch,
            dry_run: args.dry_run,
            incremental: &incremental,
            build: &build.summary,
            identity: identity.as_ref().map(|identity| IdentityOutput {
                evidence_sha256: &identity.evidence_sha256,
                baseline_evidence_sha256: identity.evidence.baseline_evidence_sha256.as_deref(),
                first_seen_records: identity.evidence.first_seen_records,
                eligible_pubkeys: identity.evidence.eligible_pubkeys,
                new_users_daily_rows: identity.evidence.new_users_daily.len(),
                delta_object_count: identity.evidence.delta_object_count,
                max_merge_buffered_bytes: identity.evidence.max_merge_buffered_bytes,
            }),
            publication,
        })?
    );
    Ok(())
}

fn target_is_already_current(
    live_plan: &CatalogDeltaPlan,
    persisted_plan: &CatalogDeltaPlan,
) -> bool {
    live_plan.previous_snapshot_id.as_deref() == Some(persisted_plan.snapshot_id.as_str())
        && live_plan.snapshot_id == persisted_plan.snapshot_id
        && live_plan.added_objects.is_empty()
        && live_plan.removed_objects.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pensieve_analytics::PlannedRunKind;

    fn plan(
        snapshot_id: &str,
        previous_snapshot_id: Option<&str>,
        added_objects: usize,
    ) -> CatalogDeltaPlan {
        CatalogDeltaPlan {
            snapshot_id: snapshot_id.to_owned(),
            previous_run_id: Some("run".to_owned()),
            previous_snapshot_id: previous_snapshot_id.map(str::to_owned),
            run_kind: PlannedRunKind::Incremental,
            added_objects: (0..added_objects)
                .map(|index| pensieve_lake::CatalogObject {
                    object_key: format!("object-{index}"),
                    work_unit_id: format!("work-{index}"),
                    part_number: 0,
                    byte_size: 1,
                    sha256: "0".repeat(64),
                    writer_version: "test".to_owned(),
                    row_count: 1,
                    min_created_at: Some("1".to_owned()),
                    max_created_at: Some("1".to_owned()),
                })
                .collect(),
            removed_objects: Vec::new(),
            unchanged_objects: 0,
            added_bytes: added_objects as u64,
            added_physical_rows: added_objects as u64,
            affected_min_created_at: None,
            affected_max_created_at: None,
            affected_range_complete: true,
        }
    }

    #[test]
    fn already_current_target_accepts_the_original_persisted_plan() {
        let persisted = plan("sha256:target", Some("sha256:baseline"), 1);
        let live = plan("sha256:target", Some("sha256:target"), 0);

        assert!(target_is_already_current(&live, &persisted));
    }

    #[test]
    fn different_or_partially_applied_live_target_is_not_a_retry() {
        let persisted = plan("sha256:target", Some("sha256:baseline"), 1);
        let different = plan("sha256:other", Some("sha256:other"), 0);
        let partial = plan("sha256:target", Some("sha256:target"), 1);

        assert!(!target_is_already_current(&different, &persisted));
        assert!(!target_is_already_current(&partial, &persisted));
    }
}
