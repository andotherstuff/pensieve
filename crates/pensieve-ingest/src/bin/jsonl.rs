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
    ClickHouseConfig, ClickHouseIndexer, DedupeIndex, SealedSegment, SegmentConfig, SegmentWriter,
    source::{EventSource, JsonlConfig, JsonlSource},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
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

    /// Progress file path (tracks completed files for resume).
    /// Defaults to <output>/jsonl-backfill-progress.json if not specified.
    #[arg(long)]
    progress_file: Option<PathBuf>,

    /// Metrics HTTP server port (0 to disable)
    #[arg(long, default_value = "9091")]
    metrics_port: u16,

    /// Index existing segments to ClickHouse (skips JSONL processing).
    /// Provide the path to the segments directory.
    #[arg(long)]
    index_segments: Option<PathBuf>,

    /// Start indexing from this segment number (use with --index-segments)
    #[arg(long, default_value = "0")]
    start_segment: u32,
}

/// Statistics collected during processing.
#[derive(Default, Clone)]
struct Stats {
    files_processed: usize,
    files_skipped: usize,
    total_events: usize,
    valid_events: usize,
    invalid_events: usize,
    duplicate_events: usize,
    segments_sealed: usize,
    total_json_bytes: usize,
    total_notepack_bytes: usize,
    compressed_notepack_bytes: usize,
}

/// Progress tracking for resumable backfill.
#[derive(Debug, Serialize, Deserialize, Default)]
struct Progress {
    /// File paths that have been fully processed.
    completed_files: HashSet<String>,
}

impl Progress {
    fn load(path: &PathBuf) -> Result<Self> {
        if path.exists() {
            let contents = fs::read_to_string(path)?;
            Ok(serde_json::from_str(&contents)?)
        } else {
            Ok(Self::default())
        }
    }

    fn save(&self, path: &PathBuf) -> Result<()> {
        let contents = serde_json::to_string_pretty(self)?;
        fs::write(path, contents)?;
        Ok(())
    }

    fn is_completed(&self, file_path: &str) -> bool {
        self.completed_files.contains(file_path)
    }

    fn mark_completed(&mut self, file_path: &str) {
        self.completed_files.insert(file_path.to_string());
    }
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

