//! Backfill adapter for Protobuf Nostr event files.
//!
//! This tool reads length-delimited protobuf files (optionally gzipped),
//! validates each event's ID and signature, deduplicates, and writes
//! valid events to notepack segments with optional ClickHouse indexing.
//!
//! Supports both local files and S3 sources. When using S3, files are
//! downloaded one at a time and deleted after processing to minimize disk usage.
//!
//! # Pipeline
//!
//! ```text
//! [S3/Local Files] → [Validation] → [Dedupe] → [SegmentWriter] → [ClickHouse]
//!                                      ↓              ↓
//!                                  RocksDB        Segments/
//! ```
//!
//! # Usage
//!
//! ```bash
//! # Local files
//! backfill-proto -i ./proto-segments/ -o ./segments/
//!
//! # S3 source (download → process → delete)
//! backfill-proto --s3-bucket my-bucket --s3-prefix nostr/segments/ \
//!     -o ./segments/ \
//!     --rocksdb-path ./data/dedupe \
//!     --clickhouse-url http://localhost:8123
//!
//! # S3 with limit for testing
//! backfill-proto --s3-bucket my-bucket --s3-prefix nostr/segments/ \
//!     -o ./segments/ --limit 5
//! ```

use anyhow::{bail, Context, Result};
use aws_sdk_s3::Client as S3Client;
use clap::Parser;
use flate2::read::GzDecoder;
use metrics::{counter, gauge};
use pensieve_core::{
    decode_length_delimited_with_size,
    metrics::{init_metrics, start_metrics_server},
    pack_event_binary_into, validate_proto_event,
};
use pensieve_ingest::{
    ClickHouseConfig, ClickHouseIndexer, DedupeIndex, PackedEvent, SealedSegment, SegmentConfig,
    SegmentWriter,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tracing_subscriber::EnvFilter;

/// Backfill Nostr events from Protobuf files to notepack archive format.
#[derive(Parser, Debug)]
#[command(name = "backfill-proto")]
#[command(about = "Convert Protobuf Nostr events to notepack archive format with full pipeline")]
struct Args {
    /// Input protobuf file or directory path (for local files)
    #[arg(short, long, conflicts_with_all = ["s3_bucket", "s3_prefix"])]
    input: Option<PathBuf>,

    /// S3 bucket name (for S3 source)
    #[arg(long)]
    s3_bucket: Option<String>,

    /// S3 key prefix (for S3 source, optional - defaults to bucket root)
    #[arg(long)]
    s3_prefix: Option<String>,

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

    /// Force gzip decompression (auto-detected from .gz extension)
    #[arg(long, default_value = "false")]
    gzip: bool,

    /// Skip validation (only parse protobuf, not ID/signature)
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

    /// Progress file path (tracks completed S3 keys for resume).
    /// Defaults to <output>/backfill-progress.json if not specified.
    #[arg(long)]
    progress_file: Option<PathBuf>,

    /// Temp directory for S3 downloads (defaults to system temp)
    #[arg(long)]
    temp_dir: Option<PathBuf>,

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
    files_skipped: usize,
    total_events: usize,
    valid_events: usize,
    invalid_events: usize,
    duplicate_events: usize,
    proto_errors: usize,
    validation_errors: usize,
    segments_sealed: usize,
    /// Compressed proto bytes (file size on disk/S3)
    compressed_proto_bytes: usize,
    /// Decompressed proto bytes (actual protobuf message sizes)
    decompressed_proto_bytes: usize,
    /// Uncompressed notepack bytes
    notepack_bytes: usize,
    /// Compressed notepack bytes (if compression enabled)
    compressed_notepack_bytes: usize,
}

/// Progress tracking for resumable backfill.
#[derive(Debug, Serialize, Deserialize, Default)]
struct Progress {
    /// S3 keys that have been fully processed.
    completed_keys: HashSet<String>,
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

    fn is_completed(&self, key: &str) -> bool {
        self.completed_keys.contains(key)
    }

    fn mark_completed(&mut self, key: &str) {
        self.completed_keys.insert(key.to_string());
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let args = Args::parse();

    // Validate args
    if args.input.is_none() && args.s3_bucket.is_none() {
        bail!("Must specify either --input (local) or --s3-bucket/--s3-prefix (S3)");
    }

    // Initialize metrics and start server (if enabled)
    if args.metrics_port > 0 {
        let metrics_handle = init_metrics();
        start_metrics_server(args.metrics_port, metrics_handle).await?;
        gauge!("backfill_running").set(1.0);
    }

    let start = Instant::now();
    let stats = if args.s3_bucket.is_some() {
        process_s3(&args).await?
    } else {
        process_local(&args)?
    };
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

    if let Some(ref input) = args.input {
        println!("Input:       {}", input.display());
    }
    if let Some(ref bucket) = args.s3_bucket {
        println!("S3 Bucket:   {}", bucket);
        if let Some(ref prefix) = args.s3_prefix {
            println!("S3 Prefix:   {}", prefix);
        }
    }
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
        println!("Files skipped:     {:>12} (already done)", stats.files_skipped);
    }
    println!("Total events:      {:>12}", stats.total_events);
    println!("Valid events:      {:>12}", stats.valid_events);
    println!("Duplicate events:  {:>12}", stats.duplicate_events);
    println!("Invalid events:    {:>12}", stats.invalid_events);
    if stats.invalid_events > 0 {
        println!("  - Proto errors:    {:>10}", stats.proto_errors);
        println!("  - Validation:      {:>10}", stats.validation_errors);
    }
    println!();
    println!("Segments sealed:   {:>12}", stats.segments_sealed);
    println!();
    println!("╭─────────────────────────────────────────────────────────────────╮");
    println!("│ SIZE COMPARISON                                                 │");
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!(
        "│ Proto (gzipped):     {:>15} bytes                      │",
        stats.compressed_proto_bytes
    );
    println!(
        "│ Proto (raw):         {:>15} bytes                      │",
        stats.decompressed_proto_bytes
    );
    println!(
        "│ Notepack (raw):      {:>15} bytes                      │",
        stats.notepack_bytes
    );
    println!(
        "│ Notepack (gzipped):  {:>15} bytes                      │",
        stats.compressed_notepack_bytes
    );
    println!("├─────────────────────────────────────────────────────────────────┤");

    // Raw vs Raw comparison (apples to apples)
    if stats.decompressed_proto_bytes > 0 {
        let notepack_vs_proto =
            stats.notepack_bytes as f64 / stats.decompressed_proto_bytes as f64;
        let reduction = (1.0 - notepack_vs_proto) * 100.0;
        println!(
            "│ Raw: Notepack vs Proto:    {:>6.1}% ({:<8})               │",
            reduction.abs(),
            if reduction > 0.0 { "smaller" } else { "larger" }
        );
    }

    // Compressed vs Compressed comparison (final storage)
    if stats.compressed_proto_bytes > 0 && stats.compressed_notepack_bytes > 0 {
        let gz_vs_gz =
            stats.compressed_notepack_bytes as f64 / stats.compressed_proto_bytes as f64;
        let reduction = (1.0 - gz_vs_gz) * 100.0;
        println!(
            "│ Gzip: Notepack vs Proto:   {:>6.1}% ({:<8})               │",
            reduction.abs(),
            if reduction > 0.0 { "smaller" } else { "larger" }
        );
    }

    // Compression ratios
    if stats.decompressed_proto_bytes > 0 && stats.compressed_proto_bytes > 0 {
        let proto_ratio =
            stats.decompressed_proto_bytes as f64 / stats.compressed_proto_bytes as f64;
        println!("│ Proto gzip ratio:          {:>6.1}x                            │", proto_ratio);
    }
    if stats.notepack_bytes > 0 && stats.compressed_notepack_bytes > 0 {
        let notepack_ratio =
            stats.notepack_bytes as f64 / stats.compressed_notepack_bytes as f64;
        println!("│ Notepack gzip ratio:       {:>6.1}x                            │", notepack_ratio);
    }
    println!("╰─────────────────────────────────────────────────────────────────╯");
    println!();
    println!("Elapsed time:      {:>12.2?}", elapsed);
    if stats.valid_events > 0 {
        let events_per_sec = stats.valid_events as f64 / elapsed.as_secs_f64();
        println!("Throughput:        {:>12.0} events/sec", events_per_sec);
    }
}

// ============================================================================
// S3 Processing
// ============================================================================

async fn process_s3(args: &Args) -> Result<Stats> {
    let bucket = args.s3_bucket.as_ref().unwrap();
    let prefix = args.s3_prefix.as_deref().unwrap_or("");
    let process_start = Instant::now();

    tracing::info!(
        "Initializing S3 client for bucket: {} (prefix: {:?})",
        bucket,
        if prefix.is_empty() { "<root>" } else { prefix }
    );

    // Initialize AWS SDK
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let s3_client = S3Client::new(&config);

    // Determine progress file path (default: <output>/backfill-progress.json)
    let progress_file = args
        .progress_file
        .clone()
        .unwrap_or_else(|| args.output.join("backfill-progress.json"));

    // Load progress (for resume)
    let mut progress = Progress::load(&progress_file)?;
    tracing::info!(
        "Loaded progress from {}: {} keys already completed",
        progress_file.display(),
        progress.completed_keys.len()
    );

    // List all objects in the bucket with the prefix
    let keys = list_s3_objects(&s3_client, bucket, prefix, args.limit).await?;
    tracing::info!("Found {} S3 objects to process", keys.len());

    // Initialize pipeline components
    let (segment_writer, dedupe, indexer_handle) = init_pipeline(args)?;

    let mut stats = Stats::default();

    // Get temp directory
    let temp_dir = args
        .temp_dir
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&temp_dir)?;

    for (idx, key) in keys.iter().enumerate() {
        // Skip if already completed
        if progress.is_completed(key) {
            tracing::info!("[{}/{}] Skipping (already done): {}", idx + 1, keys.len(), key);
            stats.files_skipped += 1;
            continue;
        }

        tracing::info!("[{}/{}] Processing: {}", idx + 1, keys.len(), key);

        // Download to temp file
        let temp_path = temp_dir.join(format!("backfill-{}.tmp", idx));
        match download_s3_object(&s3_client, bucket, key, &temp_path).await {
            Ok(size) => {
                stats.compressed_proto_bytes += size;
            }
            Err(e) => {
                tracing::error!("Failed to download {}: {}", key, e);
                if args.continue_on_error {
                    continue;
                } else {
                    return Err(e);
                }
            }
        }

        // Process the file
        let is_gzip = args.gzip || key.ends_with(".gz");
        match process_file_impl(&temp_path, is_gzip, &segment_writer, dedupe.as_ref(), args, &mut stats)
        {
            Ok(()) => {
                stats.files_processed += 1;

                // Mark as completed and save progress
                progress.mark_completed(key);
                if let Err(e) = progress.save(&progress_file) {
                    tracing::warn!("Failed to save progress: {}", e);
                }

                // Update metrics after each file
                record_metrics(&stats, process_start.elapsed().as_secs_f64());
            }
            Err(e) => {
                tracing::error!("Error processing {}: {}", key, e);
                if !args.continue_on_error {
                    // Clean up temp file before returning
                    let _ = fs::remove_file(&temp_path);
                    return Err(e);
                }
            }
        }

        // Delete temp file
        if let Err(e) = fs::remove_file(&temp_path) {
            tracing::warn!("Failed to delete temp file {}: {}", temp_path.display(), e);
        }
    }

    // Seal any remaining segment
    finalize_pipeline(&segment_writer, dedupe.as_ref(), &mut stats)?;

    // Final metrics update
    record_metrics(&stats, process_start.elapsed().as_secs_f64());

    // Wait for ClickHouse indexer to finish processing the queue
    if let Some(handle) = indexer_handle {
        tracing::info!("Waiting for ClickHouse indexer to finish...");
        // Drop the segment writer to close the sealed_sender channel,
        // signaling the indexer thread to exit after draining the queue
        drop(segment_writer);
        if let Err(e) = handle.join() {
            tracing::warn!("ClickHouse indexer thread panicked: {:?}", e);
        }
        tracing::info!("ClickHouse indexer finished");
    }

    Ok(stats)
}

