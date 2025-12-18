//! Pensieve live ingestion daemon.
//!
//! This is the main entry point for the live Nostr event ingestion service.
//! It connects to Nostr relays, receives events in real-time, and writes them
//! to the archive pipeline.
//!
//! # Usage
//!
//! ```bash
//! # Run with default settings (seed relays, discovery enabled)
//! pensieve-ingest
//!
//! # Run with custom config file
//! pensieve-ingest --config /etc/pensieve/config.toml
//!
//! # Run with custom paths
//! pensieve-ingest \
//!     --rocksdb-path /data/dedupe \
//!     --output-dir /archive/segments \
//!     --clickhouse-url http://clickhouse:8123
//! ```
//!
//! # Graceful Shutdown
//!
//! The daemon handles SIGINT (Ctrl+C) and SIGTERM for graceful shutdown:
//! 1. Stops receiving new events from relays
//! 2. Seals the current segment
//! 3. Marks pending events as archived
//! 4. Exits cleanly

use anyhow::{Context, Result};
use clap::Parser;
use metrics::gauge;
use pensieve_core::metrics::{init_metrics, start_metrics_server};
use pensieve_ingest::{
    source::{RelayConfig, RelaySource},
    ClickHouseConfig, ClickHouseIndexer, DedupeIndex, PackedEvent, SealedSegment, SegmentConfig,
    SegmentWriter,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// Pensieve live ingestion daemon.
#[derive(Parser, Debug)]
#[command(name = "pensieve-ingest")]
#[command(about = "Live Nostr event ingestion daemon")]
#[command(version)]
struct Args {
    /// RocksDB path for dedupe index
    #[arg(long, default_value = "./data/dedupe")]
    rocksdb_path: PathBuf,

    /// Output directory for notepack segments
    #[arg(long, short, default_value = "./segments")]
    output_dir: PathBuf,

    /// ClickHouse URL (e.g., http://localhost:8123)
    #[arg(long)]
    clickhouse_url: Option<String>,

    /// ClickHouse database name
    #[arg(long, default_value = "nostr")]
    clickhouse_db: String,

    /// Maximum segment size in bytes before sealing
    #[arg(long, default_value = "268435456")] // 256 MB
    segment_size: usize,

    /// Disable gzip compression of output segments
    #[arg(long)]
    no_compress: bool,

    /// Initial relay URLs (comma-separated, overrides defaults)
    #[arg(long, value_delimiter = ',')]
    seed_relays: Option<Vec<String>>,

    /// Disable relay discovery via NIP-65
    #[arg(long)]
    no_discovery: bool,

    /// Maximum number of relay connections
    #[arg(long, default_value = "100")]
    max_relays: usize,

    /// Metrics HTTP server port (0 to disable)
    #[arg(long, default_value = "9090")]
    metrics_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider (required when both ring and aws-lc-rs are present)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap())
                .add_directive("pensieve_ingest=debug".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

    tracing::info!("Pensieve live ingestion daemon starting...");

    // Initialize metrics
    if args.metrics_port > 0 {
        let metrics_handle = init_metrics();
        start_metrics_server(args.metrics_port, metrics_handle).await?;
        gauge!("ingestion_running").set(1.0);
        tracing::info!("Metrics server listening on port {}", args.metrics_port);
    }

    // Set up graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);

    ctrlc::set_handler(move || {
        tracing::info!("Shutdown signal received, stopping gracefully...");
        running_clone.store(false, Ordering::SeqCst);
    })
    .context("Failed to set Ctrl+C handler")?;

    // Initialize pipeline components
    let (segment_writer, dedupe, indexer_handle) = init_pipeline(&args)?;

    // Build relay config
    let relay_config = RelayConfig {
        seed_relays: args.seed_relays.unwrap_or_else(|| {
            vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.nostr.band".to_string(),
                "wss://nos.lol".to_string(),
                "wss://relay.snort.social".to_string(),
                "wss://purplepag.es".to_string(),
                "wss://relay.primal.net".to_string(),
                "wss://nostr.wine".to_string(),
                "wss://relay.nostr.bg".to_string(),
            ]
        }),
        discovery_enabled: !args.no_discovery,
        max_relays: args.max_relays,
        ..Default::default()
    };

    tracing::info!("Configuration:");
    tracing::info!("  RocksDB: {}", args.rocksdb_path.display());
    tracing::info!("  Output: {}", args.output_dir.display());
    tracing::info!(
        "  ClickHouse: {}",
        args.clickhouse_url.as_deref().unwrap_or("disabled")
    );
    tracing::info!("  Seed relays: {}", relay_config.seed_relays.len());
    tracing::info!("  Discovery: {}", relay_config.discovery_enabled);
    tracing::info!("  Max relays: {}", relay_config.max_relays);

    // Create relay source
    let relay_source = RelaySource::new(relay_config);

    // Clone running flag for the handler
    let handler_running = Arc::clone(&running);

    // Run stats
    let mut events_processed = 0usize;
    let mut events_deduplicated = 0usize;

    // Run the ingestion loop
    tracing::info!("Starting live ingestion...");

    let stats = relay_source
        .run_async(|event: PackedEvent| {
            // Check if we should stop
            if !handler_running.load(Ordering::SeqCst) {
                return Ok(false);
            }

            // Dedupe check
            match dedupe.check_and_mark_pending(&event.event_id) {
                Ok(true) => {
                    // New event - write to segment
                    if let Err(e) = segment_writer.write(event) {
                        tracing::error!("Failed to write event: {}", e);
                        // Continue processing despite write errors
                    } else {
                        events_processed += 1;
                    }
                }
                Ok(false) => {
                    // Duplicate - skip
                    events_deduplicated += 1;
                }
                Err(e) => {
                    tracing::warn!("Dedupe check error: {}", e);
                    // Continue processing
                }
            }

            // Update metrics periodically
            if events_processed.is_multiple_of(1000) {
                gauge!("events_processed_total").set(events_processed as f64);
                gauge!("events_deduplicated_total").set(events_deduplicated as f64);
            }

            Ok(true) // Continue
        })
        .await?;

    // Shutdown sequence
    tracing::info!("Shutting down...");

    // Seal final segment
    if let Some(sealed) = segment_writer.seal()? {
        tracing::info!(
            "Sealed final segment {}: {} events",
            sealed.segment_number, sealed.event_count
        );

        // Mark as archived
        dedupe.mark_archived(sealed.event_ids.iter())?;
    }

    // Flush dedupe
    dedupe.flush()?;

    // Wait for ClickHouse indexer if running
    if let Some(handle) = indexer_handle {
        tracing::info!("Waiting for ClickHouse indexer to finish...");
        drop(segment_writer);
        if let Err(e) = handle.join() {
            tracing::warn!("ClickHouse indexer thread panicked: {:?}", e);
        }
    }

    // Mark as stopped
    gauge!("ingestion_running").set(0.0);

    // Print summary
    tracing::info!("═══════════════════════════════════════════════════════");
    tracing::info!("SHUTDOWN COMPLETE");
    tracing::info!("═══════════════════════════════════════════════════════");
    tracing::info!("Events received:      {}", stats.total_events);
    tracing::info!("Events processed:     {}", events_processed);
    tracing::info!("Events deduplicated:  {}", events_deduplicated);
    tracing::info!(
        "Relays connected:     {}",
        stats.source_metadata.relays_connected.unwrap_or(0)
    );
    tracing::info!(
        "Relays discovered:    {}",
        stats.source_metadata.relays_discovered.unwrap_or(0)
    );

    Ok(())
}

