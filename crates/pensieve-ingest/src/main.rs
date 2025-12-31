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
use metrics::{counter, gauge};
use pensieve_core::metrics::{init_metrics, start_metrics_server};
use pensieve_ingest::{
    ClickHouseConfig, ClickHouseIndexer, DedupeIndex, RelayManager, RelayManagerConfig,
    SealedSegment, SegmentConfig, SegmentWriter, pack_nostr_event,
    relay::normalize_relay_url,
    source::{RelayConfig, RelaySource},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

/// Default seed relays to use when no CLI args or seed file is available.
fn default_seed_relays() -> Vec<String> {
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
}

/// Load seed relays from a text file (one URL per line, # for comments).
///
/// URLs are normalized before being returned. Invalid or blocked URLs are skipped.
fn load_seeds_from_file(path: &std::path::Path) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read seed file: {}", path.display()))?;

    let mut seeds = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Normalize and validate the URL
        if let Some(normalized) = normalize_relay_url(line).ok() {
            seeds.push(normalized);
        } else {
            tracing::warn!("Skipping invalid/blocked relay URL in seed file: {}", line);
        }
    }
    Ok(seeds)
}

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

    /// Initial seed relay URLs (comma-separated, overrides seed file)
    #[arg(long, value_delimiter = ',')]
    seed_relays: Option<Vec<String>>,

    /// Path to seed relays file (one URL per line, # for comments)
    #[arg(long, default_value = "./data/relays/seed.txt")]
    seed_file: PathBuf,

    /// Import discovered relays from a JSON file (tier=discovered, can be swapped)
    #[arg(long)]
    import_discovered_json: Option<PathBuf>,

    /// Disable relay discovery via NIP-65
    #[arg(long)]
    no_discovery: bool,

    /// Maximum number of relay connections
    #[arg(long, default_value = "30")]
    max_relays: usize,

    /// Metrics HTTP server port (0 to disable)
    #[arg(long, default_value = "9091")]
    metrics_port: u16,

    /// SQLite database path for relay quality tracking
    #[arg(long, default_value = "./data/relay-stats.db")]
    relay_db_path: PathBuf,

    /// Score recomputation interval in seconds
    #[arg(long, default_value = "300")]
    score_interval_secs: u64,

    /// Enable catch-up processing (use saved checkpoint to request missed events).
    /// WARNING: Most relays don't serve historical data efficiently, so this may
    /// result in much lower throughput. Generally not recommended.
    #[arg(long)]
    catchup: bool,

    /// Buffer time in seconds to subtract from checkpoint when catching up.
    /// This handles clock skew and relay propagation delay.
    /// The dedupe index will filter out any duplicates from the overlap.
    #[arg(long, default_value = "300")]
    catchup_buffer_secs: u64,

    /// How often to update the checkpoint (seconds).
    /// Lower values mean less catch-up work after a crash but more I/O.
    #[arg(long, default_value = "60")]
    checkpoint_interval_secs: u64,

    /// SOCKS5 proxy address for connecting to Tor (.onion) relays.
    ///
    /// When set, .onion relay addresses will be routed through this proxy.
    /// Clearnet relays will continue to connect directly.
    ///
    /// Example: 127.0.0.1:9050 (default Tor SOCKS5 port)
    #[arg(long, value_name = "HOST:PORT")]
    tor_proxy: Option<std::net::SocketAddr>,
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

    // Initialize relay manager for quality tracking
    let relay_manager_config = RelayManagerConfig {
        db_path: args.relay_db_path.clone(),
        max_relays: args.max_relays,
        optimization_interval_secs: args.score_interval_secs,
        ..Default::default()
    };

    let relay_manager =
        Arc::new(RelayManager::open(relay_manager_config).with_context(|| {
            format!("Failed to open relay manager at {:?}", args.relay_db_path)
        })?);

    // Load seed relays (tier=seed, protected from eviction)
    // Priority: CLI args > seed file > hardcoded defaults
    let seed_relays = if let Some(relays) = args.seed_relays.clone() {
        tracing::info!("Using {} seed relays from CLI arguments", relays.len());
        relays
    } else if args.seed_file.exists() {
        match load_seeds_from_file(&args.seed_file) {
            Ok(relays) if !relays.is_empty() => {
                tracing::info!(
                    "Loaded {} seed relays from file: {}",
                    relays.len(),
                    args.seed_file.display()
                );
                relays
            }
            Ok(_) => {
                tracing::warn!(
                    "Seed file {} is empty, using defaults",
                    args.seed_file.display()
                );
                default_seed_relays()
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load seed file {}: {}. Using defaults.",
                    args.seed_file.display(),
                    e
                );
                default_seed_relays()
            }
        }
    } else {
        tracing::info!("Using default seed relays (no seed file found)");
        default_seed_relays()
    };

    // Register seeds with tier=seed (protected, score floor 0.5)
    relay_manager.register_seed_relays(&seed_relays)?;
    tracing::info!("Registered {} seed relays (tier=seed)", seed_relays.len());

    // Import discovered relays from JSON if provided (tier=discovered, can be swapped)
    if let Some(json_path) = &args.import_discovered_json {
        match relay_manager.import_from_json(json_path) {
            Ok(count) => {
                tracing::info!(
                    "Imported {} discovered relays from JSON: {} (tier=discovered)",
                    count,
                    json_path.display()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to import discovered relays from {}: {}",
                    json_path.display(),
                    e
                );
            }
        }
    }

    // Load checkpoint for catch-up processing
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let since_timestamp = if !args.catchup {
        tracing::info!("Catch-up disabled (default) - subscribing to all events");
        None
    } else {
        match relay_manager.get_last_archived_timestamp() {
            Ok(Some(checkpoint)) => {
                // Clamp checkpoint to current time (events can have future created_at due to clock skew)
                let clamped = checkpoint.min(now_secs);
                if clamped < checkpoint {
                    tracing::warn!(
                        "Checkpoint {} was in the future, clamped to current time {}",
                        checkpoint,
                        clamped
                    );
                }

                // Apply buffer to handle clock skew and relay propagation delay
                let buffered = clamped.saturating_sub(args.catchup_buffer_secs);
                tracing::info!(
                    "Loaded checkpoint: {} (buffered to {} with {}s buffer)",
                    clamped,
                    buffered,
                    args.catchup_buffer_secs
                );
                Some(buffered)
            }
            Ok(None) => {
                tracing::info!(
                    "No checkpoint found - fresh start, subscribing to live events only"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load checkpoint: {} - starting without catch-up",
                    e
                );
                None
            }
        }
    };

    // Build relay config for RelaySource
    let relay_config = RelayConfig {
        seed_relays: seed_relays.clone(),
        discovery_enabled: !args.no_discovery,
        max_relays: args.max_relays,
        optimization_interval: Duration::from_secs(args.score_interval_secs),
        since_timestamp,
        tor_proxy: args.tor_proxy,
        ..Default::default()
    };

    if args.tor_proxy.is_some() {
        tracing::info!("Tor proxy enabled - .onion relays will be routed through proxy");
    }

    tracing::info!("Configuration:");
    tracing::info!("  RocksDB: {}", args.rocksdb_path.display());
    tracing::info!("  Relay DB: {}", args.relay_db_path.display());
    tracing::info!("  Output: {}", args.output_dir.display());
    tracing::info!(
        "  ClickHouse: {}",
        args.clickhouse_url.as_deref().unwrap_or("disabled")
    );
    tracing::info!("  Seed relays: {}", relay_config.seed_relays.len());
    tracing::info!("  Discovery: {}", relay_config.discovery_enabled);
    tracing::info!("  Max relays: {}", relay_config.max_relays);
    tracing::info!(
        "  Optimization interval: {}s",
        relay_config.optimization_interval.as_secs()
    );
    tracing::info!(
        "  Catch-up: {}",
        match relay_config.since_timestamp {
            Some(ts) => format!("enabled (since {}) - WARNING: may reduce throughput", ts),
            None => "disabled (default)".to_string(),
        }
    );
    tracing::info!("  Checkpoint interval: {}s", args.checkpoint_interval_secs);

    // Create relay source with manager for quality tracking and optimization
    // The RelaySource now handles:
    // - Recording connection attempts/disconnections
    // - Periodic score recomputation
    // - Swapping low-scoring relays for higher-scoring ones
    let relay_source = RelaySource::with_manager(relay_config, Arc::clone(&relay_manager));

    // Clone running flag for the handler
    let handler_running = Arc::clone(&running);

    // Clone relay manager for the handler (to record event novelty)
    let handler_relay_manager = Arc::clone(&relay_manager);

    // Spawn background task for updating metrics
    // (Score recomputation is now handled by RelaySource's optimization loop)
    let metrics_running = Arc::clone(&running);
    let metrics_relay_manager = Arc::clone(&relay_manager);
    let metrics_interval = args.score_interval_secs;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(metrics_interval));
        interval.tick().await; // Skip first immediate tick

        while metrics_running.load(Ordering::SeqCst) {
            interval.tick().await;

            if !metrics_running.load(Ordering::SeqCst) {
                break;
            }

            // Update aggregate metrics
            if let Ok(agg_stats) = metrics_relay_manager.get_aggregate_stats() {
                gauge!("relay_manager_total_relays").set(agg_stats.total_relays as f64);
                gauge!("relay_manager_active_relays").set(agg_stats.active_relays as f64);
                gauge!("relay_manager_blocked_relays").set(agg_stats.blocked_relays as f64);
                gauge!("relay_manager_avg_score").set(agg_stats.avg_score);
                gauge!("relay_manager_events_novel_1h").set(agg_stats.events_novel_1h as f64);
                gauge!("relay_manager_events_duplicate_1h")
                    .set(agg_stats.events_duplicate_1h as f64);
            }
        }

        tracing::debug!("Metrics update task stopped");
    });

    // Run stats - use atomics so we can read from the metrics task
    let events_received = Arc::new(AtomicUsize::new(0));
    let events_processed = Arc::new(AtomicUsize::new(0));
    let events_deduplicated = Arc::new(AtomicUsize::new(0));

    // Track max created_at timestamp for checkpoint updates
    // Start with the current checkpoint (if any) so we don't regress
    let max_created_at = Arc::new(std::sync::atomic::AtomicU64::new(
        since_timestamp.unwrap_or(0),
    ));

    // Clone for the handler
    let handler_events_received = Arc::clone(&events_received);
    let handler_events_processed = Arc::clone(&events_processed);
    let handler_events_deduplicated = Arc::clone(&events_deduplicated);
    let handler_max_created_at = Arc::clone(&max_created_at);

    // Spawn background task for rate metrics calculation
    let rate_running = Arc::clone(&running);
    let rate_events_received = Arc::clone(&events_received);
    let rate_events_processed = Arc::clone(&events_processed);
    let rate_events_deduplicated = Arc::clone(&events_deduplicated);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.tick().await; // Skip first immediate tick

        let mut last_received = 0usize;
        let mut last_processed = 0usize;
        let mut last_deduplicated = 0usize;

        while rate_running.load(Ordering::SeqCst) {
            interval.tick().await;

            if !rate_running.load(Ordering::SeqCst) {
                break;
            }

            // Read current counts
            let curr_received = rate_events_received.load(Ordering::Relaxed);
            let curr_processed = rate_events_processed.load(Ordering::Relaxed);
            let curr_deduplicated = rate_events_deduplicated.load(Ordering::Relaxed);

            // Calculate rates (events per second, over 5-second window)
            let received_rate = (curr_received - last_received) as f64 / 5.0;
            let processed_rate = (curr_processed - last_processed) as f64 / 5.0;
            let deduplicated_rate = (curr_deduplicated - last_deduplicated) as f64 / 5.0;

            // Update counters (absolute values - Prometheus calculates rate())
            // Using absolute() to set the counter to the current value
            counter!("ingest_events_received_total").absolute(curr_received as u64);
            counter!("ingest_events_processed_total").absolute(curr_processed as u64);
            counter!("ingest_events_deduplicated_total").absolute(curr_deduplicated as u64);

            // Rate gauges (our own 5-second rolling average for quick dashboards)
            gauge!("ingest_events_received_rate").set(received_rate);
            gauge!("ingest_events_processed_rate").set(processed_rate);
            gauge!("ingest_events_deduplicated_rate").set(deduplicated_rate);

            // Dedupe ratio (what percentage of incoming events are duplicates)
            let total_handled = curr_processed + curr_deduplicated;
            let dedupe_ratio = if total_handled > 0 {
                curr_deduplicated as f64 / total_handled as f64
            } else {
                0.0
            };
            gauge!("ingest_dedupe_ratio").set(dedupe_ratio);

            // Save for next iteration
            last_received = curr_received;
            last_processed = curr_processed;
            last_deduplicated = curr_deduplicated;

            // Log throughput periodically (every 5 seconds)
            if curr_received > 0 {
                tracing::debug!(
                    "Throughput: {:.0} received/s, {:.0} processed/s, {:.0} deduped/s ({:.1}% duplicates)",
                    received_rate,
                    processed_rate,
                    deduplicated_rate,
                    dedupe_ratio * 100.0
                );
            }
        }

        tracing::debug!("Rate metrics task stopped");
    });

    // Spawn background task for checkpoint updates
    let checkpoint_running = Arc::clone(&running);
    let checkpoint_relay_manager = Arc::clone(&relay_manager);
    let checkpoint_max_created_at = Arc::clone(&max_created_at);
    let checkpoint_interval = args.checkpoint_interval_secs;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(checkpoint_interval));
        interval.tick().await; // Skip first immediate tick

        while checkpoint_running.load(Ordering::SeqCst) {
            interval.tick().await;

            if !checkpoint_running.load(Ordering::SeqCst) {
                break;
            }

            // Read the current max timestamp and persist it
            let max_ts = checkpoint_max_created_at.load(Ordering::Relaxed);
            if max_ts > 0 {
                if let Err(e) = checkpoint_relay_manager.update_last_archived_timestamp(max_ts) {
                    tracing::warn!("Failed to update checkpoint: {}", e);
                } else {
                    tracing::debug!("Updated checkpoint to {}", max_ts);
                    gauge!("ingest_checkpoint_timestamp").set(max_ts as f64);
                }
            }
        }

        tracing::debug!("Checkpoint update task stopped");
    });

    // Run the ingestion loop
    tracing::info!("Starting live ingestion...");

    let stats = relay_source
        .run_async(|relay_url: String, event: &nostr_sdk::Event| {
            // Check if we should stop
            if !handler_running.load(Ordering::SeqCst) {
                return Ok(false);
            }

            // Count all received events (before dedupe)
            handler_events_received.fetch_add(1, Ordering::Relaxed);

            // Get the event ID bytes for dedupe check (CHEAP - just a reference)
            let event_id = event.id.as_bytes();

            // Dedupe check FIRST - before expensive packing
            let is_novel = match dedupe.check_and_mark_pending(event_id) {
                Ok(novel) => novel,
                Err(e) => {
                    tracing::warn!("Dedupe check error: {}", e);
                    false // Treat as duplicate on error
                }
            };

            // Record event for relay quality tracking
            handler_relay_manager.record_event(&relay_url, is_novel);

            if is_novel {
                // New event - NOW pack it (only for novel events)
                match pack_nostr_event(event) {
                    Ok(packed) => {
                        // Write to segment
                        if let Err(e) = segment_writer.write(packed) {
                            tracing::error!("Failed to write event: {}", e);
                            // Continue processing despite write errors
                        } else {
                            handler_events_processed.fetch_add(1, Ordering::Relaxed);

                            // Track max created_at for checkpoint
                            // Use fetch_max to atomically update to the highest value
                            // Clamp to current time to avoid saving future timestamps
                            let event_ts = event.created_at.as_secs();
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let clamped_ts = event_ts.min(now);
                            handler_max_created_at.fetch_max(clamped_ts, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to pack event: {}", e);
                    }
                }
            } else {
                // Duplicate - skip (no packing needed!)
                handler_events_deduplicated.fetch_add(1, Ordering::Relaxed);
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
            sealed.segment_number,
            sealed.event_count
        );

        // Mark as archived
        dedupe.mark_archived(sealed.event_ids.iter())?;
    }

    // Flush dedupe
    dedupe.flush()?;

    // Flush relay manager stats
    if let Err(e) = relay_manager.flush() {
        tracing::warn!("Failed to flush relay stats: {}", e);
    }

    // Save final checkpoint
    let final_checkpoint = max_created_at.load(Ordering::Relaxed);
    if final_checkpoint > 0 {
        match relay_manager.update_last_archived_timestamp(final_checkpoint) {
            Ok(()) => tracing::info!("Saved final checkpoint: {}", final_checkpoint),
            Err(e) => tracing::warn!("Failed to save final checkpoint: {}", e),
        }
    }

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

    // Get final relay manager stats
    let relay_stats = relay_manager.get_aggregate_stats().ok();

    // Get final counts from atomics
    let final_received = events_received.load(Ordering::Relaxed);
    let final_processed = events_processed.load(Ordering::Relaxed);
    let final_deduplicated = events_deduplicated.load(Ordering::Relaxed);

    // Print summary
    tracing::info!("═══════════════════════════════════════════════════════");
    tracing::info!("SHUTDOWN COMPLETE");
    tracing::info!("═══════════════════════════════════════════════════════");
    tracing::info!("Events received:      {}", final_received);
    tracing::info!("Events processed:     {}", final_processed);
    tracing::info!("Events deduplicated:  {}", final_deduplicated);
    tracing::info!(
        "Relays connected:     {}",
        stats.source_metadata.relays_connected.unwrap_or(0)
    );
    tracing::info!(
        "Relays discovered:    {}",
        stats.source_metadata.relays_discovered.unwrap_or(0)
    );
    if let Some(rs) = relay_stats {
        tracing::info!("───────────────────────────────────────────────────────");
        tracing::info!("Relay Manager Stats:");
        tracing::info!("  Total relays tracked:  {}", rs.total_relays);
        tracing::info!("  Active relays:         {}", rs.active_relays);
        tracing::info!("  Blocked relays:        {}", rs.blocked_relays);
        tracing::info!("  Seed relays:           {}", rs.seed_relays);
        tracing::info!("  Discovered relays:     {}", rs.discovered_relays);
        tracing::info!("  Avg quality score:     {:.3}", rs.avg_score);
    }

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