async fn list_s3_objects(
    client: &S3Client,
    bucket: &str,
    prefix: &str,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut continuation_token: Option<String> = None;

    loop {
        let mut request = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix);

        if let Some(token) = continuation_token {
            request = request.continuation_token(token);
        }

        let response = request.send().await?;

        if let Some(contents) = response.contents {
            for obj in contents {
                if let Some(key) = obj.key {
                    // Filter to only .pb or .gz files
                    if key.ends_with(".pb") || key.ends_with(".gz") || key.ends_with(".pb.gz") {
                        keys.push(key);
                    }
                }
            }
        }

        // Check if we've hit the limit
        if let Some(limit) = limit
            && keys.len() >= limit
        {
            keys.truncate(limit);
            break;
        }

        // Check for more pages
        if response.is_truncated.unwrap_or(false) {
            continuation_token = response.next_continuation_token;
        } else {
            break;
        }
    }

    // Sort for deterministic order
    keys.sort();

    Ok(keys)
}

async fn download_s3_object(
    client: &S3Client,
    bucket: &str,
    key: &str,
    dest: &PathBuf,
) -> Result<usize> {
    let response = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("Failed to get S3 object: {}", key))?;

    let content_length = response.content_length.unwrap_or(0) as usize;

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("Failed to create temp file: {}", dest.display()))?;

    let mut stream = response.body;
    let mut total_written = 0usize;

    while let Some(bytes) = stream.try_next().await? {
        file.write_all(&bytes).await?;
        total_written += bytes.len();
    }

    file.flush().await?;

    tracing::info!(
        "Downloaded {} bytes from s3://{}/{}",
        total_written, bucket, key
    );

    Ok(content_length)
}