    // Check for index-segments mode
    if let Some(ref segments_dir) = args.index_segments {
        let clickhouse_url = args.clickhouse_url.clone().ok_or_else(|| {
            anyhow::anyhow!("--clickhouse-url is required when using --index-segments")
        })?;

        let result = index_segments_mode(
            segments_dir,
            &clickhouse_url,
            &args.clickhouse_db,
            args.start_segment,
        ).await;

        gauge!("backfill_running").set(0.0);
        return result;
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

/// Index existing segment files to ClickHouse.
///
/// This mode reads notepack segment files from a directory and indexes them
/// to ClickHouse. Useful for re-indexing segments that failed to index due
/// to ClickHouse being unavailable.
async fn index_segments_mode(
    segments_dir: &PathBuf,
    clickhouse_url: &str,
    clickhouse_db: &str,
    start_segment: u32,
) -> Result<()> {
    tracing::info!(
        "Index segments mode: dir={}, start_segment={}, clickhouse={}",
        segments_dir.display(),
        start_segment,
        clickhouse_url
    );

    // Find all segment files
    let mut segment_files: Vec<PathBuf> = Vec::new();

    for entry in fs::read_dir(segments_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if filename.starts_with("segment-") && filename.contains(".notepack") {
                // Extract segment number
                if let Some(num_str) = filename
                    .strip_prefix("segment-")
                    .and_then(|s| s.split('.').next())
                {
                    if let Ok(num) = num_str.parse::<u32>() {
                        if num >= start_segment {
                            segment_files.push(path);
                        }
                    }
                }
            }
        }
    }

    // Sort by segment number
    segment_files.sort_by(|a, b| {
        let num_a = extract_segment_number(a).unwrap_or(0);
        let num_b = extract_segment_number(b).unwrap_or(0);
        num_a.cmp(&num_b)
    });

    tracing::info!("Found {} segments to index (starting from {})", segment_files.len(), start_segment);

    if segment_files.is_empty() {
        tracing::warn!("No segment files found matching criteria");
        return Ok(());
    }

    // Initialize ClickHouse indexer
    let ch_config = ClickHouseConfig {
        url: clickhouse_url.to_string(),
        database: clickhouse_db.to_string(),
        table: "events_local".to_string(),
        batch_size: 10000,
    };
    let indexer = ClickHouseIndexer::new(ch_config)?;

    // Health check
    if !indexer.health_check().await? {
        anyhow::bail!("ClickHouse health check failed");
    }
    tracing::info!("ClickHouse connection verified");

    let start_time = Instant::now();
    let mut total_events = 0usize;
    let mut segments_indexed = 0usize;
    let mut errors = 0usize;

    for (i, segment_path) in segment_files.iter().enumerate() {
        let segment_num = extract_segment_number(segment_path).unwrap_or(0);
        tracing::info!(
            "[{}/{}] Indexing segment {} from {}",
            i + 1,
            segment_files.len(),
            segment_num,
            segment_path.display()
        );

        match indexer.index_segment_file(segment_path).await {
            Ok(count) => {
                total_events += count;
                segments_indexed += 1;
                tracing::info!(
                    "Indexed segment {}: {} events (total: {})",
                    segment_num,
                    count,
                    total_events
                );
            }
            Err(e) => {
                tracing::error!("Failed to index segment {}: {}", segment_num, e);
                errors += 1;
                // Continue with next segment
            }
        }
    }

    let elapsed = start_time.elapsed();
    let events_per_sec = if elapsed.as_secs() > 0 {
        total_events as f64 / elapsed.as_secs_f64()
    } else {
        total_events as f64
    };

    println!("\n══════════════════════════════════════════════════════════════════");
    println!("INDEX SEGMENTS COMPLETE");
    println!("══════════════════════════════════════════════════════════════════\n");
    println!("Segments indexed:  {:>12}", segments_indexed);
    println!("Segments failed:   {:>12}", errors);
    println!("Total events:      {:>12}", total_events);
    println!("Time elapsed:      {:>12.2?}", elapsed);
    println!("Events/sec:        {:>12.0}", events_per_sec);

    if errors > 0 {
        anyhow::bail!("{} segments failed to index", errors);
    }

    Ok(())
}

/// Extract segment number from a path like "segment-1234.notepack.gz"
fn extract_segment_number(path: &PathBuf) -> Option<u32> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|s| s.strip_prefix("segment-"))
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse::<u32>().ok())
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
    if stats.files_skipped > 0 {
        println!(
            "Files skipped:     {:>12} (already done)",
            stats.files_skipped
        );
    }
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

    // Determine progress file path (default: <output>/jsonl-backfill-progress.json)
    let progress_file = args
        .progress_file
        .clone()
        .unwrap_or_else(|| args.output.join("jsonl-backfill-progress.json"));

    // Load progress (for resume)
    let mut progress = Progress::load(&progress_file)?;
    tracing::info!(
        "Loaded progress from {}: {} files already completed",
        progress_file.display(),
        progress.completed_files.len()
    );

    // Initialize pipeline components
    let (segment_writer, dedupe, indexer_handle) = init_pipeline(args)?;

    // Collect input files
    let files = collect_files(&args.input, args.limit)?;
    tracing::info!("Found {} JSONL files to process", files.len());

    // Track duplicates
    let duplicate_count = Arc::new(AtomicUsize::new(0));

    for (file_idx, file_path) in files.iter().enumerate() {
        let file_path_str = file_path.to_string_lossy().to_string();

        // Skip if already completed
        if progress.is_completed(&file_path_str) {
            tracing::info!(
                "[{}/{}] Skipping (already done): {}",
                file_idx + 1,
                files.len(),
                file_path.display()
            );
            stats.files_skipped += 1;
            continue;
        }

        tracing::info!(
            "[{}/{}] Processing: {}",
            file_idx + 1,
            files.len(),
            file_path.display()
        );

        // Get file size for stats
        let file_size = fs::metadata(file_path)?.len() as usize;
        stats.total_json_bytes += file_size;

        // Create JSONL source for this single file
        let source_config = JsonlConfig {
            input: file_path.clone(),
            skip_validation: args.skip_validation,
            continue_on_error: args.continue_on_error,
            limit: None, // No file limit, we're processing one file
            progress_interval: args.progress_interval,
        };
        let mut source = JsonlSource::new(source_config);

        // Create handler closure
        let segment_writer_ref = segment_writer.clone();
        let dedupe_ref = dedupe.clone();
        let duplicate_count_ref = duplicate_count.clone();
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

        // Process this file
        match source.process(handler) {
            Ok(source_stats) => {
                stats.total_events += source_stats.total_events;
                stats.invalid_events += source_stats.invalid_events;
                stats.valid_events += source_stats.valid_events;
                stats.files_processed += 1;

                // Mark as completed and save progress
                progress.mark_completed(&file_path_str);
                if let Err(e) = progress.save(&progress_file) {
                    tracing::warn!("Failed to save progress: {}", e);
                }

                // Update metrics after each file (calculate adjusted valid_events on the fly)
                let current_duplicates = duplicate_count.load(Ordering::Relaxed);
                let mut metrics_stats = stats.clone();
                metrics_stats.duplicate_events = current_duplicates;
                metrics_stats.valid_events =
                    metrics_stats.valid_events.saturating_sub(current_duplicates);
                record_metrics(&metrics_stats, process_start.elapsed().as_secs_f64());
            }
            Err(e) => {
                tracing::error!("Error processing {}: {}", file_path.display(), e);
                if !args.continue_on_error {
                    return Err(e.into());
                }
            }
        }
    }

    // Finalize pipeline
    finalize_pipeline(&segment_writer, dedupe.as_ref(), &mut stats)?;

    // Final stats adjustment: subtract duplicates from valid_events once
    stats.duplicate_events = duplicate_count.load(Ordering::Relaxed);
    stats.valid_events = stats.valid_events.saturating_sub(stats.duplicate_events);
    record_metrics(&stats, process_start.elapsed().as_secs_f64());

    // Wait for ClickHouse indexer to finish processing the queue
    if let Some(handle) = indexer_handle {
        tracing::info!("Waiting for ClickHouse indexer to finish...");
        drop(segment_writer);
        if let Err(e) = handle.join() {
            tracing::warn!("ClickHouse indexer thread panicked: {:?}", e);
        }
        tracing::info!("ClickHouse indexer finished");
    }

    Ok(stats)
}

