//! Strict command-line validator for canonical V1 Parquet archive files.

use std::path::PathBuf;

use clap::Parser;
use pensieve_parquet::validate_file;

#[derive(Debug, Parser)]
#[command(about = "Strictly validate a canonical Nostr V1 Parquet archive")]
struct Args {
    /// Canonical Parquet archive file to validate.
    path: PathBuf,
}

fn main() {
    let args = Args::parse();
    match validate_file(&args.path) {
        Ok(report) => {
            println!(
                "valid canonical V1 archive: rows={}, row_groups={}, created_at={:?}..={:?}",
                report.rows, report.row_groups, report.min_created_at, report.max_created_at
            );
        }
        Err(error) => {
            eprintln!("invalid canonical V1 archive: {error}");
            std::process::exit(1);
        }
    }
}