// ============================================================================
// Local Processing
// ============================================================================

fn process_local(args: &Args) -> Result<Stats> {
    let input = args.input.as_ref().unwrap();
    let mut stats = Stats::default();
    let process_start = Instant::now();

    // Initialize pipeline components
    let (segment_writer, dedupe, indexer_handle) = init_pipeline(args)?;

    // Collect input files
    let files = collect_local_files(input, args.limit)?;
    tracing::info!("Found {} files to process", files.len());

    for (file_idx, file_path) in files.iter().enumerate() {
        tracing::info!(
            "[{}/{}] Processing: {}",
            file_idx + 1,
            files.len(),
            file_path.display()
        );

        let file_size = fs::metadata(file_path)?.len() as usize;
        stats.compressed_proto_bytes += file_size;

        let is_gzip = args.gzip || file_path.extension().is_some_and(|ext| ext == "gz");

        match process_file_impl(file_path, is_gzip, &segment_writer, dedupe.as_ref(), args, &mut stats) {
            Ok(()) => {
                stats.files_processed += 1;
                // Update metrics after each file
                record_metrics(&stats, process_start.elapsed().as_secs_f64());
            }
            Err(e) => {
                tracing::warn!("Error processing {}: {}", file_path.display(), e);
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
        tracing::info!("Waiting for ClickHouse indexer to finish...");
        // Drop the segment writer to close the sealed_sender channel,
        // signaling the indexer thread to exit after draining the queue
        drop(segment_writer);
        if let Err(e) = handle.join() {
            tracing::warn!("ClickHouse indexer thread panicked: {:?}", e);
        }
        tracing::info!("ClickHouse indexer finished");
    }

    Ok(stats)
}

fn collect_local_files(input: &PathBuf, limit: Option<usize>) -> Result<Vec<PathBuf>> {
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
                    && (path.extension().is_some_and(|ext| ext == "pb" || ext == "gz"))
            })
            .map(|e| e.path())
            .collect();

        entries.sort();
        files.extend(entries);
    } else {
        bail!("Input path does not exist: {}", input.display());
    }

    if let Some(limit) = limit {
        files.truncate(limit);
    }

    Ok(files)
}

