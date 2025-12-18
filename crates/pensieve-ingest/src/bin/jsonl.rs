//! Backfill command for JSONL Nostr event files.
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

use anyhow::Result;
use clap::Parser;
use metrics::{counter, gauge};
use pensieve_core::metrics::{init_metrics, start_metrics_server};
use pensieve_ingest::{
    source::{EventSource, JsonlConfig, JsonlSource},
    ClickHouseConfig, ClickHouseIndexer, DedupeIndex, SealedSegment, SegmentConfig, SegmentWriter,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    total_events: usize,
    valid_events: usize,
    invalid_events: usize,
    duplicate_events: usize,
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
    println!("Total events:      {:>12}", stats.total_events);
    println!("Valid events:      {:>12}", stats.valid_events);
    println!("Duplicate events:  {:>12}", stats.duplicate_events);
    println!("Invalid events:    {:>12}", stats.invalid_events);
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

    // Create JSONL source
    let source_config = JsonlConfig {
        input: args.input.clone(),
        skip_validation: args.skip_validation,
        continue_on_error: args.continue_on_error,
        limit: args.limit,
        progress_interval: args.progress_interval,
    };
    let mut source = JsonlSource::new(source_config);

    // Track duplicates in handler (source can't see them)
    let duplicate_count = Arc::new(AtomicUsize::new(0));
    let duplicate_count_ref = duplicate_count.clone();

    // Create handler closure
    let segment_writer_ref = segment_writer.clone();
    let dedupe_ref = dedupe.clone();
    let handler =
        move |packed_event: pensieve_ingest::PackedEvent| -> pensieve_ingest::Result<bool> {
            // Dedupe check
            if let Some(ref dedupe) = dedupe_ref
                && !dedupe.check_and_mark_pending(&packed_event.event_id)?
            {
                duplicate_count_ref.fetch_add(1, Ordering::Relaxed);
                return Ok(true); // Duplicate, continue
            }

            // Write to segment
            segment_writer_ref.write(packed_event)?;
            Ok(true) // Continue
        };

    // Run the source
    let source_stats = source.process(handler)?;

    // Update stats from source
    stats.total_events = source_stats.total_events;
    stats.invalid_events = source_stats.invalid_events;
    stats.duplicate_events = duplicate_count.load(Ordering::Relaxed);
    // valid_events = events that passed validation AND were not duplicates
    stats.valid_events = source_stats
        .valid_events
        .saturating_sub(stats.duplicate_events);
    if let Some(files) = source_stats.source_metadata.files_processed {
        stats.files_processed = files;
    }
    if let Some(bytes) = source_stats.source_metadata.bytes_read {
        stats.total_json_bytes = bytes;
    }

    // Finalize pipeline
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
    counter!("backfill_bytes_total", "type" => "json_raw").absolute(stats.total_json_bytes as u64);
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

    #[test]
    fn test_stats_default() {
        let stats = Stats::default();
        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.total_events, 0);
        assert_eq!(stats.valid_events, 0);
        assert_eq!(stats.invalid_events, 0);
        assert_eq!(stats.duplicate_events, 0);
    }

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
            total_events: 900,
            valid_events: 800,
            invalid_events: 50,
            duplicate_events: 50,
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
