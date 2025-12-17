//! Backfill adapter for JSONL Nostr event files.
//!
//! This tool reads JSONL files where each line is a JSON Nostr event,
//! validates each event's ID and signature, deduplicates, and writes
//! valid events to notepack segments with optional ClickHouse indexing.
//!
//! # Pipeline
//!
//! ```text
//! [JSONL Files] → [Validation] → [Dedupe] → [SegmentWriter] → [ClickHouse]
//!                                    ↓              ↓
//!                                RocksDB        Segments/
//! ```
//!
//! # Usage
//!
//! ```bash
//! # Single file
//! backfill-jsonl -i events.jsonl -o ./segments/
//!
//! # Directory of JSONL files
//! backfill-jsonl -i ./jsonl-data/ -o ./segments/ \
//!     --rocksdb-path ./data/dedupe \
//!     --clickhouse-url http://localhost:8123
//!
//! # With metrics
//! backfill-jsonl -i ./data/ -o ./segments/ --metrics-port 9091
//! ```

use anyhow::{bail, Context, Result};
use clap::Parser;
use metrics::{counter, gauge};
use notepack::{pack_note_into, NoteBuf};
use pensieve_core::{
    metrics::{init_metrics, start_metrics_server},
    pack_event_binary_into, validate_event,
};
use pensieve_ingest::{
    clickhouse::{ClickHouseConfig, ClickHouseIndexer},
    dedupe::DedupeIndex,
    segment::{PackedEvent, SealedSegment, SegmentConfig, SegmentWriter},
};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Backfill Nostr events from JSONL files to notepack archive format.
#[derive(Parser, Debug)]
#[command(name = "backfill-jsonl")]
#[command(about = "Convert JSONL Nostr events to notepack archive format with full pipeline")]
struct Args {
    /// Input JSONL file or directory path
    #[arg(short, long)]
    input: PathBuf,

    /// Output directory for notepack segments
    #[arg(short, long)]
    output: PathBuf,

    /// RocksDB path for dedupe index (enables deduplication)
    #[arg(long)]
    rocksdb_path: Option<PathBuf>,

    /// ClickHouse URL (enables indexing, e.g., http://localhost:8123)
    #[arg(long)]
    clickhouse_url: Option<String>,

    /// ClickHouse database name
    #[arg(long, default_value = "nostr")]
    clickhouse_db: String,

    /// Maximum segment size in bytes before sealing
    #[arg(long, default_value = "268435456")] // 256 MB
    segment_size: usize,

    /// Skip validation (only check JSON parsing, not ID/signature)
    #[arg(long, default_value = "false")]
    skip_validation: bool,

    /// Continue on validation errors (log and skip invalid events)
    #[arg(long, default_value = "true")]
    continue_on_error: bool,

    /// Limit number of files to process (for testing)
    #[arg(long)]
    limit: Option<usize>,

    /// Print progress every N events
    #[arg(long, default_value = "100000")]
    progress_interval: usize,

    /// Disable gzip compression of output segments
    #[arg(long, default_value = "false")]
    no_compress: bool,

    /// Metrics HTTP server port (0 to disable)
    #[arg(long, default_value = "9091")]
    metrics_port: u16,
}

/// Statistics collected during processing.
#[derive(Default)]
struct Stats {
    files_processed: usize,
    total_lines: usize,
    total_events: usize,
    valid_events: usize,
    invalid_events: usize,
    duplicate_events: usize,
    json_errors: usize,
    validation_errors: usize,
    notepack_errors: usize,
    segments_sealed: usize,
    total_json_bytes: usize,
    total_notepack_bytes: usize,
    compressed_notepack_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let args = Args::parse();

    // Initialize metrics and start server (if enabled)
    if args.metrics_port > 0 {
        let metrics_handle = init_metrics();
        start_metrics_server(args.metrics_port, metrics_handle).await?;
        gauge!("backfill_running").set(1.0);
    }

    let start = Instant::now();
    let stats = process(&args)?;
    let elapsed = start.elapsed();

    // Mark as no longer running
    gauge!("backfill_running").set(0.0);