// ============================================================================
// Common Pipeline
// ============================================================================

/// Pipeline components: (SegmentWriter, DedupeIndex, ClickHouse indexer handle)
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
    tracing::info!(
        "Segment compression: {}",
        if segment_config.compress {
            "enabled"
        } else {
            "disabled"
        }
    );
    let segment_writer = Arc::new(SegmentWriter::new(segment_config, sealed_sender)?);

    // Initialize ClickHouse indexer (optional)
    let indexer_handle = if let (Some(ch_url), Some(receiver)) =
        (&args.clickhouse_url, sealed_receiver)
    {
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
        stats.segments_sealed += 1;

        // Mark events as archived in dedupe index
        if let Some(dedupe) = dedupe {
            dedupe.mark_archived(sealed.event_ids.iter())?;
        }
    }

    // Get final stats from segment writer
    let seg_stats = segment_writer.stats();
    stats.notepack_bytes = seg_stats.total_bytes;
    stats.compressed_notepack_bytes = seg_stats.total_compressed_bytes;
    stats.segments_sealed = seg_stats.segment_number as usize;

    // Flush dedupe index
    if let Some(dedupe) = dedupe {
        dedupe.flush()?;
        let dedupe_stats = dedupe.stats();
        tracing::info!("Dedupe index: ~{} keys", dedupe_stats.approximate_keys);
    }

    Ok(())
}

