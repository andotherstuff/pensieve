//! Build or advance bounded fixed-activity state from Parquet.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    BatchLimits, BuildConfig, CatalogDeltaPlan, FixedActivityConfig,
    advance_bounded_fixed_activity, build_bounded_fixed_activity, load_bounded_fixed_activity,
    resolve_delta_locations, resolve_snapshot,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded exact fixed-activity state")]
struct Args {
    /// Canonical active-file snapshot.
    #[arg(long)]
    catalog: PathBuf,
    /// Verified local object root; omit to read immutable objects from the catalog store.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Dedicated immutable fixed-activity batch and merge workspace.
    #[arg(long)]
    activity_work_root: PathBuf,
    /// Immutable fixed-activity completion evidence.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Verified predecessor activity evidence; enables append-only advancement.
    #[arg(long, requires_all = ["delta_plan", "delta_object_root"])]
    activity_baseline_evidence: Option<PathBuf>,
    /// Persisted catalog delta plan used to stage successor objects.
    #[arg(long)]
    delta_plan: Option<PathBuf>,
    /// Root containing only the delta plan's verified added objects.
    #[arg(long)]
    delta_object_root: Option<PathBuf>,
    /// Fixed Unix timestamp bounding eligible events.
    #[arg(long)]
    as_of: u64,
    /// Build/commit identity recorded in the evidence.
    #[arg(long)]
    code_version: String,
    /// AWS region used by DuckDB's environment credential chain.
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    s3_region: String,
    /// Use path-style S3 addressing.
    #[arg(long)]
    s3_force_path_style: bool,
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
    activity_records: u64,
    pubkey_flag_records: u64,
    distinct_period_rows: u64,
    active_period_rows: u64,
    activity_evidence_sha256: String,
    baseline_evidence_sha256: Option<String>,
    delta_object_count: u64,
    activity_artifact_sha256: String,
    flags_artifact_sha256: String,
    max_merge_buffered_bytes: usize,
    max_week_kinds_buffered: usize,
    max_month_kinds_buffered: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("activity build failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let snapshot = resolve_snapshot(&args.catalog, args.local_object_root.as_deref())
        .context("resolve immutable snapshot")?;
    let snapshot_id = snapshot.catalog.snapshot_id.clone();
    let build_config = BuildConfig {
        as_of_epoch: args.as_of,
        code_version: args.code_version,
        s3_region: args.s3_region,
        s3_force_path_style: args.s3_force_path_style,
        memory_limit: args.memory_limit,
        threads: args.threads,
    };
    let activity_config = FixedActivityConfig {
        work_root: args.activity_work_root,
        batch_limits: BatchLimits {
            max_bytes: args.batch_bytes,
            max_rows: args.batch_rows,
        },
        merge_fan_in: args.merge_fan_in,
        disk_reserve_bytes: args.disk_reserve_bytes,
    };
    let activity = match (
        args.activity_baseline_evidence.as_ref(),
        args.delta_plan.as_ref(),
        args.delta_object_root.as_ref(),
    ) {
        (Some(baseline_path), Some(plan_path), Some(delta_root)) => {
            let baseline = load_bounded_fixed_activity(baseline_path)
                .context("load baseline fixed-activity evidence")?;
            let plan: CatalogDeltaPlan = serde_json::from_slice(
                &std::fs::read(plan_path).context("read persisted activity delta plan")?,
            )
            .context("decode persisted activity delta plan")?;
            let delta_locations = resolve_delta_locations(&plan, delta_root)
                .context("resolve verified activity delta objects")?;
            advance_bounded_fixed_activity(
                &args.activity_evidence,
                &baseline,
                snapshot,
                &plan,
                &delta_locations,
                build_config,
                activity_config,
            )
            .context("advance bounded fixed-activity state")?
        }
        (None, None, None) => build_bounded_fixed_activity(
            &args.activity_evidence,
            snapshot,
            build_config,
            activity_config,
        )
        .context("build bounded fixed-activity state")?,
        _ => bail!(
            "activity baseline evidence, delta plan, and delta object root must be supplied together"
        ),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id,
            activity_records: activity.evidence.activity_artifact.row_count,
            pubkey_flag_records: activity.evidence.flags_artifact.row_count,
            distinct_period_rows: activity.evidence.distinct_period_rows,
            active_period_rows: activity.evidence.active_period_rows,
            activity_evidence_sha256: activity.evidence_sha256,
            baseline_evidence_sha256: activity.evidence.baseline_evidence_sha256,
            delta_object_count: activity.evidence.delta_object_count,
            activity_artifact_sha256: activity.evidence.activity_artifact.sha256,
            flags_artifact_sha256: activity.evidence.flags_artifact.sha256,
            max_merge_buffered_bytes: activity.evidence.max_merge_buffered_bytes,
            max_week_kinds_buffered: activity.evidence.max_week_kinds_buffered,
            max_month_kinds_buffered: activity.evidence.max_month_kinds_buffered,
        })?
    );
    Ok(())
}
