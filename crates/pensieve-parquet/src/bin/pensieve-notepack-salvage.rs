//! Create an evidence-preserving repair bundle for a terminally truncated segment.

use std::path::PathBuf;

use clap::Parser;
use pensieve_parquet::{DEFAULT_MAX_EVENT_BYTES, salvage_truncated_segment};

#[derive(Debug, Parser)]
#[command(about = "Salvage the complete prefix of a terminally truncated notepack segment")]
struct Args {
    /// Original plain or gzip notepack segment.
    input: PathBuf,
    /// New bundle directory; an existing destination is never replaced.
    output_directory: PathBuf,
    /// Maximum accepted declared event frame size.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_event_bytes: usize,
}

fn main() {
    let args = Args::parse();
    match salvage_truncated_segment(args.input, &args.output_directory, args.max_event_bytes) {
        Ok(report) => println!(
            concat!(
                "report={} source={} source_sha256={} complete_frames={} valid={} rejected={} ",
                "truncated_frame={} salvaged_sha256={} bundle={}"
            ),
            report.report_id,
            report.source_name(),
            report.source_sha256(),
            report.complete_frames(),
            report.valid_events(),
            report.rejected_events(),
            report.truncated_frame_index(),
            report.salvaged_segment_sha256(),
            args.output_directory.display()
        ),
        Err(error) => {
            eprintln!("notepack salvage failed: {error}");
            std::process::exit(1);
        }
    }
}