fn process_file_impl(
    file_path: &PathBuf,
    is_gzip: bool,
    segment_writer: &Arc<SegmentWriter>,
    dedupe: Option<&Arc<DedupeIndex>>,
    args: &Args,
    stats: &mut Stats,
) -> Result<()> {
    let file = File::open(file_path)
        .with_context(|| format!("Failed to open file: {}", file_path.display()))?;

    // Create reader (with optional gzip decompression)
    let mut reader: Box<dyn Read> = if is_gzip {
        Box::new(BufReader::new(GzDecoder::new(BufReader::new(file))))
    } else {
        Box::new(BufReader::new(file))
    };

    // Reusable buffer for notepack encoding
    let mut pack_buf: Vec<u8> = Vec::with_capacity(4096);

    loop {
        stats.total_events += 1;

        // Read the next length-delimited protobuf event
        let (proto_event, proto_bytes) = match decode_length_delimited_with_size(&mut reader) {
            Ok(Some((event, bytes))) => (event, bytes),
            Ok(None) => {
                stats.total_events -= 1; // Don't count EOF as an event
                break;
            }
            Err(e) => {
                tracing::warn!("Protobuf decode error: {}", e);
                stats.invalid_events += 1;
                stats.proto_errors += 1;
                if args.continue_on_error {
                    break;
                } else {
                    bail!("Protobuf decode error: {}", e);
                }
            }
        };

        stats.decompressed_proto_bytes += proto_bytes;

        pack_buf.clear();

        // Validate and pack
        let (event_id, _packed_len) = if args.skip_validation {
            match proto_to_notepack_unvalidated(&proto_event, &mut pack_buf) {
                Ok(len) => {
                    let id_bytes = hex_to_bytes32(&proto_event.id)?;
                    (id_bytes, len)
                }
                Err(e) => {
                    tracing::warn!("Notepack encoding error: {}", e);
                    stats.invalid_events += 1;
                    stats.proto_errors += 1;
                    if args.continue_on_error {
                        continue;
                    } else {
                        bail!("Notepack encoding error: {}", e);
                    }
                }
            }
        } else {
            match validate_proto_event(&proto_event) {
                Ok(event) => {
                    let id_bytes: [u8; 32] = *event.id.as_bytes();
                    let len = pack_event_binary_into(&event, &mut pack_buf);
                    (id_bytes, len)
                }
                Err(e) => {
                    tracing::warn!("Validation error: {}", e);
                    stats.invalid_events += 1;
                    stats.validation_errors += 1;
                    if args.continue_on_error {
                        continue;
                    } else {
                        bail!("Validation error: {}", e);
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

        // Progress reporting and metrics
        if stats.total_events.is_multiple_of(args.progress_interval) {
            tracing::info!(
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

fn proto_to_notepack_unvalidated(
    proto: &pensieve_core::ProtoEvent,
    buf: &mut Vec<u8>,
) -> Result<usize> {
    use notepack::{pack_note_into, NoteBuf};

    let tags: Vec<Vec<String>> = proto.tags.iter().map(|t| t.values.clone()).collect();

    let note = NoteBuf {
        id: proto.id.clone(),
        pubkey: proto.pubkey.clone(),
        created_at: proto.created_at as u64,
        kind: proto.kind as u64,
        tags,
        content: proto.content.clone(),
        sig: proto.sig.clone(),
    };

    pack_note_into(&note, buf).map_err(|e| anyhow::anyhow!("notepack error: {}", e))
}

// ============================================================================
// Metrics
// ============================================================================

/// Record metrics from current stats.
fn record_metrics(stats: &Stats, elapsed_secs: f64) {
    // Counters (these are cumulative, so we set absolute values)
    counter!("backfill_events_total").absolute(stats.total_events as u64);
    counter!("backfill_events_valid_total").absolute(stats.valid_events as u64);
    counter!("backfill_events_duplicate_total").absolute(stats.duplicate_events as u64);
    counter!("backfill_events_invalid_total").absolute(stats.invalid_events as u64);
    counter!("backfill_files_total").absolute(stats.files_processed as u64);
    counter!("backfill_segments_sealed_total").absolute(stats.segments_sealed as u64);

    // Bytes by type
    counter!("backfill_bytes_total", "type" => "proto_compressed")
        .absolute(stats.compressed_proto_bytes as u64);
    counter!("backfill_bytes_total", "type" => "proto_raw")
        .absolute(stats.decompressed_proto_bytes as u64);
    counter!("backfill_bytes_total", "type" => "notepack_raw")
        .absolute(stats.notepack_bytes as u64);
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
    use pensieve_core::Tag;
    use tempfile::TempDir;

    // =========================================================================
    // Progress tests
    // =========================================================================

    #[test]
    fn test_progress_default() {
        let progress = Progress::default();
        assert!(progress.completed_keys.is_empty());
    }

    #[test]
    fn test_progress_mark_completed() {
        let mut progress = Progress::default();
        assert!(!progress.is_completed("key1"));

        progress.mark_completed("key1");
        assert!(progress.is_completed("key1"));
        assert!(!progress.is_completed("key2"));
    }

    #[test]
    fn test_progress_mark_completed_idempotent() {
        let mut progress = Progress::default();
        progress.mark_completed("key1");
        progress.mark_completed("key1");
        progress.mark_completed("key1");

        // Should still only have one entry
        assert_eq!(progress.completed_keys.len(), 1);
        assert!(progress.is_completed("key1"));
    }

    #[test]
    fn test_progress_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let progress_path = tmp.path().join("progress.json");

        // Create and save progress
        let mut progress = Progress::default();
        progress.mark_completed("s3://bucket/key1.proto.gz");
        progress.mark_completed("s3://bucket/key2.proto.gz");
        progress.save(&progress_path).unwrap();

        // Load it back
        let loaded = Progress::load(&progress_path).unwrap();
        assert!(loaded.is_completed("s3://bucket/key1.proto.gz"));
        assert!(loaded.is_completed("s3://bucket/key2.proto.gz"));
        assert!(!loaded.is_completed("s3://bucket/key3.proto.gz"));
    }

    #[test]
    fn test_progress_load_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let progress_path = tmp.path().join("nonexistent.json");

        let progress = Progress::load(&progress_path).unwrap();
        assert!(progress.completed_keys.is_empty());
    }

    #[test]
    fn test_progress_multiple_keys() {
        let mut progress = Progress::default();
        for i in 0..100 {
            progress.mark_completed(&format!("key{}", i));
        }

        assert_eq!(progress.completed_keys.len(), 100);
        assert!(progress.is_completed("key0"));
        assert!(progress.is_completed("key99"));
        assert!(!progress.is_completed("key100"));
    }

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

    // =========================================================================
    // proto_to_notepack_unvalidated tests
    // =========================================================================

    fn make_test_proto(id: &str, content: &str) -> pensieve_core::ProtoEvent {
        pensieve_core::ProtoEvent {
            id: id.to_string(),
            pubkey: "0000000000000000000000000000000000000000000000000000000000000001"
                .to_string(),
            created_at: 1700000000,
            kind: 1,
            tags: vec![],
            content: content.to_string(),
            sig: "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001".to_string(),
        }
    }

    #[test]
    fn test_proto_to_notepack_unvalidated_basic() {
        let proto = make_test_proto(
            "0000000000000000000000000000000000000000000000000000000000000001",
            "Hello, world!",
        );

        let mut buf = Vec::new();
        let size = proto_to_notepack_unvalidated(&proto, &mut buf).unwrap();

        assert!(size > 0);
        assert_eq!(buf.len(), size);
    }

    #[test]
    fn test_proto_to_notepack_unvalidated_with_tags() {
        let proto = pensieve_core::ProtoEvent {
            id: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            pubkey: "0000000000000000000000000000000000000000000000000000000000000002"
                .to_string(),
            created_at: 1700000000,
            kind: 1,
            tags: vec![
                Tag {
                    values: vec!["p".to_string(), "target_pubkey".to_string()],
                },
                Tag {
                    values: vec![
                        "e".to_string(),
                        "event_id".to_string(),
                        "relay_url".to_string(),
                    ],
                },
            ],
            content: "Tagged content".to_string(),
            sig: "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001".to_string(),
        };

        let mut buf = Vec::new();
        let size = proto_to_notepack_unvalidated(&proto, &mut buf).unwrap();
        assert!(size > 0);
    }

    #[test]
    fn test_proto_to_notepack_unvalidated_unicode() {
        let proto = make_test_proto(
            "0000000000000000000000000000000000000000000000000000000000000001",
            "Hello 🌍 世界 emoji: 🎉",
        );

        let mut buf = Vec::new();
        let size = proto_to_notepack_unvalidated(&proto, &mut buf).unwrap();
        assert!(size > 0);
    }

    #[test]
    fn test_proto_to_notepack_unvalidated_empty_content() {
        let proto = make_test_proto(
            "0000000000000000000000000000000000000000000000000000000000000001",
            "",
        );

        let mut buf = Vec::new();
        let size = proto_to_notepack_unvalidated(&proto, &mut buf).unwrap();
        assert!(size > 0);
    }

    #[test]
    fn test_proto_to_notepack_unvalidated_appends_to_buffer() {
        let proto = make_test_proto(
            "0000000000000000000000000000000000000000000000000000000000000001",
            "test",
        );

        let mut buf = vec![0xAA, 0xBB]; // Pre-existing data
        let size = proto_to_notepack_unvalidated(&proto, &mut buf).unwrap();

        // Buffer should contain original bytes plus new data
        assert_eq!(buf[0], 0xAA);
        assert_eq!(buf[1], 0xBB);
        assert_eq!(buf.len(), 2 + size);
    }

    // =========================================================================
    // Stats tests
    // =========================================================================

    #[test]
    fn test_stats_default() {
        let stats = Stats::default();
        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.total_events, 0);
        assert_eq!(stats.valid_events, 0);
        assert_eq!(stats.invalid_events, 0);
        assert_eq!(stats.duplicate_events, 0);
        assert_eq!(stats.proto_errors, 0);
        assert_eq!(stats.validation_errors, 0);
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

    // =========================================================================
    // print_summary tests (smoke test - just ensure it doesn't panic)
    // =========================================================================

    #[test]
    fn test_print_summary_local_input() {
        let args = Args {
            input: Some(PathBuf::from("/test/input")),
            s3_bucket: None,
            s3_prefix: None,
            output: PathBuf::from("/test/output"),
            rocksdb_path: Some(PathBuf::from("/test/rocksdb")),
            clickhouse_url: Some("http://localhost:8123".to_string()),
            clickhouse_db: "nostr".to_string(),
            segment_size: 256 * 1024 * 1024,
            gzip: false,
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100000,
            progress_file: None,
            temp_dir: None,
            no_compress: false,
            metrics_port: 9091,
        };

        let stats = Stats {
            files_processed: 10,
            files_skipped: 2,
            total_events: 1000,
            valid_events: 900,
            invalid_events: 50,
            duplicate_events: 50,
            proto_errors: 20,
            validation_errors: 30,
            segments_sealed: 3,
            compressed_proto_bytes: 500_000,
            decompressed_proto_bytes: 1_000_000,
            notepack_bytes: 600_000,
            compressed_notepack_bytes: 250_000,
        };

        let elapsed = std::time::Duration::from_secs(10);

        // This should not panic
        print_summary(&args, &stats, elapsed);
    }

    #[test]
    fn test_print_summary_s3_input() {
        let args = Args {
            input: None,
            s3_bucket: Some("my-bucket".to_string()),
            s3_prefix: Some("nostr/segments/".to_string()),
            output: PathBuf::from("/test/output"),
            rocksdb_path: None,
            clickhouse_url: None,
            clickhouse_db: "nostr".to_string(),
            segment_size: 256 * 1024 * 1024,
            gzip: false,
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100000,
            progress_file: None,
            temp_dir: None,
            no_compress: false,
            metrics_port: 0,
        };

        let stats = Stats::default();
        let elapsed = std::time::Duration::from_secs(0);

        // This should not panic
        print_summary(&args, &stats, elapsed);
    }

    #[test]
    fn test_print_summary_zero_bytes() {
        let args = Args {
            input: Some(PathBuf::from("/test")),
            s3_bucket: None,
            s3_prefix: None,
            output: PathBuf::from("/out"),
            rocksdb_path: None,
            clickhouse_url: None,
            clickhouse_db: "nostr".to_string(),
            segment_size: 256 * 1024 * 1024,
            gzip: false,
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100000,
            progress_file: None,
            temp_dir: None,
            no_compress: false,
            metrics_port: 0,
        };

        // All zeros - should handle division by zero gracefully
        let stats = Stats::default();
        let elapsed = std::time::Duration::from_secs(0);

        print_summary(&args, &stats, elapsed);
    }
}
