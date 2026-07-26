//! Run resumable local publication for sealed historical notepack segments.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use pensieve_lake::{
    CampaignConfig, Inventory, LocalObjectStore, Publisher, S3Publisher, S3PublisherConfig,
    cleanup_published_local_artifacts, run_notepack_work_unit,
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
    /// Remove generated staging artifacts after durable publication.
    #[arg(long)]
    cleanup_published_staging: bool,
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
        let outcome = run_notepack_work_unit(&mut inventory, publisher.as_ref(), input, &config)
            .and_then(|summary| {
                let cleanup = args
                    .cleanup_published_staging
                    .then(|| {
                        cleanup_published_local_artifacts(
                            &inventory,
                            &summary.work_unit_id,
                            &config.staging_dir,
                        )
                    })
                    .transpose()?;
                Ok((summary, cleanup))
            });
        match outcome {
            Ok((summary, cleanup)) => {
                println!(
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
                );
                if let Some(cleanup) = cleanup {
                    println!(
                        "[{}/{}] {}: staging_cleanup_files={}, staging_cleanup_bytes={}",
                        index + 1,
                        inputs.len(),
                        input.display(),
                        cleanup.files,
                        cleanup.bytes,
                    );
                }
            }
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
    let compressed_representations: HashSet<_> = result
        .iter()
        .filter(|path| is_compressed_notepack_segment(path))
        .map(|path| path.with_extension(""))
        .collect();
    result.retain(|path| {
        !is_plain_notepack_segment(path) || !compressed_representations.contains(path)
    });
    Ok(result)
}

fn is_notepack_segment(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".notepack") || name.ends_with(".notepack.gz")
}

fn is_plain_notepack_segment(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".notepack"))
}

fn is_compressed_notepack_segment(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".notepack.gz"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_inputs_prefer_gzip_for_the_same_logical_segment() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for name in [
            "segment-000000001.notepack",
            "segment-000000001.notepack.gz",
            "segment-000000002.notepack",
            "segment-000000003.notepack.gz",
            "segment-000000004.notepack.open",
            "unrelated.txt",
        ] {
            fs::write(directory.path().join(name), []).expect("fixture file");
        }

        let inputs = expand_inputs(&[directory.path().to_owned()]).expect("expand directory");
        let names: Vec<_> = inputs
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "segment-000000001.notepack.gz",
                "segment-000000002.notepack",
                "segment-000000003.notepack.gz",
            ]
        );
    }

    #[test]
    fn explicit_inputs_also_prefer_gzip_and_deduplicate_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let plain = directory.path().join("segment-000000001.notepack");
        let gzip = directory.path().join("segment-000000001.notepack.gz");
        fs::write(&plain, []).expect("plain fixture");
        fs::write(&gzip, []).expect("gzip fixture");

        assert_eq!(
            expand_inputs(&[plain.clone(), gzip.clone(), plain]).expect("expand files"),
            vec![gzip]
        );
    }
}