/// Collect files to process based on input path.
fn collect_files(input: &PathBuf, limit: Option<usize>) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if input.is_file() {
        files.push(input.clone());
    } else if input.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(input)?
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
        anyhow::bail!("Input path does not exist: {}", input.display());
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
        tracing::info!("Opening dedupe index at {}", rocksdb_path.display());
        Some(Arc::new(DedupeIndex::open(rocksdb_path)?))
    } else {
        tracing::warn!("Running without deduplication (no --rocksdb-path specified)");
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
    let indexer_handle =
        if let (Some(ch_url), Some(receiver)) = (&args.clickhouse_url, sealed_receiver) {
            tracing::info!("Starting ClickHouse indexer for {}", ch_url);
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
        tracing::info!(
            "Sealed final segment {}: {} events",
            sealed.segment_number,
            sealed.event_count
        );

        // Mark events as archived in dedupe index
        if let Some(dedupe) = dedupe {
            dedupe.mark_archived(sealed.event_ids.iter())?;
        }
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
        tracing::info!("Dedupe index: ~{} keys", dedupe_stats.approximate_keys);
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
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.total_events, 0);
        assert_eq!(stats.valid_events, 0);
        assert_eq!(stats.invalid_events, 0);
        assert_eq!(stats.duplicate_events, 0);
    }

    #[test]
    fn test_progress_default() {
        let progress = Progress::default();
        assert!(progress.completed_files.is_empty());
    }

    #[test]
    fn test_progress_mark_completed() {
        let mut progress = Progress::default();
        assert!(!progress.is_completed("/path/to/file1.jsonl"));

        progress.mark_completed("/path/to/file1.jsonl");
        assert!(progress.is_completed("/path/to/file1.jsonl"));
        assert!(!progress.is_completed("/path/to/file2.jsonl"));
    }

    #[test]
    fn test_progress_save_and_load() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let progress_path = tmp.path().join("progress.json");

        // Create and save progress
        let mut progress = Progress::default();
        progress.mark_completed("/path/to/file1.jsonl");
        progress.mark_completed("/path/to/file2.jsonl");
        progress.save(&progress_path).unwrap();

        // Load it back
        let loaded = Progress::load(&progress_path).unwrap();
        assert!(loaded.is_completed("/path/to/file1.jsonl"));
        assert!(loaded.is_completed("/path/to/file2.jsonl"));
        assert!(!loaded.is_completed("/path/to/file3.jsonl"));
    }

    #[test]
    fn test_progress_load_nonexistent() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let progress_path = tmp.path().join("nonexistent.json");

        let progress = Progress::load(&progress_path).unwrap();
        assert!(progress.completed_files.is_empty());
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
            progress_file: None,
            metrics_port: 9091,
        };

        let stats = Stats {
            files_processed: 10,
            files_skipped: 2,
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
            progress_file: None,
            metrics_port: 0,
        };

        let stats = Stats::default();
        let elapsed = std::time::Duration::from_secs(0);

        // Should handle zeros gracefully
        print_summary(&args, &stats, elapsed);
    }
}
