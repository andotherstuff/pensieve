//! Run resumable local publication for sealed historical notepack segments.

use std::fs;
use std::path::PathBuf;

use clap::Parser;
use pensieve_lake::{
    CampaignConfig, Inventory, LocalObjectStore, Publisher, S3Publisher, S3PublisherConfig,
    run_notepack_work_unit,
};
use pensieve_parquet::DEFAULT_MAX_EVENT_BYTES;

#[derive(Debug, Parser)]
#[command(about = "Convert sealed notepack work units into an inventoried Parquet lake")]
struct Args {
    /// Input segment files or directories containing sealed segments.
    #[arg(required = true)]
    input: Vec<PathBuf>,
    /// Durable SQLite work-unit journal and object inventory.
    #[arg(long)]
    state_db: PathBuf,
    /// Durable local directory for generated and validated artifacts.
    #[arg(long)]
    staging_dir: PathBuf,
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
    /// Target represented bytes per Parquet object.
    #[arg(long, default_value_t = pensieve_lake::DEFAULT_TARGET_UNCOMPRESSED_BYTES)]
    target_uncompressed_bytes: usize,
    /// Maximum accepted notepack frame size.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_event_bytes: usize,
    /// Continue processing later work units after an error.
    #[arg(long)]
    continue_on_error: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("campaign failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let inputs = expand_inputs(&args.input)?;
    let mut inventory = Inventory::open(&args.state_db)?;
    let publisher: Box<dyn Publisher> = if let Some(root) = args.lake_dir {
        Box::new(LocalObjectStore::new(root)?)
    } else {
        Box::new(S3Publisher::from_environment(S3PublisherConfig {
            bucket: args
                .s3_bucket
                .expect("clap requires one publication target"),
            region: args.s3_region,
            endpoint_url: args.s3_endpoint_url,
            force_path_style: args.s3_force_path_style,
        })?)
    };
    let config = CampaignConfig {
        staging_dir: args.staging_dir,
        object_prefix: args.object_prefix,
        target_uncompressed_bytes: args.target_uncompressed_bytes,
        max_event_bytes: args.max_event_bytes,
    };

    let mut failures = 0usize;
    for (index, input) in inputs.iter().enumerate() {
        match run_notepack_work_unit(&mut inventory, publisher.as_ref(), input, &config) {
            Ok(summary) => println!(
                concat!(
                    "[{}/{}] {}: state={}, input={}, rows={}, rejected={}, ",
                    "parquet_objects={}, resumed={}"
                ),
                index + 1,
                inputs.len(),
                input.display(),
                summary.state,
                summary.input_events,
                summary.output_rows,
                summary.rejected_events,
                summary.parquet_objects,
                summary.resumed,
            ),
            Err(error) => {
                failures += 1;
                eprintln!(
                    "[{}/{}] {}: {error}",
                    index + 1,
                    inputs.len(),
                    input.display()
                );
                if !args.continue_on_error {
                    return Err(error.into());
                }
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} work unit(s) failed").into());
    }
    Ok(())
}

fn expand_inputs(inputs: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for input in inputs {
        if input.is_dir() {
            for entry in fs::read_dir(input)? {
                let path = entry?.path();
                if path.is_file() && is_notepack_segment(&path) {
                    result.push(path);
                }
            }
        } else {
            result.push(input.clone());
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn is_notepack_segment(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".notepack") || name.ends_with(".notepack.gz")
}
