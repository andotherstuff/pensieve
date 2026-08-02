//! Compare a framed notepack source with its canonical V1 Parquet output.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Parser;
use pensieve_parquet::{
    CanonicalEvent, DEFAULT_MAX_EVENT_BYTES, prepare_canonical_events, read_validated_file,
    scan_segment,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Compare all seven Nostr fields between one source and Parquet outputs")]
struct Args {
    /// Plain or gzip-compressed length-prefixed notepack source.
    #[arg(long)]
    source: PathBuf,
    /// Canonical V1 Parquet output; repeat for multipart work units.
    #[arg(long, required = true)]
    parquet: Vec<PathBuf>,
    /// Maximum accepted notepack frame size.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_event_bytes: usize,
}

#[derive(Debug, Serialize)]
struct ComparisonSummary {
    source: PathBuf,
    source_frames: usize,
    source_valid_events: usize,
    source_rejected_events: usize,
    source_duplicate_events: usize,
    parquet_files: usize,
    parquet_rows: usize,
    equal: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Parquet comparison failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let scan = scan_segment(&args.source, args.max_event_bytes)?;
    let valid_events = scan.events.len();
    let source_rows = prepare_canonical_events(scan.events);
    let mut parquet_by_id = BTreeMap::<[u8; 32], CanonicalEvent>::new();
    let mut parquet_rows = 0usize;
    for path in &args.parquet {
        for row in read_validated_file(path)? {
            parquet_rows = parquet_rows
                .checked_add(1)
                .ok_or("Parquet row count overflow")?;
            if parquet_by_id.insert(*row.id(), row).is_some() {
                return Err(format!(
                    "duplicate event ID appears across Parquet outputs for {}",
                    path.display()
                )
                .into());
            }
        }
    }
    let mut output_rows: Vec<_> = parquet_by_id.into_values().collect();
    output_rows.sort_unstable_by(|left, right| {
        left.created_at()
            .cmp(&right.created_at())
            .then_with(|| left.id().cmp(right.id()))
    });

    let equal = source_rows == output_rows;
    let summary = ComparisonSummary {
        source: args.source,
        source_frames: valid_events + scan.rejected.len(),
        source_valid_events: valid_events,
        source_rejected_events: scan.rejected.len(),
        source_duplicate_events: valid_events.saturating_sub(source_rows.len()),
        parquet_files: args.parquet.len(),
        parquet_rows,
        equal,
    };
    println!("{}", serde_json::to_string(&summary)?);
    if !equal {
        report_first_difference(&source_rows, &output_rows);
        return Err("source and Parquet rows differ".into());
    }
    Ok(())
}

fn report_first_difference(source: &[CanonicalEvent], output: &[CanonicalEvent]) {
    let different = source
        .iter()
        .zip(output)
        .position(|(source, output)| source != output);
    match different {
        Some(index) => {
            let source = &source[index];
            let output = &output[index];
            eprintln!(
                concat!(
                    "first differing row={} source_id={} output_id={} fields:",
                    " id={} pubkey={} created_at={} kind={} tags={} content={} sig={}"
                ),
                index,
                hex::encode(source.id()),
                hex::encode(output.id()),
                source.id() == output.id(),
                source.pubkey() == output.pubkey(),
                source.created_at() == output.created_at(),
                source.kind() == output.kind(),
                source.tags() == output.tags(),
                source.content() == output.content(),
                source.signature() == output.signature(),
            );
        }
        None => eprintln!(
            "row-count mismatch after equal prefix: source={} output={}",
            source.len(),
            output.len()
        ),
    }
}
