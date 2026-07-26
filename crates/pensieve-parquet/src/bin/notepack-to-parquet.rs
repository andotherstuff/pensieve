//! Convert one framed notepack segment into a canonical V1 Parquet file.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use pensieve_parquet::{
    ConversionSummary, DEFAULT_MAX_EVENT_BYTES, convert_segment,
    convert_segment_quarantining_invalid,
};

#[derive(Debug, Parser)]
#[command(about = "Convert one framed notepack segment to canonical V1 Parquet")]
struct Args {
    /// Input `.notepack` or `.notepack.gz` segment.
    input: PathBuf,
    /// New output `.parquet` file. Existing files are never overwritten.
    output: PathBuf,
    /// Maximum accepted notepack frame size in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_event_bytes: usize,
    /// Preserve invalid frames in this new notepack segment and continue.
    #[arg(long)]
    rejects: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    let started = Instant::now();
    let result = match args.rejects {
        Some(rejected_output) => convert_segment_quarantining_invalid(
            &args.input,
            &args.output,
            rejected_output,
            args.max_event_bytes,
        ),
        None => convert_segment(&args.input, &args.output, args.max_event_bytes),
    };
    match result {
        Ok(summary) => print_summary(summary, started.elapsed().as_secs_f64()),
        Err(error) => {
            eprintln!("conversion failed: {error}");
            std::process::exit(1);
        }
    }
}

fn print_summary(summary: ConversionSummary, elapsed_seconds: f64) {
    let event_rate = summary.input_events as f64 / elapsed_seconds;
    let input_mib = summary.input_file_bytes as f64 / (1024.0 * 1024.0);
    let input_rate = input_mib / elapsed_seconds;
    println!(
        concat!(
            "converted: input_events={}, output_rows={}, duplicates={}, rejected={}, row_groups={}, ",
            "input_bytes={}, output_bytes={}, elapsed_seconds={:.3}, ",
            "events_per_second={:.0}, input_mib_per_second={:.1}"
        ),
        summary.input_events,
        summary.output_rows,
        summary.duplicate_events,
        summary.rejected_events,
        summary.row_groups,
        summary.input_file_bytes,
        summary.output_file_bytes,
        elapsed_seconds,
        event_rate,
        input_rate,
    );
}
