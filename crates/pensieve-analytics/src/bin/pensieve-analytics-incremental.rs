//! Advance one DuckDB/Postgres analytics run from a verified object delta.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use pensieve_analytics::{
    AllBoundedProducts, AllRecurringProducts, AllRecurringProductsWithPublisher,
    AllRecurringProductsWithRelay, AllRecurringProductsWithServing, BatchLimits, BuildConfig,
    COHORT_RETENTION_QUERY_VERSION, CatalogDeltaPlan, CohortRetentionEvidence,
    FIXED_ACTIVITY_QUERY_VERSION, FLEXIBLE_DISTINCT_TOLERANCE_PPM, FixedActivityConfig,
    FlexibleDistinctConfig, FlexibleDistinctPublication, IDENTITY_QUERY_VERSION,
    PubkeyFirstSeenConfig, PublishOutcome, PublisherRankingConfig, RelayDistributionConfig,
    SemanticFactsConfig, SemanticPublication, ServingFactsConfig, ZapDistinctConfig,
    acquire_publication_lock, advance_bounded_fixed_activity, advance_bounded_flexible_distinct,
    advance_bounded_pubkey_first_seen, advance_bounded_publisher_ranking,
    advance_bounded_relay_distribution, advance_bounded_semantic_facts,
    advance_bounded_serving_facts, apply_incremental, build_bounded_cohort_retention,
    build_bounded_zap_distinct, build_flexible_distinct_validation, load_bounded_fixed_activity,
    load_bounded_flexible_distinct, load_bounded_pubkey_first_seen, load_bounded_publisher_ranking,
    load_bounded_relay_distribution_for_advance, load_bounded_semantic_facts,
    load_bounded_serving_facts, plan_catalog_delta_for_query_version, publish_incremental,
    publish_incremental_with_all_bounded_products,
    publish_incremental_with_all_bounded_products_and_flexible,
    publish_incremental_with_all_bounded_products_flexible_and_semantic,
    publish_incremental_with_all_recurring_products_and_publisher,
    publish_incremental_with_all_recurring_products_and_relay,
    publish_incremental_with_all_recurring_products_and_serving, publish_incremental_with_identity,
    publish_incremental_with_identity_and_activity, resolve_delta_locations, resolve_snapshot,
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
    /// Current immutable fixed-activity evidence; enables Slice B2 publication.
    #[arg(long)]
    activity_baseline_evidence: Option<PathBuf>,
    /// Immutable fixed-activity successor evidence output.
    #[arg(long)]
    activity_evidence: Option<PathBuf>,
    /// Dedicated immutable fixed-activity successor workspace.
    #[arg(long)]
    activity_work_root: Option<PathBuf>,
    /// Maximum compressed delta bytes in one fixed-activity scan.
    #[arg(long, default_value_t = 1_073_741_824)]
    activity_batch_bytes: u64,
    /// Maximum physical delta rows in one fixed-activity scan.
    #[arg(long, default_value_t = 5_000_000)]
    activity_batch_rows: u64,
    /// Maximum fixed-activity runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    activity_merge_fan_in: usize,
    /// Free fixed-activity work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 53_687_091_200)]
    activity_disk_reserve_bytes: u64,
    /// Current immutable cohort evidence; enables Slice B3 publication.
    #[arg(long)]
    cohort_baseline_evidence: Option<PathBuf>,
    /// Immutable cohort-retention successor evidence output.
    #[arg(long)]
    cohort_evidence: Option<PathBuf>,
    /// Hard ceiling for compact cohort-retention matrix rows.
    #[arg(long, default_value_t = 2_000_000)]
    cohort_matrix_row_limit: usize,
    /// Current immutable flexible-distinct evidence; enables recurring Slice 6.
    #[arg(long)]
    flexible_baseline_evidence: Option<PathBuf>,
    /// Immutable flexible-distinct successor evidence output.
    #[arg(long)]
    flexible_evidence: Option<PathBuf>,
    /// Dedicated immutable flexible-distinct successor workspace.
    #[arg(long)]
    flexible_work_root: Option<PathBuf>,
    /// Canonical exact-versus-estimated tolerance evidence output.
    #[arg(long)]
    flexible_validation_evidence: Option<PathBuf>,
    /// Exact activity records transformed by one bounded Slice 6 batch.
    #[arg(long, default_value_t = 1_000_000)]
    flexible_source_records_per_batch: u64,
    /// Maximum Slice 6 immutable runs opened by one merge.
    #[arg(long, default_value_t = 16)]
    flexible_merge_fan_in: usize,
    /// Free Slice 6 work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    flexible_disk_reserve_bytes: u64,
    /// Maximum accepted representative Slice 6 relative error in ppm.
    #[arg(long, default_value_t = FLEXIBLE_DISTINCT_TOLERANCE_PPM)]
    flexible_tolerance_ppm: u64,
    /// Current immutable semantic evidence; enables recurring Slice 7.
    #[arg(long)]
    semantic_baseline_evidence: Option<PathBuf>,
    /// Current immutable semantic fact artifact named by baseline evidence.
    #[arg(long)]
    semantic_baseline_artifact: Option<PathBuf>,
    /// Immutable semantic successor evidence output.
    #[arg(long)]
    semantic_evidence: Option<PathBuf>,
    /// Dedicated immutable semantic successor workspace.
    #[arg(long)]
    semantic_work_root: Option<PathBuf>,
    /// Immutable zap-distinct successor evidence output.
    #[arg(long)]
    zap_distinct_evidence: Option<PathBuf>,
    /// Dedicated immutable zap-distinct successor workspace.
    #[arg(long)]
    zap_distinct_work_root: Option<PathBuf>,
    /// Maximum compressed delta bytes in one semantic scan.
    #[arg(long, default_value_t = 1_073_741_824)]
    semantic_batch_bytes: u64,
    /// Maximum physical delta rows in one semantic scan.
    #[arg(long, default_value_t = 5_000_000)]
    semantic_batch_rows: u64,
    /// Maximum semantic runs opened by one merge.
    #[arg(long, default_value_t = 16)]
    semantic_merge_fan_in: usize,
    /// Free semantic work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    semantic_disk_reserve_bytes: u64,
    /// Maximum zap identities held by one sorted chunk.
    #[arg(long, default_value_t = 1_000_000)]
    zap_distinct_chunk_records: usize,
    /// Maximum zap identity runs opened by one merge.
    #[arg(long, default_value_t = 16)]
    zap_distinct_merge_fan_in: usize,
    /// Free zap work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    zap_distinct_disk_reserve_bytes: u64,
    /// Current immutable relay-distribution evidence; enables recurring Slice 8.
    #[arg(long)]
    relay_baseline_evidence: Option<PathBuf>,
    /// Durable append-only relay candidate ledger shared across generations.
    #[arg(long)]
    relay_state_database: Option<PathBuf>,
    /// Immutable relay-distribution successor evidence output.
    #[arg(long)]
    relay_evidence: Option<PathBuf>,
    /// Maximum compressed object bytes in one relay scan.
    #[arg(long, default_value_t = 1_073_741_824)]
    relay_batch_bytes: u64,
    /// Maximum physical rows in one relay scan.
    #[arg(long, default_value_t = 5_000_000)]
    relay_batch_rows: u64,
    /// Hard ceiling for the durable relay candidate ledger.
    #[arg(long, default_value_t = 53_687_091_200)]
    relay_max_state_bytes: u64,
    /// SQLite page-cache bound for relay candidate state.
    #[arg(long, default_value_t = 268_435_456)]
    relay_sqlite_cache_bytes: u64,
    /// Minimum winning pubkeys required for a served relay row.
    #[arg(long, default_value_t = 10)]
    relay_minimum_users: u64,
    /// Free relay-state filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    relay_disk_reserve_bytes: u64,
    /// Current immutable publisher evidence; enables recurring Slice 9.
    #[arg(long)]
    publisher_baseline_evidence: Option<PathBuf>,
    /// Current publisher ledger named by predecessor evidence.
    #[arg(long)]
    publisher_baseline_state_database: Option<PathBuf>,
    /// Target generation's durable publisher ledger.
    #[arg(long)]
    publisher_state_database: Option<PathBuf>,
    /// Target generation's canonical ranking artifact.
    #[arg(long)]
    publisher_artifact: Option<PathBuf>,
    /// Immutable publisher successor evidence output.
    #[arg(long)]
    publisher_evidence: Option<PathBuf>,
    /// Exact supported publisher windows.
    #[arg(long, value_delimiter = ',', default_value = "1,7,30,90,365")]
    publisher_windows_days: Vec<u32>,
    /// Maximum served publishers per exact window/filter.
    #[arg(long, default_value_t = 1_000)]
    publisher_top_limit: usize,
    /// Pubkeys committed in one publisher-ledger transaction.
    #[arg(long, default_value_t = 10_000)]
    publisher_batch_size: usize,
    /// Hard target publisher-ledger ceiling.
    #[arg(long, default_value_t = 536_870_912_000_u64)]
    publisher_max_state_bytes: u64,
    /// Fixed target publisher-ledger page cache.
    #[arg(long, default_value_t = 536_870_912_u64)]
    publisher_sqlite_cache_bytes: u64,
    /// Free publisher-ledger filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400_u64)]
    publisher_disk_reserve_bytes: u64,
    /// Current immutable serving-facts evidence; enables recurring Slice 9.5.
    #[arg(long)]
    serving_baseline_evidence: Option<PathBuf>,
    /// Immutable serving-facts successor evidence output.
    #[arg(long)]
    serving_evidence: Option<PathBuf>,
    /// Dedicated immutable serving-facts successor workspace.
    #[arg(long)]
    serving_work_root: Option<PathBuf>,
    /// Maximum compressed delta bytes in one serving-facts scan.
    #[arg(long, default_value_t = 1_073_741_824)]
    serving_batch_bytes: u64,
    /// Maximum physical delta rows in one serving-facts scan.
    #[arg(long, default_value_t = 5_000_000)]
    serving_batch_rows: u64,
    /// Maximum serving-facts runs opened by one merge.
    #[arg(long, default_value_t = 16)]
    serving_merge_fan_in: usize,
    /// Free serving-facts filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    serving_disk_reserve_bytes: u64,
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
    activity: Option<ActivityOutput<'a>>,
    cohort: Option<CohortOutput<'a>>,
    flexible: Option<FlexibleOutput<'a>>,
    semantic: Option<SemanticOutput<'a>>,
    relay: Option<RelayOutput<'a>>,
    publisher: Option<PublisherOutput<'a>>,
    serving: Option<ServingOutput<'a>>,
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
struct ActivityOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: Option<&'a str>,
    activity_records: u64,
    pubkey_flag_records: u64,
    distinct_period_rows: u64,
    active_period_rows: u64,
    delta_object_count: u64,
    max_merge_buffered_bytes: usize,
    max_week_kinds_buffered: usize,
    max_month_kinds_buffered: usize,
}

