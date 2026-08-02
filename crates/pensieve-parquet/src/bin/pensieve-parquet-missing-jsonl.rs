//! Extract canonically valid source rows absent from existing Parquet outputs.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use clap::Parser;
use pensieve_parquet::{
    CanonicalEvent, DEFAULT_MAX_EVENT_BYTES, prepare_canonical_events, read_validated_file,
    scan_segment,
};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(about = "Write valid source events missing from canonical Parquet outputs as JSONL")]
struct Args {
    /// Plain or gzip-compressed length-prefixed notepack source.
    #[arg(long)]
    source: PathBuf,
    /// Canonical V1 Parquet output; repeat for multipart work units.
    #[arg(long, required = true)]
    parquet: Vec<PathBuf>,
    /// New JSONL file for source rows absent from every Parquet output.
    #[arg(long)]
    output: PathBuf,
    /// Maximum accepted notepack frame size.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_event_bytes: usize,
}

#[derive(Debug, Serialize)]
struct ExtractionSummary {
    source: PathBuf,
    source_frames: usize,
    source_valid_events: usize,
    source_rejected_events: usize,
    source_duplicate_events: usize,
    parquet_files: usize,
    parquet_rows: usize,
    missing_events: usize,
    output: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Parquet missing-row extraction failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let scan = scan_segment(&args.source, args.max_event_bytes)?;
    let valid_events = scan.events.len();
    let source_rows = prepare_canonical_events(scan.events);
    let source_by_id: BTreeMap<_, _> = source_rows
        .iter()
        .map(|event| (*event.id(), event))
        .collect();

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

    for (id, output) in &parquet_by_id {
        let Some(source) = source_by_id.get(id) else {
            return Err(format!(
                "Parquet output contains ID absent from source: {}",
                hex::encode(id)
            )
            .into());
        };
        if *source != output {
            return Err(format!(
                "Parquet fields differ from source for ID {}",
                hex::encode(id)
            )
            .into());
        }
    }

    let missing: Vec<_> = source_rows
        .iter()
        .filter(|event| !parquet_by_id.contains_key(event.id()))
        .collect();
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)?;
    let mut writer = BufWriter::new(file);
    for event in &missing {
        serde_json::to_writer(
            &mut writer,
            &json!({
                "id": hex::encode(event.id()),
                "pubkey": hex::encode(event.pubkey()),
                "created_at": event.created_at(),
                "kind": event.kind(),
                "tags": event.tags(),
                "content": event.content(),
                "sig": hex::encode(event.signature()),
            }),
        )?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;

    println!(
        "{}",
        serde_json::to_string(&ExtractionSummary {
            source: args.source,
            source_frames: valid_events + scan.rejected.len(),
            source_valid_events: valid_events,
            source_rejected_events: scan.rejected.len(),
            source_duplicate_events: valid_events.saturating_sub(source_rows.len()),
            parquet_files: args.parquet.len(),
            parquet_rows,
            missing_events: missing.len(),
            output: args.output,
        })?
    );
    Ok(())
}