    // Print summary
    print_summary(&args, &stats, elapsed);

    Ok(())
}

fn print_summary(args: &Args, stats: &Stats, elapsed: std::time::Duration) {
    println!("\n══════════════════════════════════════════════════════════════════");
    println!("SUMMARY");
    println!("══════════════════════════════════════════════════════════════════\n");

    println!("Input:       {}", args.input.display());
    println!("Output:      {}", args.output.display());
    if let Some(ref rocksdb) = args.rocksdb_path {
        println!("RocksDB:     {}", rocksdb.display());
    }
    if let Some(ref ch) = args.clickhouse_url {
        println!("ClickHouse:  {}", ch);
    }
    println!();
    println!("Files processed:   {:>12}", stats.files_processed);
    println!("Total lines:       {:>12}", stats.total_lines);
    println!("Total events:      {:>12}", stats.total_events);
    println!("Valid events:      {:>12}", stats.valid_events);
    println!("Duplicate events:  {:>12}", stats.duplicate_events);
    println!("Invalid events:    {:>12}", stats.invalid_events);
    if stats.invalid_events > 0 {
        println!("  - JSON errors:     {:>10}", stats.json_errors);
        println!("  - Validation:      {:>10}", stats.validation_errors);
        println!("  - Notepack:        {:>10}", stats.notepack_errors);
    }
    println!();
    println!("Segments sealed:   {:>12}", stats.segments_sealed);
    println!();
    println!("╭─────────────────────────────────────────────────────────────────╮");
    println!("│ SIZE COMPARISON                                                 │");
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!(
        "│ JSON input:          {:>15} bytes                      │",
        stats.total_json_bytes
    );
    println!(
        "│ Notepack (raw):      {:>15} bytes                      │",
        stats.total_notepack_bytes
    );
    println!(
        "│ Notepack (gzipped):  {:>15} bytes                      │",
        stats.compressed_notepack_bytes
    );
    println!("├─────────────────────────────────────────────────────────────────┤");

    if stats.total_json_bytes > 0 && stats.total_notepack_bytes > 0 {
        let notepack_vs_json = stats.total_notepack_bytes as f64 / stats.total_json_bytes as f64;
        let reduction = (1.0 - notepack_vs_json) * 100.0;
        println!(
            "│ Raw: Notepack vs JSON:     {:>6.1}% smaller                     │",
            reduction
        );
    }

    if stats.total_json_bytes > 0 && stats.compressed_notepack_bytes > 0 {
        let gz_vs_json = stats.compressed_notepack_bytes as f64 / stats.total_json_bytes as f64;
        let reduction = (1.0 - gz_vs_json) * 100.0;
        println!(
            "│ Gzip: Notepack vs JSON:    {:>6.1}% smaller                     │",
            reduction
        );
    }

    println!("╰─────────────────────────────────────────────────────────────────╯");
    println!();
    println!("Elapsed time:      {:>12.2?}", elapsed);

    if stats.valid_events > 0 {
        let events_per_sec = stats.valid_events as f64 / elapsed.as_secs_f64();
        println!("Throughput:        {:>12.0} events/sec", events_per_sec);
    }

    println!();
}

// ============================================================================
// Processing
// ============================================================================

fn process(args: &Args) -> Result<Stats> {
    let mut stats = Stats::default();
    let process_start = Instant::now();

    // Initialize pipeline components
    let (segment_writer, dedupe, indexer_handle) = init_pipeline(args)?;

    // Collect input files
    let files = collect_files(&args.input, args.limit)?;
    info!("Found {} JSONL files to process", files.len());

    for (file_idx, file_path) in files.iter().enumerate() {
        info!(
            "[{}/{}] Processing: {}",
            file_idx + 1,
            files.len(),
            file_path.display()
        );

        let file_size = fs::metadata(file_path)?.len() as usize;
        stats.total_json_bytes += file_size;

        match process_file(file_path, &segment_writer, dedupe.as_ref(), args, &mut stats) {
            Ok(()) => {
                stats.files_processed += 1;
                // Update metrics after each file
                record_metrics(&stats, process_start.elapsed().as_secs_f64());
            }
            Err(e) => {
                warn!("Error processing {}: {}", file_path.display(), e);
                if !args.continue_on_error {
                    return Err(e);
                }
            }
        }
    }

    // Seal any remaining segment
    finalize_pipeline(&segment_writer, dedupe.as_ref(), &mut stats)?;

    // Final metrics update
    record_metrics(&stats, process_start.elapsed().as_secs_f64());

    // Wait for ClickHouse indexer to finish processing the queue
    if let Some(handle) = indexer_handle {
        info!("Waiting for ClickHouse indexer to finish...");
        drop(segment_writer);
        if let Err(e) = handle.join() {
            warn!("ClickHouse indexer thread panicked: {:?}", e);
        }
        info!("ClickHouse indexer finished");
    }

    Ok(stats)
}

