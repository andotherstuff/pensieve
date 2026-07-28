//! Freeze, inspect, and audit bounded historical source manifests.

use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pensieve_lake::{
    ActiveRawFragment, HistoricalSourceManifest, Inventory, audit_historical_completion,
    read_historical_source_manifest, write_catalog_atomically,
    write_historical_source_manifest_noclobber,
};

#[derive(Debug, Parser)]
#[command(about = "Manage the frozen historical notepack source universe")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Freeze a bounded manifest from one `rclone lsjson` capture.
    Build {
        /// JSON file produced by `rclone lsjson`.
        #[arg(long)]
        rclone_lsjson: PathBuf,
        /// Inclusive historical segment boundary.
        #[arg(long)]
        max_segment_number: u64,
        /// Immutable manifest destination; an existing file is never replaced.
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify and summarize one frozen manifest.
    Verify {
        /// Canonical source manifest.
        #[arg(long)]
        manifest: PathBuf,
        /// Optional operator-configured boundary that must match the manifest.
        #[arg(long)]
        expected_max_segment_number: Option<u64>,
    },
    /// Emit validated entries as tab-separated source name and bytes.
    Entries {
        /// Canonical source manifest.
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Audit manifest coverage against a campaign inventory.
    Audit {
        /// Canonical source manifest.
        #[arg(long)]
        manifest: PathBuf,
        /// Existing SQLite campaign inventory.
        #[arg(long)]
        inventory: PathBuf,
        /// Canonical JSON audit-report destination.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("source manifest failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Args::parse().command {
        Command::Build {
            rclone_lsjson,
            max_segment_number,
            output,
        } => {
            let bytes = std::fs::read(rclone_lsjson)?;
            let manifest =
                HistoricalSourceManifest::from_rclone_lsjson(&bytes, max_segment_number)?;
            write_historical_source_manifest_noclobber(output, &manifest)?;
            print_manifest(&manifest);
        }
        Command::Verify {
            manifest,
            expected_max_segment_number,
        } => {
            let manifest = read_historical_source_manifest(manifest)?;
            if let Some(expected) = expected_max_segment_number
                && expected != manifest.max_segment_number()
            {
                return Err(format!(
                    "manifest boundary {} does not match configured boundary {expected}",
                    manifest.max_segment_number()
                )
                .into());
            }
            print_manifest(&manifest);
        }
        Command::Entries { manifest } => {
            let manifest = read_historical_source_manifest(manifest)?;
            for entry in manifest.entries() {
                println!("{}\t{}", entry.source_name, entry.source_bytes);
            }
        }
        Command::Audit {
            manifest,
            inventory,
            output,
        } => {
            let manifest = read_historical_source_manifest(manifest)?;
            let mut inventory = Inventory::open_read_only(inventory)?;
            let work_units = inventory.work_units()?;
            let fragment = ActiveRawFragment::export(
                &mut inventory,
                "historical-completion-audit",
                "inventory://historical-campaign",
            )?;
            let active_work_unit_ids: BTreeSet<_> = fragment
                .work_units()
                .iter()
                .map(|work| work.work_unit_id.clone())
                .collect();
            let audit = audit_historical_completion(
                &manifest,
                &work_units,
                &active_work_unit_ids,
                fragment.totals().objects,
                fragment.totals().physical_rows,
            )?;
            if let Some(output) = output {
                write_catalog_atomically(output, &audit)?;
            }
            println!(
                concat!(
                    "audit={} complete={} manifest_sources={} published_sources={} ",
                    "objects={} rows={} duplicates={} rejected={} problems={}"
                ),
                audit.audit_id,
                audit.is_complete(),
                audit.totals().manifest_sources,
                audit.totals().published_sources,
                audit.totals().active_raw_objects,
                audit.totals().active_raw_rows,
                audit.totals().duplicate_events,
                audit.totals().rejected_events,
                audit.problems().len()
            );
            for problem in audit.problems() {
                eprintln!("{}: {}", problem.source_name, problem.reason);
            }
            if !audit.is_complete() {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

fn print_manifest(manifest: &HistoricalSourceManifest) {
    println!(
        "manifest={} max_segment={} selection_high_water_gzip={} sources={} bytes={}",
        manifest.manifest_id,
        manifest.max_segment_number(),
        manifest.selection_high_water_gzip(),
        manifest.totals().sources,
        manifest.totals().source_bytes
    );
}