#[derive(Serialize)]
struct CohortOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: &'a str,
    period_rows: u64,
    active_pubkeys_sum: u64,
    metric_sha256: &'a str,
    matrix_row_limit: usize,
    max_pubkey_periods_buffered: usize,
}

#[derive(Serialize)]
struct FlexibleOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: &'a str,
    complete_through_epoch: u64,
    identity_rows: u64,
    leaf_rows: u64,
    validation_evidence_sha256: &'a str,
    max_relative_error_ppm: u64,
}

#[derive(Serialize)]
struct SemanticOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: &'a str,
    retained_relevant_events: u64,
    logical_relevant_events: u64,
    delta_object_count: u64,
    zap_distinct_evidence_sha256: &'a str,
    zap_distinct_identity_rows: u64,
    zap_distinct_leaf_rows: usize,
}

#[derive(Serialize)]
struct RelayOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: &'a str,
    delta_object_count: u64,
    candidate_events: u64,
    winning_pubkeys: u64,
    relay_rows: usize,
}

#[derive(Serialize)]
struct PublisherOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: &'a str,
    ledger_rows: u64,
    ranking_groups: u64,
    ranking_rows: u64,
    ranking_bytes: u64,
}

#[derive(Serialize)]
struct ServingOutput<'a> {
    evidence_sha256: &'a str,
    baseline_evidence_sha256: &'a str,
    logical_events: u64,
    delta_object_count: u64,
    hourly_rows: u64,
    kind_rows: u64,
    complete_through_epoch: u64,
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
    let activity_enabled = args.activity_baseline_evidence.is_some();
    let cohort_enabled = args.cohort_baseline_evidence.is_some();
    let flexible_enabled = args.flexible_baseline_evidence.is_some();
    let semantic_enabled = args.semantic_baseline_evidence.is_some();
    let relay_enabled = args.relay_baseline_evidence.is_some();
    let publisher_enabled = args.publisher_baseline_evidence.is_some();
    let serving_enabled = args.serving_baseline_evidence.is_some();
    require_complete_product_args(
        "identity",
        identity_enabled,
        args.identity_evidence.is_some(),
        args.identity_work_root.is_some(),
    )?;
    require_complete_product_args(
        "activity",
        activity_enabled,
        args.activity_evidence.is_some(),
        args.activity_work_root.is_some(),
    )?;
    require_complete_cohort_args(cohort_enabled, args.cohort_evidence.is_some())?;
    require_complete_flexible_args(
        flexible_enabled,
        args.flexible_evidence.is_some(),
        args.flexible_work_root.is_some(),
        args.flexible_validation_evidence.is_some(),
    )?;
    require_complete_semantic_args([
        semantic_enabled,
        args.semantic_baseline_artifact.is_some(),
        args.semantic_evidence.is_some(),
        args.semantic_work_root.is_some(),
        args.zap_distinct_evidence.is_some(),
        args.zap_distinct_work_root.is_some(),
    ])?;
    require_complete_relay_args([
        relay_enabled,
        args.relay_state_database.is_some(),
        args.relay_evidence.is_some(),
    ])?;
    require_complete_publisher_args([
        publisher_enabled,
        args.publisher_baseline_state_database.is_some(),
        args.publisher_state_database.is_some(),
        args.publisher_artifact.is_some(),
        args.publisher_evidence.is_some(),
    ])?;
    require_complete_serving_args([
        serving_enabled,
        args.serving_evidence.is_some(),
        args.serving_work_root.is_some(),
    ])?;
    if activity_enabled && !identity_enabled {
        bail!("fixed-activity advancement requires identity advancement in the same run");
    }
    if cohort_enabled && !activity_enabled {
        bail!(
            "cohort-retention advancement requires identity and activity advancement in the same run"
        );
    }
    if flexible_enabled && !cohort_enabled {
        bail!("flexible-distinct advancement requires the complete B3 lane in the same run");
    }
    if semantic_enabled && !flexible_enabled {
        bail!("semantic advancement requires the complete Slice 6 lane in the same run");
    }
    if relay_enabled && !semantic_enabled {
        bail!("relay advancement requires the complete Slice 7 lane in the same run");
    }
    if publisher_enabled && !relay_enabled {
        bail!("publisher advancement requires the complete Slice 8 lane in the same run");
    }
    if serving_enabled && !publisher_enabled {
        bail!("serving-facts advancement requires the complete Slice 9 lane in the same run");
    }
    let desired_query_version = if cohort_enabled {
        COHORT_RETENTION_QUERY_VERSION
    } else if activity_enabled {
        FIXED_ACTIVITY_QUERY_VERSION
    } else if identity_enabled {
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
    let cohort_baseline_sha256 = if let Some(path) = args.cohort_baseline_evidence.as_ref() {
        Some(validate_cohort_baseline(
            &mut client,
            previous_run_id,
            path,
            args.identity_baseline_evidence
                .as_deref()
                .expect("cohort requires identity baseline"),
            args.activity_baseline_evidence
                .as_deref()
                .expect("cohort requires activity baseline"),
            persisted_plan
                .previous_snapshot_id
                .as_deref()
                .expect("incremental cohort plan has a baseline snapshot"),
            baseline_as_of_epoch,
        )?)
    } else {
        None
    };
    // Validate every immutable predecessor before advancing DuckDB or building
    // any successor product. A late predecessor failure would otherwise waste
    // hours rebuilding unrelated products even though publication must fail
    // closed. Keep these loaded products for the later advancement calls so
    // the full validations are not repeated.
    let identity_baseline = args
        .identity_baseline_evidence
        .as_ref()
        .map(load_bounded_pubkey_first_seen)
        .transpose()
        .context("preflight baseline first-seen evidence")?;
    let activity_baseline = args
        .activity_baseline_evidence
        .as_ref()
        .map(load_bounded_fixed_activity)
        .transpose()
        .context("preflight baseline fixed-activity evidence")?;
    let flexible_baseline = args
        .flexible_baseline_evidence
        .as_ref()
        .map(load_bounded_flexible_distinct)
        .transpose()
        .context("preflight baseline flexible-distinct evidence")?;
    let semantic_baseline = match (
        args.semantic_baseline_evidence.as_ref(),
        args.semantic_baseline_artifact.as_ref(),
    ) {
        (Some(evidence), Some(artifact)) => Some(
            load_bounded_semantic_facts(evidence, artifact)
                .context("preflight baseline semantic evidence")?,
        ),
        _ => None,
    };
    let relay_baseline = match (
        args.relay_baseline_evidence.as_ref(),
        args.relay_state_database.as_ref(),
    ) {
        (Some(evidence), Some(state)) => Some(
            load_bounded_relay_distribution_for_advance(evidence, state, &target)
                .context("preflight baseline relay-distribution evidence and state")?,
        ),
        _ => None,
    };
    let publisher_baseline = match (
        args.publisher_baseline_evidence.as_ref(),
        args.publisher_baseline_state_database.as_ref(),
    ) {
        (Some(evidence), Some(state)) => Some(
            load_bounded_publisher_ranking(evidence, state)
                .context("preflight baseline publisher ranking evidence and state")?,
        ),
        _ => None,
    };
    let serving_baseline = args
        .serving_baseline_evidence
        .as_ref()
        .map(load_bounded_serving_facts)
        .transpose()
        .context("preflight baseline serving-facts evidence")?;
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
    let identity = if let (Some(baseline), Some(evidence_path), Some(work_root)) = (
        identity_baseline.as_ref(),
        args.identity_evidence.as_ref(),
        args.identity_work_root.as_ref(),
    ) {
        Some(
            advance_bounded_pubkey_first_seen(
                evidence_path,
                baseline,
                target.clone(),
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
    let activity = if let (Some(baseline), Some(evidence_path), Some(work_root)) = (
        activity_baseline.as_ref(),
        args.activity_evidence.as_ref(),
        args.activity_work_root.as_ref(),
    ) {
        Some(
            advance_bounded_fixed_activity(
                evidence_path,
                baseline,
                target.clone(),
                &persisted_plan,
                &delta_locations,
                build.config.clone(),
                FixedActivityConfig {
                    work_root: work_root.clone(),
                    batch_limits: BatchLimits {
                        max_bytes: args.activity_batch_bytes,
                        max_rows: args.activity_batch_rows,
                    },
                    merge_fan_in: args.activity_merge_fan_in,
                    disk_reserve_bytes: args.activity_disk_reserve_bytes,
                },
            )
            .context("advance bounded fixed-activity state")?,
        )
    } else {
        None
    };
    let cohort = if let (Some(evidence_path), Some(identity), Some(activity)) = (
        args.cohort_evidence.as_ref(),
        identity.as_ref(),
        activity.as_ref(),
    ) {
        Some(
            build_bounded_cohort_retention(
                evidence_path,
                identity,
                activity,
                args.cohort_matrix_row_limit,
            )
            .context("build bounded cohort-retention state")?,
        )
    } else {
        None
    };
    let flexible = if let (
        Some(baseline),
        Some(evidence_path),
        Some(work_root),
        Some(baseline_activity),
        Some(activity),
    ) = (
        flexible_baseline.as_ref(),
        args.flexible_evidence.as_ref(),
        args.flexible_work_root.as_ref(),
        activity_baseline.as_ref(),
        activity.as_ref(),
    ) {
        Some(
            advance_bounded_flexible_distinct(
                evidence_path,
                baseline,
                baseline_activity,
                activity,
                FlexibleDistinctConfig {
                    work_root: work_root.clone(),
                    source_records_per_batch: args.flexible_source_records_per_batch,
                    merge_fan_in: args.flexible_merge_fan_in,
                    disk_reserve_bytes: args.flexible_disk_reserve_bytes,
                },
            )
            .context("advance bounded flexible-distinct state")?,
        )
    } else {
        None
    };
    let flexible_validation = if let (Some(path), Some(activity), Some(flexible_path)) = (
        args.flexible_validation_evidence.as_ref(),
        activity.as_ref(),
        args.flexible_evidence.as_ref(),
    ) {
        Some(
            build_flexible_distinct_validation(
                path,
                activity,
                flexible_path,
                args.flexible_tolerance_ppm,
            )
            .context("validate flexible-distinct production tolerance")?,
        )
    } else {
        None
    };
    let flexible_validation_sha256 = args
        .flexible_validation_evidence
        .as_ref()
        .filter(|_| flexible_validation.is_some())
        .map(pensieve_lake::sha256_file)
        .transpose()
        .context("hash flexible-distinct tolerance evidence")?;
    let semantic = if let (Some(baseline), Some(evidence_path), Some(work_root)) = (
        semantic_baseline.as_ref(),
        args.semantic_evidence.as_ref(),
        args.semantic_work_root.as_ref(),
    ) {
        Some(
            advance_bounded_semantic_facts(
                evidence_path,
                baseline,
                target.clone(),
                &persisted_plan,
                &delta_locations,
                build.config.clone(),
                SemanticFactsConfig {
                    work_root: work_root.clone(),
                    batch_limits: BatchLimits {
                        max_bytes: args.semantic_batch_bytes,
                        max_rows: args.semantic_batch_rows,
                    },
                    merge_fan_in: args.semantic_merge_fan_in,
                    disk_reserve_bytes: args.semantic_disk_reserve_bytes,
                },
            )
            .context("advance bounded semantic facts")?,
        )
    } else {
        None
    };
    let zap_distinct = if let (Some(semantic), Some(evidence_path), Some(work_root)) = (
        semantic.as_ref(),
        args.zap_distinct_evidence.as_ref(),
        args.zap_distinct_work_root.as_ref(),
    ) {
        Some(
            build_bounded_zap_distinct(
                semantic,
                evidence_path,
                ZapDistinctConfig {
                    work_root: work_root.clone(),
                    chunk_records: args.zap_distinct_chunk_records,
                    merge_fan_in: args.zap_distinct_merge_fan_in,
                    disk_reserve_bytes: args.zap_distinct_disk_reserve_bytes,
                },
            )
            .context("build bounded zap-distinct state")?,
        )
    } else {
        None
    };
    let relay = if let (Some(baseline), Some(state_database), Some(evidence_path)) = (
        relay_baseline.as_ref(),
        args.relay_state_database.as_ref(),
        args.relay_evidence.as_ref(),
    ) {
        Some(
            advance_bounded_relay_distribution(
                evidence_path,
                baseline,
                target.clone(),
                build.config.clone(),
                RelayDistributionConfig {
                    state_database: state_database.clone(),
                    batch_limits: BatchLimits {
                        max_bytes: args.relay_batch_bytes,
                        max_rows: args.relay_batch_rows,
                    },
                    max_state_bytes: args.relay_max_state_bytes,
                    sqlite_cache_bytes: args.relay_sqlite_cache_bytes,
                    minimum_users: args.relay_minimum_users,
                    disk_reserve_bytes: args.relay_disk_reserve_bytes,
                },
            )
            .context("advance bounded relay-distribution state")?,
        )
    } else {
        None
    };
    let publisher = if let (
        Some(baseline),
        Some(state_database),
        Some(artifact),
        Some(evidence_path),
        Some(activity),
    ) = (
        publisher_baseline.as_ref(),
        args.publisher_state_database.as_ref(),
        args.publisher_artifact.as_ref(),
        args.publisher_evidence.as_ref(),
        activity.as_ref(),
    ) {
        Some(
            advance_bounded_publisher_ranking(
                evidence_path,
                baseline,
                activity,
                PublisherRankingConfig {
                    state_database: state_database.clone(),
                    artifact_path: artifact.clone(),
                    windows_days: args.publisher_windows_days.clone(),
                    top_limit: args.publisher_top_limit,
                    publisher_batch_size: args.publisher_batch_size,
                    max_state_bytes: args.publisher_max_state_bytes,
                    sqlite_cache_bytes: args.publisher_sqlite_cache_bytes,
                    disk_reserve_bytes: args.publisher_disk_reserve_bytes,
                },
            )
            .context("advance bounded publisher ranking state")?,
        )
    } else {
        None
    };
    let serving =
        if let (Some(baseline), Some(evidence_path), Some(work_root), Some(activity_evidence)) = (
            serving_baseline.as_ref(),
            args.serving_evidence.as_ref(),
            args.serving_work_root.as_ref(),
            args.activity_evidence.as_ref(),
        ) {
            Some(
                advance_bounded_serving_facts(
                    evidence_path,
                    baseline,
                    target.clone(),
                    &persisted_plan,
                    &delta_locations,
                    build.config.clone(),
                    ServingFactsConfig {
                        work_root: work_root.clone(),
                        batch_limits: BatchLimits {
                            max_bytes: args.serving_batch_bytes,
                            max_rows: args.serving_batch_rows,
                        },
                        merge_fan_in: args.serving_merge_fan_in,
                        disk_reserve_bytes: args.serving_disk_reserve_bytes,
                    },
                    activity_evidence,
                )
                .context("advance bounded serving facts")?,
            )
        } else {
            None
        };
    let completed_at = Utc::now();
    let publication = if args.dry_run {
        None
    } else {
        Some(
            match if let (
                Some(identity),
                Some(activity),
                Some(cohort),
                Some(flexible),
                Some(validation_path),
                Some(validation_sha256),
                Some(semantic),
                Some(zap_distinct),
                Some(relay),
                Some(publisher),
                Some(serving),
            ) = (
                identity.as_ref(),
                activity.as_ref(),
                cohort.as_ref(),
                flexible.as_ref(),
                args.flexible_validation_evidence.as_deref(),
                flexible_validation_sha256.as_deref(),
                semantic.as_ref(),
                zap_distinct.as_ref(),
                relay.as_ref(),
                publisher.as_ref(),
                serving.as_ref(),
            ) {
                publish_incremental_with_all_recurring_products_and_serving(
                    &mut client,
                    &build,
                    AllRecurringProductsWithServing {
                        recurring: AllRecurringProductsWithPublisher {
                            recurring: AllRecurringProductsWithRelay {
                                recurring: AllRecurringProducts {
                                    bounded: AllBoundedProducts {
                                        identity,
                                        activity,
                                        cohort,
                                    },
                                    flexible: FlexibleDistinctPublication {
                                        product: flexible,
                                        validation_evidence_path: validation_path,
                                        validation_evidence_sha256: validation_sha256,
                                    },
                                    semantic: SemanticPublication {
                                        product: semantic,
                                        zap_distinct,
                                    },
                                },
                                relay,
                            },
                            publisher,
                        },
                        serving,
                    },
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let (
                Some(identity),
                Some(activity),
                Some(cohort),
                Some(flexible),
                Some(validation_path),
                Some(validation_sha256),
                Some(semantic),
                Some(zap_distinct),
                Some(relay),
                Some(publisher),
            ) = (
                identity.as_ref(),
                activity.as_ref(),
                cohort.as_ref(),
                flexible.as_ref(),
                args.flexible_validation_evidence.as_deref(),
                flexible_validation_sha256.as_deref(),
                semantic.as_ref(),
                zap_distinct.as_ref(),
                relay.as_ref(),
                publisher.as_ref(),
            ) {
                publish_incremental_with_all_recurring_products_and_publisher(
                    &mut client,
                    &build,
                    AllRecurringProductsWithPublisher {
                        recurring: AllRecurringProductsWithRelay {
                            recurring: AllRecurringProducts {
                                bounded: AllBoundedProducts {
                                    identity,
                                    activity,
                                    cohort,
                                },
                                flexible: FlexibleDistinctPublication {
                                    product: flexible,
                                    validation_evidence_path: validation_path,
                                    validation_evidence_sha256: validation_sha256,
                                },
                                semantic: SemanticPublication {
                                    product: semantic,
                                    zap_distinct,
                                },
                            },
                            relay,
                        },
                        publisher,
                    },
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let (
                Some(identity),
                Some(activity),
                Some(cohort),
                Some(flexible),
                Some(validation_path),
                Some(validation_sha256),
                Some(semantic),
                Some(zap_distinct),
                Some(relay),
            ) = (
                identity.as_ref(),
                activity.as_ref(),
                cohort.as_ref(),
                flexible.as_ref(),
                args.flexible_validation_evidence.as_deref(),
                flexible_validation_sha256.as_deref(),
                semantic.as_ref(),
                zap_distinct.as_ref(),
                relay.as_ref(),
            ) {
                publish_incremental_with_all_recurring_products_and_relay(
                    &mut client,
                    &build,
                    AllRecurringProductsWithRelay {
                        recurring: AllRecurringProducts {
                            bounded: AllBoundedProducts {
                                identity,
                                activity,
                                cohort,
                            },
                            flexible: FlexibleDistinctPublication {
                                product: flexible,
                                validation_evidence_path: validation_path,
                                validation_evidence_sha256: validation_sha256,
                            },
                            semantic: SemanticPublication {
                                product: semantic,
                                zap_distinct,
                            },
                        },
                        relay,
                    },
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let (
                Some(identity),
                Some(activity),
                Some(cohort),
                Some(flexible),
                Some(validation_path),
                Some(validation_sha256),
                Some(semantic),
                Some(zap_distinct),
            ) = (
                identity.as_ref(),
                activity.as_ref(),
                cohort.as_ref(),
                flexible.as_ref(),
                args.flexible_validation_evidence.as_deref(),
                flexible_validation_sha256.as_deref(),
                semantic.as_ref(),
                zap_distinct.as_ref(),
            ) {
                publish_incremental_with_all_bounded_products_flexible_and_semantic(
                    &mut client,
                    &build,
                    AllRecurringProducts {
                        bounded: AllBoundedProducts {
                            identity,
                            activity,
                            cohort,
                        },
                        flexible: FlexibleDistinctPublication {
                            product: flexible,
                            validation_evidence_path: validation_path,
                            validation_evidence_sha256: validation_sha256,
                        },
                        semantic: SemanticPublication {
                            product: semantic,
                            zap_distinct,
                        },
                    },
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let (
                Some(identity),
                Some(activity),
                Some(cohort),
                Some(flexible),
                Some(validation_path),
                Some(validation_sha256),
            ) = (
                identity.as_ref(),
                activity.as_ref(),
                cohort.as_ref(),
                flexible.as_ref(),
                args.flexible_validation_evidence.as_deref(),
                flexible_validation_sha256.as_deref(),
            ) {
                publish_incremental_with_all_bounded_products_and_flexible(
                    &mut client,
                    &build,
                    AllBoundedProducts {
                        identity,
                        activity,
                        cohort,
                    },
                    FlexibleDistinctPublication {
                        product: flexible,
                        validation_evidence_path: validation_path,
                        validation_evidence_sha256: validation_sha256,
                    },
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let (Some(identity), Some(activity), Some(cohort)) =
                (identity.as_ref(), activity.as_ref(), cohort.as_ref())
            {
                publish_incremental_with_all_bounded_products(
                    &mut client,
                    &build,
                    AllBoundedProducts {
                        identity,
                        activity,
                        cohort,
                    },
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let (Some(identity), Some(activity)) = (identity.as_ref(), activity.as_ref())
            {
                publish_incremental_with_identity_and_activity(
                    &mut client,
                    &build,
                    identity,
                    activity,
                    previous_run_id,
                    started_at,
                    completed_at,
                )
            } else if let Some(identity) = identity.as_ref() {
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
            activity: activity.as_ref().map(|activity| ActivityOutput {
                evidence_sha256: &activity.evidence_sha256,
                baseline_evidence_sha256: activity.evidence.baseline_evidence_sha256.as_deref(),
                activity_records: activity.evidence.activity_artifact.row_count,
                pubkey_flag_records: activity.evidence.flags_artifact.row_count,
                distinct_period_rows: activity.evidence.distinct_period_rows,
                active_period_rows: activity.evidence.active_period_rows,
                delta_object_count: activity.evidence.delta_object_count,
                max_merge_buffered_bytes: activity.evidence.max_merge_buffered_bytes,
                max_week_kinds_buffered: activity.evidence.max_week_kinds_buffered,
                max_month_kinds_buffered: activity.evidence.max_month_kinds_buffered,
            }),
            cohort: cohort.as_ref().map(|cohort| CohortOutput {
                evidence_sha256: &cohort.evidence_sha256,
                baseline_evidence_sha256: cohort_baseline_sha256
                    .as_deref()
                    .expect("cohort output has baseline evidence"),
                period_rows: cohort.evidence.period_rows,
                active_pubkeys_sum: cohort.evidence.active_pubkeys_sum,
                metric_sha256: &cohort.evidence.metric_sha256,
                matrix_row_limit: cohort.evidence.matrix_row_limit,
                max_pubkey_periods_buffered: cohort.evidence.max_pubkey_periods_buffered,
            }),
            flexible: flexible.as_ref().map(|flexible| FlexibleOutput {
                evidence_sha256: &flexible.evidence_sha256,
                baseline_evidence_sha256: flexible
                    .evidence
                    .baseline_evidence_sha256
                    .as_deref()
                    .expect("flexible successor has baseline evidence"),
                complete_through_epoch: flexible.evidence.complete_through_epoch,
                identity_rows: flexible.evidence.identity_artifact.row_count,
                leaf_rows: flexible.evidence.leaf_artifact.row_count,
                validation_evidence_sha256: flexible_validation_sha256
                    .as_deref()
                    .expect("flexible output has validation evidence"),
                max_relative_error_ppm: flexible_validation
                    .as_ref()
                    .expect("flexible output has validation result")
                    .max_relative_error_ppm,
            }),
            semantic: semantic.as_ref().map(|semantic| SemanticOutput {
                evidence_sha256: &semantic.evidence_sha256,
                baseline_evidence_sha256: semantic
                    .evidence
                    .baseline_evidence_sha256
                    .as_deref()
                    .expect("semantic successor has baseline evidence"),
                retained_relevant_events: semantic.evidence.retained_relevant_events,
                logical_relevant_events: semantic.evidence.logical_relevant_events,
                delta_object_count: semantic.evidence.delta_object_count,
                zap_distinct_evidence_sha256: &zap_distinct
                    .as_ref()
                    .expect("semantic output has zap distinct")
                    .evidence_sha256,
                zap_distinct_identity_rows: zap_distinct
                    .as_ref()
                    .expect("semantic output has zap distinct")
                    .evidence
                    .logical_identities,
                zap_distinct_leaf_rows: zap_distinct
                    .as_ref()
                    .expect("semantic output has zap distinct")
                    .evidence
                    .leaves
                    .len(),
            }),
            relay: relay.as_ref().map(|relay| RelayOutput {
                evidence_sha256: &relay.evidence_sha256,
                baseline_evidence_sha256: relay
                    .evidence
                    .baseline_evidence_sha256
                    .as_deref()
                    .expect("relay successor has baseline evidence"),
                delta_object_count: relay.evidence.delta_object_count,
                candidate_events: relay.evidence.candidate_events,
                winning_pubkeys: relay.evidence.winning_pubkeys,
                relay_rows: relay.evidence.rows.len(),
            }),
            publisher: publisher.as_ref().map(|publisher| PublisherOutput {
                evidence_sha256: &publisher.evidence_sha256,
                baseline_evidence_sha256: publisher
                    .evidence
                    .baseline_evidence_sha256
                    .as_deref()
                    .expect("publisher successor has baseline evidence"),
                ledger_rows: publisher.evidence.ledger_rows,
                ranking_groups: publisher.evidence.ranking_groups,
                ranking_rows: publisher.evidence.ranking_artifact.row_count,
                ranking_bytes: publisher.evidence.ranking_artifact.byte_size,
            }),
            serving: serving.as_ref().map(|serving| ServingOutput {
                evidence_sha256: &serving.evidence_sha256,
                baseline_evidence_sha256: serving
                    .evidence
                    .baseline_evidence_sha256
                    .as_deref()
                    .expect("serving successor has baseline evidence"),
                logical_events: serving.evidence.logical_events,
                delta_object_count: serving.evidence.delta_object_count,
                hourly_rows: serving.evidence.hourly_artifact.row_count,
                kind_rows: serving.evidence.kind_artifact.row_count,
                complete_through_epoch: serving.evidence.complete_through_epoch,
            }),
            publication,
        })?
    );
    Ok(())
}

fn require_complete_cohort_args(baseline: bool, evidence: bool) -> Result<()> {
    if baseline == evidence {
        return Ok(());
    }
    bail!("cohort baseline evidence and output evidence must be supplied together")
}

fn require_complete_flexible_args(
    baseline: bool,
    evidence: bool,
    work_root: bool,
    validation: bool,
) -> Result<()> {
    if [baseline, evidence, work_root, validation]
        .into_iter()
        .all(|value| value == baseline)
    {
        return Ok(());
    }
    bail!("flexible baseline, output, work root, and validation evidence must be supplied together")
}

fn require_complete_semantic_args(present: [bool; 6]) -> Result<()> {
    if present.into_iter().all(|value| value == present[0]) {
        return Ok(());
    }
    bail!(
        "semantic baseline evidence/artifact, successor evidence/work root, and zap evidence/work root must be supplied together"
    )
}

fn require_complete_relay_args(present: [bool; 3]) -> Result<()> {
    if present.into_iter().all(|value| value == present[0]) {
        return Ok(());
    }
    bail!(
        "relay baseline evidence, state database, and successor evidence must be supplied together"
    )
}

fn require_complete_publisher_args(present: [bool; 5]) -> Result<()> {
    if present.into_iter().all(|value| value == present[0]) {
        return Ok(());
    }
    bail!(
        "publisher baseline evidence/state and successor state/artifact/evidence must be supplied together"
    )
}

fn require_complete_serving_args(present: [bool; 3]) -> Result<()> {
    if present.into_iter().all(|value| value == present[0]) {
        return Ok(());
    }
    bail!("serving baseline evidence, successor evidence, and work root must be supplied together")
}

fn validate_cohort_baseline(
    client: &mut postgres::Client,
    previous_run_id: &str,
    cohort_path: &std::path::Path,
    identity_path: &std::path::Path,
    activity_path: &std::path::Path,
    expected_snapshot_id: &str,
    expected_as_of_epoch: u64,
) -> Result<String> {
    let evidence: CohortRetentionEvidence = serde_json::from_slice(
        &std::fs::read(cohort_path).context("read baseline cohort evidence")?,
    )
    .context("decode baseline cohort evidence")?;
    let evidence_sha256 = pensieve_lake::sha256_file(cohort_path)?;
    let identity_sha256 = pensieve_lake::sha256_file(identity_path)?;
    let activity_sha256 = pensieve_lake::sha256_file(activity_path)?;
    let published_sha256: Option<String> = client
        .query_one(
            "SELECT validation ->> 'cohort_retention_evidence_sha256' FROM pensieve_analytics.runs WHERE run_id = $1",
            &[&previous_run_id],
        )
        .context("read published baseline cohort identity")?
        .get(0);
    if evidence.schema_version != 1
        || evidence.runner_version != "pensieve-analytics-cohort-retention-v1"
        || evidence.status != "completed"
        || evidence.snapshot_id != expected_snapshot_id
        || evidence.as_of_epoch != expected_as_of_epoch
        || evidence.identity_evidence_sha256 != identity_sha256
        || evidence.activity_evidence_sha256 != activity_sha256
        || evidence.period_rows != u64::try_from(evidence.periods.len())?
        || evidence.periods.len() > evidence.matrix_row_limit
        || published_sha256.as_deref() != Some(evidence_sha256.as_str())
    {
        bail!("baseline cohort evidence does not match the current published B3 run");
    }
    Ok(evidence_sha256)
}

fn require_complete_product_args(
    product: &str,
    baseline: bool,
    evidence: bool,
    work_root: bool,
) -> Result<()> {
    if baseline == evidence && evidence == work_root {
        return Ok(());
    }
    bail!("{product} baseline evidence, output evidence, and work root must be supplied together")
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

    #[test]
    fn cohort_arguments_are_all_or_nothing() {
        require_complete_cohort_args(false, false).expect("disabled cohort is valid");
        require_complete_cohort_args(true, true).expect("complete cohort arguments are valid");
        assert!(require_complete_cohort_args(true, false).is_err());
        assert!(require_complete_cohort_args(false, true).is_err());
    }

    #[test]
    fn flexible_arguments_are_all_or_nothing() {
        require_complete_flexible_args(false, false, false, false)
            .expect("disabled flexible lane is valid");
        require_complete_flexible_args(true, true, true, true)
            .expect("complete flexible lane is valid");
        assert!(require_complete_flexible_args(true, true, true, false).is_err());
        assert!(require_complete_flexible_args(false, true, true, true).is_err());
    }

    #[test]
    fn semantic_arguments_are_all_or_nothing() {
        require_complete_semantic_args([false; 6]).expect("disabled semantic lane is valid");
        require_complete_semantic_args([true; 6]).expect("complete semantic lane is valid");
        assert!(require_complete_semantic_args([true, true, true, true, true, false]).is_err());
        assert!(require_complete_semantic_args([false, true, true, true, true, true]).is_err());
    }

    #[test]
    fn relay_arguments_are_all_or_nothing() {
        require_complete_relay_args([false; 3]).expect("disabled relay lane is valid");
        require_complete_relay_args([true; 3]).expect("complete relay lane is valid");
        assert!(require_complete_relay_args([true, true, false]).is_err());
        assert!(require_complete_relay_args([false, true, true]).is_err());
    }

    #[test]
    fn publisher_arguments_are_all_or_nothing() {
        require_complete_publisher_args([false; 5]).expect("disabled publisher lane is valid");
        require_complete_publisher_args([true; 5]).expect("complete publisher lane is valid");
        assert!(require_complete_publisher_args([true, true, true, true, false]).is_err());
        assert!(require_complete_publisher_args([false, true, true, true, true]).is_err());
    }

    #[test]
    fn serving_arguments_are_all_or_nothing() {
        require_complete_serving_args([false; 3]).expect("disabled serving lane is valid");
        require_complete_serving_args([true; 3]).expect("complete serving lane is valid");
        assert!(require_complete_serving_args([true, true, false]).is_err());
        assert!(require_complete_serving_args([false, true, true]).is_err());
    }
}
