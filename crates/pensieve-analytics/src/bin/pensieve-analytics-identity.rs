//! Build or advance bounded pubkey first-seen (identity) state from Parquet.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    BatchLimits, BuildConfig, CatalogDeltaPlan, PubkeyFirstSeenConfig,
    advance_bounded_pubkey_first_seen, build_bounded_pubkey_first_seen,
    load_bounded_pubkey_first_seen, resolve_delta_locations, resolve_snapshot,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded exact pubkey first-seen (identity) state")]
struct Args {
    /// Canonical active-file snapshot.
    #[arg(long)]
    catalog: PathBuf,
    /// Verified local object root; omit to read immutable objects from the catalog store.
    #[arg(long)]
    local_object_root: Option<PathBuf>,
    /// Dedicated immutable first-seen batch and merge workspace.
    #[arg(long)]
    identity_work_root: PathBuf,
    /// Immutable first-seen completion evidence.
    #[arg(long)]
    identity_evidence: PathBuf,
    /// Verified predecessor first-seen evidence; enables append-only advancement.
    #[arg(long, requires_all = ["delta_plan", "delta_object_root"])]
    identity_baseline_evidence: Option<PathBuf>,
    /// Persisted append-only catalog delta plan used to stage successor objects.
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
    eligible_pubkeys: u64,
    new_users_daily_rows: usize,
    identity_evidence_sha256: String,
    identity_artifact_sha256: String,
    max_merge_buffered_bytes: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("identity build failed: {error:#}");
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
        s3_region: String::new(),
        s3_force_path_style: false,
        memory_limit: args.memory_limit,
        threads: args.threads,
    };
    let identity_config = PubkeyFirstSeenConfig {
        work_root: args.identity_work_root,
        batch_limits: BatchLimits {
            max_bytes: args.batch_bytes,
            max_rows: args.batch_rows,
        },
        merge_fan_in: args.merge_fan_in,
        disk_reserve_bytes: args.disk_reserve_bytes,
    };
    let identity = match (
        args.identity_baseline_evidence.as_ref(),
        args.delta_plan.as_ref(),
        args.delta_object_root.as_ref(),
    ) {
        (Some(baseline_path), Some(plan_path), Some(delta_root)) => {
            let baseline = load_bounded_pubkey_first_seen(baseline_path)
                .context("load baseline first-seen evidence")?;
            let plan: CatalogDeltaPlan = serde_json::from_slice(
                &std::fs::read(plan_path).context("read persisted identity delta plan")?,
            )
            .context("decode persisted identity delta plan")?;
            let delta_locations = resolve_delta_locations(&plan, delta_root)
                .context("resolve verified identity delta objects")?;
            advance_bounded_pubkey_first_seen(
                &args.identity_evidence,
                &baseline,
                snapshot,
                &plan,
                &delta_locations,
                build_config,
                identity_config,
            )
            .context("advance bounded first-seen state")?
        }
        (None, None, None) => build_bounded_pubkey_first_seen(
            &args.identity_evidence,
            snapshot,
            build_config,
            identity_config,
        )
        .context("build bounded first-seen state")?,
        _ => bail!(
            "identity baseline evidence, delta plan, and delta object root must be supplied together"
        ),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id,
            eligible_pubkeys: identity.evidence.eligible_pubkeys,
            new_users_daily_rows: identity.evidence.new_users_daily.len(),
            identity_evidence_sha256: identity.evidence_sha256,
            identity_artifact_sha256: identity.evidence.final_artifact.sha256,
            max_merge_buffered_bytes: identity.evidence.max_merge_buffered_bytes,
        })?
    );
    Ok(())
}