/// Pipeline components: (segment writer, dedupe index, optional ClickHouse indexer handle).
type PipelineComponents = (
    Arc<SegmentWriter>,
    Arc<DedupeIndex>,
    Option<std::thread::JoinHandle<()>>,
);

/// Initialize pipeline components.
fn init_pipeline(args: &Args) -> Result<PipelineComponents> {
    // Initialize dedupe index
    tracing::info!("Opening dedupe index at {}", args.rocksdb_path.display());
    let dedupe = Arc::new(
        DedupeIndex::open(&args.rocksdb_path)
            .with_context(|| format!("Failed to open dedupe index at {:?}", args.rocksdb_path))?,
    );

    let dedupe_stats = dedupe.stats();
    tracing::info!(
        "Dedupe index opened: ~{} keys",
        dedupe_stats.approximate_keys
    );

    // Set up ClickHouse channel (optional)
    let (sealed_sender, sealed_receiver) = if args.clickhouse_url.is_some() {
        let (tx, rx) = crossbeam_channel::unbounded::<SealedSegment>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Initialize segment writer
    let segment_config = SegmentConfig {
        output_dir: args.output_dir.clone(),
        max_segment_size: args.segment_size,
        segment_prefix: "segment".to_string(),
        compress: !args.no_compress,
    };

    tracing::info!(
        "Segment writer: output={}, max_size={}, compress={}",
        args.output_dir.display(),
        args.segment_size,
        !args.no_compress
    );

    let segment_writer = Arc::new(
        SegmentWriter::new(segment_config, sealed_sender)
            .with_context(|| "Failed to create segment writer")?,
    );

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
            let indexer = ClickHouseIndexer::new(ch_config)
                .with_context(|| "Failed to create ClickHouse indexer")?;
            Some(indexer.start(receiver))
        } else {
            None
        };

    Ok((segment_writer, dedupe, indexer_handle))
}