fn collect_files(input: &PathBuf, limit: Option<usize>) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if input.is_file() {
        files.push(input.clone());
    } else if input.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(input)
            .with_context(|| format!("Failed to read directory: {}", input.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext == "jsonl" || ext == "json" || ext == "ndjson")
            })
            .map(|e| e.path())
            .collect();

        // Sort for deterministic processing order
        entries.sort();
        files = entries;
    } else {
        bail!("Input path does not exist: {}", input.display());
    }

    // Apply limit if specified
    if let Some(limit) = limit {
        files.truncate(limit);
    }

    Ok(files)
}

// ============================================================================
// Pipeline
// ============================================================================

/// Result of pipeline initialization: (segment writer, dedupe index, clickhouse indexer handle).
type PipelineComponents = (
    Arc<SegmentWriter>,
    Option<Arc<DedupeIndex>>,
    Option<std::thread::JoinHandle<()>>,
);

fn init_pipeline(args: &Args) -> Result<PipelineComponents> {
    // Initialize dedupe index (optional)
    let dedupe = if let Some(ref rocksdb_path) = args.rocksdb_path {
        info!("Opening dedupe index at {}", rocksdb_path.display());
        Some(Arc::new(DedupeIndex::open(rocksdb_path)?))
    } else {
        warn!("Running without deduplication (no --rocksdb-path specified)");
        None
    };

    // Set up ClickHouse channel (optional)
    let (sealed_sender, sealed_receiver) = if args.clickhouse_url.is_some() {
        let (tx, rx) = crossbeam_channel::unbounded::<SealedSegment>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Initialize segment writer
    let segment_config = SegmentConfig {
        output_dir: args.output.clone(),
        max_segment_size: args.segment_size,
        segment_prefix: "segment".to_string(),
        compress: !args.no_compress,
    };
    let segment_writer = Arc::new(SegmentWriter::new(segment_config, sealed_sender)?);

    // Initialize ClickHouse indexer (optional)
    let indexer_handle = if let (Some(ch_url), Some(receiver)) =
        (&args.clickhouse_url, sealed_receiver)
    {
        info!("Starting ClickHouse indexer for {}", ch_url);
        let ch_config = ClickHouseConfig {
            url: ch_url.clone(),
            database: args.clickhouse_db.clone(),
            table: "events_local".to_string(),
            batch_size: 10000,
        };
        let indexer = ClickHouseIndexer::new(ch_config)?;
        Some(indexer.start(receiver))
    } else {
        None
    };

    Ok((segment_writer, dedupe, indexer_handle))
}

fn finalize_pipeline(
    segment_writer: &Arc<SegmentWriter>,
    dedupe: Option<&Arc<DedupeIndex>>,
    stats: &mut Stats,
) -> Result<()> {
    // Seal any remaining segment
    if let Some(sealed) = segment_writer.seal()? {
        info!(
            "Sealed final segment {}: {} events",
            sealed.segment_number, sealed.event_count
        );
    }

    // Update stats from segment writer
    let seg_stats = segment_writer.stats();
    stats.segments_sealed = seg_stats.segment_number as usize;
    stats.total_notepack_bytes = seg_stats.total_bytes;
    stats.compressed_notepack_bytes = seg_stats.total_compressed_bytes;

    // Flush dedupe index
    if let Some(dedupe) = dedupe {
        dedupe.flush()?;
        let dedupe_stats = dedupe.stats();
        info!("Dedupe index: ~{} keys", dedupe_stats.approximate_keys);
    }

    Ok(())
}

fn process_file(
    file_path: &PathBuf,
    segment_writer: &Arc<SegmentWriter>,
    dedupe: Option<&Arc<DedupeIndex>>,
    args: &Args,
    stats: &mut Stats,
) -> Result<()> {
    let file = File::open(file_path)
        .with_context(|| format!("Failed to open file: {}", file_path.display()))?;
    let reader = BufReader::new(file);

    // Reusable buffer for notepack encoding
    let mut pack_buf: Vec<u8> = Vec::with_capacity(4096);

    for (line_num, line_result) in reader.lines().enumerate() {
        stats.total_lines += 1;

        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                warn!("Line {}: I/O error: {}", line_num + 1, e);
                stats.invalid_events += 1;
                stats.json_errors += 1;
                if args.continue_on_error {
                    continue;
                } else {
                    bail!("I/O error at line {}: {}", line_num + 1, e);
                }
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        stats.total_events += 1;
        pack_buf.clear();

        // Parse and validate
        let event_id = if args.skip_validation {
            match serde_json::from_str::<NoteBuf>(&line) {
                Ok(note) => {
                    match pack_note_into(&note, &mut pack_buf) {
                        Ok(_) => {}
                        Err(e) => {
                            warn!("Line {}: Notepack encoding error: {}", line_num + 1, e);
                            stats.invalid_events += 1;
                            stats.notepack_errors += 1;
                            if args.continue_on_error {
                                continue;
                            } else {
                                bail!("Notepack encoding error at line {}: {}", line_num + 1, e);
                            }
                        }
                    }
                    // Parse event ID from the note
                    hex_to_bytes32(&note.id)?
                }
                Err(e) => {
                    warn!("Line {}: JSON parse error: {}", line_num + 1, e);
                    stats.invalid_events += 1;
                    stats.json_errors += 1;
                    if args.continue_on_error {
                        continue;
                    } else {
                        bail!("JSON parse error at line {}: {}", line_num + 1, e);
                    }
                }
            }
        } else {
            match validate_event(&line) {
                Ok(event) => {
                    let id_bytes: [u8; 32] = *event.id.as_bytes();
                    pack_event_binary_into(&event, &mut pack_buf);
                    id_bytes
                }
                Err(e) => {
                    warn!("Line {}: Validation error: {}", line_num + 1, e);
                    stats.invalid_events += 1;
                    stats.validation_errors += 1;
                    if args.continue_on_error {
                        continue;
                    } else {
                        bail!("Validation error at line {}: {}", line_num + 1, e);
                    }
                }
            }
        };

        // Dedupe check
        if let Some(dedupe) = dedupe
            && !dedupe.check_and_mark_pending(&event_id)?
        {
            stats.duplicate_events += 1;
            continue;
        }

        // Write to segment
        let packed_event = PackedEvent {
            event_id,
            data: pack_buf.clone(),
        };

        if segment_writer.write(packed_event)? {
            stats.segments_sealed += 1;
        }

        stats.valid_events += 1;

        // Progress reporting
        if stats.total_events.is_multiple_of(args.progress_interval) {
            info!(
                "Progress: {} events, {} valid, {} duplicates, {} invalid, {} segments",
                stats.total_events,
                stats.valid_events,
                stats.duplicate_events,
                stats.invalid_events,
                stats.segments_sealed
            );
        }
    }

    Ok(())
}

fn hex_to_bytes32(hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex).context("Invalid hex string")?;
    if bytes.len() != 32 {
        bail!("Expected 32 bytes, got {}", bytes.len());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

// ============================================================================
// Metrics
// ============================================================================

/// Record metrics from current stats.
fn record_metrics(stats: &Stats, elapsed_secs: f64) {
    // Counters
    counter!("backfill_events_total").absolute(stats.total_events as u64);
    counter!("backfill_events_valid_total").absolute(stats.valid_events as u64);
    counter!("backfill_events_duplicate_total").absolute(stats.duplicate_events as u64);
    counter!("backfill_events_invalid_total").absolute(stats.invalid_events as u64);
    counter!("backfill_files_total").absolute(stats.files_processed as u64);
    counter!("backfill_segments_sealed_total").absolute(stats.segments_sealed as u64);

    // Bytes by type
    counter!("backfill_bytes_total", "type" => "json_raw")
        .absolute(stats.total_json_bytes as u64);
    counter!("backfill_bytes_total", "type" => "notepack_raw")
        .absolute(stats.total_notepack_bytes as u64);
    counter!("backfill_bytes_total", "type" => "notepack_compressed")
        .absolute(stats.compressed_notepack_bytes as u64);

    // Gauges
    if elapsed_secs > 0.0 {
        let events_per_sec = stats.valid_events as f64 / elapsed_secs;
        gauge!("backfill_events_per_second").set(events_per_sec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // =========================================================================
    // hex_to_bytes32 tests
    // =========================================================================

    #[test]
    fn test_hex_to_bytes32_valid() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let result = hex_to_bytes32(hex).unwrap();
        assert_eq!(result[31], 1);
        assert_eq!(result[0], 0);
    }

    #[test]
    fn test_hex_to_bytes32_all_zeros() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = hex_to_bytes32(hex).unwrap();
        assert!(result.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_hex_to_bytes32_all_ff() {
        let hex = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let result = hex_to_bytes32(hex).unwrap();
        assert!(result.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn test_hex_to_bytes32_too_short() {
        let hex = "0001020304";
        let result = hex_to_bytes32(hex);
        assert!(result.is_err());
    }

    #[test]
    fn test_hex_to_bytes32_too_long() {
        let hex = "00000000000000000000000000000000000000000000000000000000000000000000";
        let result = hex_to_bytes32(hex);
        assert!(result.is_err());
    }

    #[test]
    fn test_hex_to_bytes32_invalid_chars() {
        let hex = "000000000000000000000000000000000000000000000000000000000000gggg";
        let result = hex_to_bytes32(hex);
        assert!(result.is_err());
    }

    #[test]
    fn test_hex_to_bytes32_empty() {
        let result = hex_to_bytes32("");
        assert!(result.is_err());
    }

    // =========================================================================
    // collect_files tests
    // =========================================================================

    #[test]
    fn test_collect_files_single_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.jsonl");
        File::create(&file_path).unwrap();

        let files = collect_files(&file_path, None).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], file_path);
    }

    #[test]
    fn test_collect_files_directory() {
        let tmp = TempDir::new().unwrap();
        File::create(tmp.path().join("a.jsonl")).unwrap();
        File::create(tmp.path().join("b.jsonl")).unwrap();
        File::create(tmp.path().join("c.json")).unwrap();
        File::create(tmp.path().join("d.ndjson")).unwrap();
        File::create(tmp.path().join("e.txt")).unwrap(); // Should be ignored

        let files = collect_files(&tmp.path().to_path_buf(), None).unwrap();
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn test_collect_files_sorted_order() {
        let tmp = TempDir::new().unwrap();
        File::create(tmp.path().join("z.jsonl")).unwrap();
        File::create(tmp.path().join("a.jsonl")).unwrap();
        File::create(tmp.path().join("m.jsonl")).unwrap();

        let files = collect_files(&tmp.path().to_path_buf(), None).unwrap();
        assert_eq!(files.len(), 3);
        assert!(files[0].file_name().unwrap() == "a.jsonl");
        assert!(files[1].file_name().unwrap() == "m.jsonl");
        assert!(files[2].file_name().unwrap() == "z.jsonl");
    }

    #[test]
    fn test_collect_files_with_limit() {
        let tmp = TempDir::new().unwrap();
        for i in 0..10 {
            File::create(tmp.path().join(format!("{:02}.jsonl", i))).unwrap();
        }

        let files = collect_files(&tmp.path().to_path_buf(), Some(3)).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_collect_files_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let files = collect_files(&tmp.path().to_path_buf(), None).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_files_nonexistent_path() {
        let result = collect_files(&PathBuf::from("/nonexistent/path"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_collect_files_ignores_subdirectories() {
        let tmp = TempDir::new().unwrap();
        File::create(tmp.path().join("test.jsonl")).unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        File::create(tmp.path().join("subdir/nested.jsonl")).unwrap();

        let files = collect_files(&tmp.path().to_path_buf(), None).unwrap();
        // Should only find the top-level file
        assert_eq!(files.len(), 1);
    }

    // =========================================================================
    // Stats tests
    // =========================================================================

    #[test]
    fn test_stats_default() {
        let stats = Stats::default();
        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.total_lines, 0);
        assert_eq!(stats.total_events, 0);
        assert_eq!(stats.valid_events, 0);
        assert_eq!(stats.invalid_events, 0);
        assert_eq!(stats.duplicate_events, 0);
    }

    #[test]
    fn test_stats_mutation() {
        let stats = Stats {
            total_events: 100,
            valid_events: 80,
            duplicate_events: 15,
            invalid_events: 5,
            ..Default::default()
        };

        assert_eq!(
            stats.valid_events + stats.duplicate_events + stats.invalid_events,
            100
        );
    }

    #[test]
    fn test_collect_files_extensions() {
        let tmp = TempDir::new().unwrap();

        // Create files with various extensions
        File::create(tmp.path().join("a.jsonl")).unwrap();
        File::create(tmp.path().join("b.json")).unwrap();
        File::create(tmp.path().join("c.ndjson")).unwrap();
        File::create(tmp.path().join("d.csv")).unwrap();
        File::create(tmp.path().join("e.txt")).unwrap();
        File::create(tmp.path().join("f.jsonl.bak")).unwrap();

        let files = collect_files(&tmp.path().to_path_buf(), None).unwrap();

        // Should only match .jsonl, .json, .ndjson
        assert_eq!(files.len(), 3);

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"a.jsonl"));
        assert!(names.contains(&"b.json"));
        assert!(names.contains(&"c.ndjson"));
    }

    #[test]
    fn test_collect_files_limit_zero() {
        let tmp = TempDir::new().unwrap();
        File::create(tmp.path().join("test.jsonl")).unwrap();

        let files = collect_files(&tmp.path().to_path_buf(), Some(0)).unwrap();
        assert!(files.is_empty());
    }

    // =========================================================================
    // print_summary tests (smoke test - just ensure it doesn't panic)
    // =========================================================================

    #[test]
    fn test_print_summary_does_not_panic() {
        let args = Args {
            input: PathBuf::from("/test/input"),
            output: PathBuf::from("/test/output"),
            rocksdb_path: Some(PathBuf::from("/test/rocksdb")),
            clickhouse_url: Some("http://localhost:8123".to_string()),
            clickhouse_db: "nostr".to_string(),
            segment_size: 256 * 1024 * 1024,
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100000,
            no_compress: false,
            metrics_port: 9091,
        };

        let stats = Stats {
            files_processed: 10,
            total_lines: 1000,
            total_events: 900,
            valid_events: 800,
            invalid_events: 50,
            duplicate_events: 50,
            json_errors: 20,
            validation_errors: 25,
            notepack_errors: 5,
            segments_sealed: 3,
            total_json_bytes: 1_000_000,
            total_notepack_bytes: 500_000,
            compressed_notepack_bytes: 200_000,
        };

        let elapsed = std::time::Duration::from_secs(10);

        // This should not panic
        print_summary(&args, &stats, elapsed);
    }

    #[test]
    fn test_print_summary_zero_values() {
        let args = Args {
            input: PathBuf::from("/test"),
            output: PathBuf::from("/out"),
            rocksdb_path: None,
            clickhouse_url: None,
            clickhouse_db: "nostr".to_string(),
            segment_size: 256 * 1024 * 1024,
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100000,
            no_compress: false,
            metrics_port: 0,
        };

        let stats = Stats::default();
        let elapsed = std::time::Duration::from_secs(0);

        // Should handle zeros gracefully
        print_summary(&args, &stats, elapsed);
    }
}
