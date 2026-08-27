//! Build resumable bounded daily zap participant sketches from Slice 7 facts.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pensieve_analytics::{
    ZapDistinctConfig, build_bounded_zap_distinct, load_bounded_semantic_facts,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Build bounded daily zap sender and recipient sketches")]
struct Args {
    /// Validated Slice 7 semantic completion evidence.
    #[arg(long)]
    semantic_evidence: PathBuf,
    /// Final compact semantic fact artifact named by the evidence.
    #[arg(long)]
    semantic_artifact: PathBuf,
    /// Explicitly authorized SHA-256 of semantic completion evidence.
    #[arg(long)]
    semantic_evidence_sha256: String,
    /// Dedicated immutable identity chunk/merge workspace.
    #[arg(long)]
    work_root: PathBuf,
    /// Immutable zap-distinct completion evidence JSON.
    #[arg(long)]
    evidence: PathBuf,
    /// Maximum 41-byte identities held before sorting a chunk.
    #[arg(long, default_value_t = 1_000_000)]
    chunk_records: usize,
    /// Maximum chunk/merge runs opened by one streaming merge.
    #[arg(long, default_value_t = 16)]
    merge_fan_in: usize,
    /// Free work-filesystem bytes left untouched.
    #[arg(long, default_value_t = 107_374_182_400)]
    disk_reserve_bytes: u64,
}

#[derive(Serialize)]
struct Output<'a> {
    snapshot_id: &'a str,
    as_of_epoch: u64,
    physical_identities: u64,
    logical_identities: u64,
    duplicate_identities: u64,
    daily_leaves: usize,
    identity_artifact: &'a str,
    identity_artifact_sha256: &'a str,
    evidence_sha256: &'a str,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let semantic = load_bounded_semantic_facts(&args.semantic_evidence, &args.semantic_artifact)
        .context("load and validate semantic facts")?;
    if semantic.evidence_sha256 != args.semantic_evidence_sha256 {
        bail!("semantic evidence SHA-256 differs from the authorized gate");
    }
    let completed = build_bounded_zap_distinct(
        &semantic,
        &args.evidence,
        ZapDistinctConfig {
            work_root: args.work_root,
            chunk_records: args.chunk_records,
            merge_fan_in: args.merge_fan_in,
            disk_reserve_bytes: args.disk_reserve_bytes,
        },
    )
    .context("build bounded zap participant sketches")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            snapshot_id: &completed.evidence.snapshot_id,
            as_of_epoch: completed.evidence.as_of_epoch,
            physical_identities: completed.evidence.physical_identities,
            logical_identities: completed.evidence.logical_identities,
            duplicate_identities: completed.evidence.duplicate_identities,
            daily_leaves: completed.evidence.leaves.len(),
            identity_artifact: &completed.identity_path.to_string_lossy(),
            identity_artifact_sha256: &completed.evidence.identity_artifact.sha256,
            evidence_sha256: &completed.evidence_sha256,
        })?
    );
    Ok(())
}
