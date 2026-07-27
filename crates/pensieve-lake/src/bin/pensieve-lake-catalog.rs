//! Export, merge, and verify deterministic active-file lake catalogs.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pensieve_lake::{
    ActiveRawFragment, Inventory, LocalObjectStore, Publisher, S3Publisher, S3PublisherConfig,
    merge_active_raw_fragments, read_catalog_fragment, read_catalog_snapshot, sha256_file,
    write_catalog_atomically,
};

#[derive(Debug, Parser)]
#[command(about = "Build deterministic active-file snapshots from lake inventories")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Export one writer's published work and active raw objects.
    Export {
        /// Existing SQLite inventory to read without modifying.
        #[arg(long)]
        inventory: PathBuf,
        /// Stable, unique identity for this writer inventory.
        #[arg(long)]
        inventory_id: String,
        /// Stable, non-secret identity shared by inventories using one object store.
        #[arg(long)]
        store_id: String,
        /// Fragment JSON destination.
        #[arg(long)]
        output: PathBuf,
    },
    /// Merge one or more portable fragments into a unified snapshot.
    Merge {
        /// Fragment JSON inputs.
        #[arg(long, required = true)]
        fragment: Vec<PathBuf>,
        /// Unified snapshot JSON destination.
        #[arg(long)]
        output: PathBuf,
    },
    /// Publish a validated snapshot at its immutable content-addressed key.
    Publish {
        /// Snapshot JSON to validate and publish.
        #[arg(long)]
        snapshot: PathBuf,
        /// Local immutable object-store root. Conflicts with --s3-bucket.
        #[arg(
            long,
            required_unless_present = "s3_bucket",
            conflicts_with = "s3_bucket"
        )]
        lake_dir: Option<PathBuf>,
        /// S3-compatible immutable object-store bucket. Conflicts with --lake-dir.
        #[arg(
            long,
            required_unless_present = "lake_dir",
            conflicts_with = "lake_dir"
        )]
        s3_bucket: Option<String>,
        /// Optional AWS region override for S3 publication.
        #[arg(long, requires = "s3_bucket")]
        s3_region: Option<String>,
        /// Optional endpoint for an S3-compatible provider.
        #[arg(long, requires = "s3_bucket")]
        s3_endpoint_url: Option<String>,
        /// Use path-style S3 bucket addressing.
        #[arg(long, requires = "s3_bucket")]
        s3_force_path_style: bool,
        /// Object-key prefix.
        #[arg(long, default_value = "nostr/v1")]
        object_prefix: String,
    },
    /// Verify a fragment or snapshot without changing it.
    Verify {
        /// Catalog JSON to verify.
        #[arg(long, conflicts_with = "snapshot")]
        fragment: Option<PathBuf>,
        /// Snapshot JSON to verify.
        #[arg(long, conflicts_with = "fragment")]
        snapshot: Option<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("lake catalog failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Args::parse().command {
        Command::Export {
            inventory,
            inventory_id,
            store_id,
            output,
        } => {
            let mut inventory = Inventory::open_read_only(inventory)?;
            let fragment = ActiveRawFragment::export(&mut inventory, inventory_id, store_id)?;
            write_catalog_atomically(output, &fragment)?;
            println!(
                "fragment={} inventory={} work_units={} objects={} rows={} bytes={}",
                fragment.fragment_id,
                fragment.inventory_id(),
                fragment.totals().work_units,
                fragment.totals().objects,
                fragment.totals().physical_rows,
                fragment.totals().object_bytes
            );
        }
        Command::Merge { fragment, output } => {
            let fragments = fragment
                .into_iter()
                .map(read_catalog_fragment)
                .collect::<pensieve_lake::Result<Vec<_>>>()?;
            let snapshot = merge_active_raw_fragments(fragments)?;
            write_catalog_atomically(output, &snapshot)?;
            println!(
                "snapshot={} store={} work_units={} objects={} rows={} bytes={}",
                snapshot.snapshot_id,
                snapshot.store_id(),
                snapshot.totals().work_units,
                snapshot.totals().objects,
                snapshot.totals().physical_rows,
                snapshot.totals().object_bytes
            );
        }
        Command::Publish {
            snapshot,
            lake_dir,
            s3_bucket,
            s3_region,
            s3_endpoint_url,
            s3_force_path_style,
            object_prefix,
        } => {
            let catalog = read_catalog_snapshot(&snapshot)?;
            let publisher: Box<dyn Publisher> = if let Some(root) = lake_dir {
                Box::new(LocalObjectStore::new(root)?)
            } else {
                Box::new(S3Publisher::from_environment(S3PublisherConfig {
                    bucket: s3_bucket.expect("clap requires one publication target"),
                    region: s3_region,
                    endpoint_url: s3_endpoint_url,
                    force_path_style: s3_force_path_style,
                })?)
            };
            let digest = catalog
                .snapshot_id
                .strip_prefix("sha256:")
                .expect("validated snapshot IDs have a sha256 prefix");
            let object_prefix = object_prefix.trim_matches('/');
            if object_prefix.is_empty() {
                return Err("--object-prefix must not be empty".into());
            }
            let key = format!("{object_prefix}/catalog/active-raw/{digest}.json");
            let byte_size = std::fs::metadata(&snapshot)?.len();
            let sha256 = sha256_file(&snapshot)?;
            let published = publisher.publish(&key, &snapshot, byte_size, &sha256)?;
            println!(
                "published snapshot={} key={} bytes={} file_sha256={}",
                catalog.snapshot_id, published.key, published.byte_size, published.sha256
            );
        }
        Command::Verify { fragment, snapshot } => match (fragment, snapshot) {
            (Some(path), None) => {
                let fragment = read_catalog_fragment(path)?;
                println!(
                    "valid fragment={} inventory={} objects={}",
                    fragment.fragment_id,
                    fragment.inventory_id(),
                    fragment.totals().objects
                );
            }
            (None, Some(path)) => {
                let snapshot = read_catalog_snapshot(path)?;
                println!(
                    "valid snapshot={} store={} objects={}",
                    snapshot.snapshot_id,
                    snapshot.store_id(),
                    snapshot.totals().objects
                );
            }
            _ => return Err("exactly one of --fragment or --snapshot is required".into()),
        },
    }
    Ok(())
}
