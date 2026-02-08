//! Negentropy sync implementation for NIP-77 reconciliation.
//!
//! This module provides periodic sync with trusted relays using the negentropy
//! set reconciliation protocol (NIP-77).
//!
//! ## Event Flow
//!
//! Events are captured as they stream through the nostr-sdk `save_event()` callback
//! during negentropy reconciliation. This eliminates the need for a separate fetch step:
//!
//! 1. Negentropy reconciliation identifies missing event IDs
//! 2. nostr-sdk automatically fetches those events via REQ
//! 3. As events arrive, `save_event()` forwards them to a channel
//! 4. `sync_once()` collects events from the channel
//! 5. Events are passed to the handler for dedupe/segment writing
//! 6. Only after successful segment write is the event recorded in sync-state
//!
//! This ensures that sync-state only contains events we've actually archived,
//! so failed events will be re-fetched on the next sync cycle.

use super::SyncStateDb;
use crate::Result;
use metrics::{counter, gauge, histogram};
use nostr_sdk::prelude::*;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Configuration for negentropy sync.
#[derive(Debug, Clone)]
pub struct NegentropySyncConfig {
    /// Relay URLs to sync from (must support NIP-77).
    pub relays: Vec<String>,

    /// How often to run sync (e.g., every 10 minutes).
    pub interval: Duration,

    /// How far back to sync (e.g., last 7 days).
    /// This creates the `since` filter for negentropy reconciliation.
    pub lookback: Duration,

    /// Timeout for the negentropy protocol and event fetching.
    /// Events stream through as they're fetched, so this is the overall timeout.
    pub protocol_timeout: Duration,

    /// Direction for sync (default: Down = receive only).
    pub direction: SyncDirection,
}

impl Default for NegentropySyncConfig {
    fn default() -> Self {
        Self {
            // Only Damus confirmed to support NIP-77
            // Primal does NOT support NIP-77 (connects but ignores NEG-OPEN)
            relays: vec!["wss://relay.damus.io".to_string()],
            interval: Duration::from_secs(1800), // 30 minutes
            lookback: Duration::from_secs(14 * 24 * 3600), // 14 days
            protocol_timeout: Duration::from_secs(900), // 15 min for full sync
            direction: SyncDirection::Down,
        }
    }
}

/// Statistics from a single sync cycle.
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    /// Number of events discovered during reconciliation.
    pub events_discovered: usize,
    /// Number of events received via streaming (same as discovered for successful syncs).
    pub events_received: usize,
    /// Number of events that were already in our dedupe index.
    pub events_deduplicated: usize,
    /// Number of events written to segments.
    pub events_written: usize,
    /// Duration of the sync cycle.
    pub duration: Duration,
    /// Number of relays that responded successfully.
    pub relays_responded: usize,
    /// Number of relays that errored.
    pub relays_errored: usize,
}

/// NostrDatabase adapter that captures events during negentropy sync.
///
/// This provides a minimal implementation of the NostrDatabase trait that:
/// - Provides (event_id, timestamp) pairs for negentropy reconciliation via `negentropy_items()`
/// - Captures full events as they stream through `save_event()` via a channel
/// - Does NOT record events to sync-state (that's done after successful segment write)
pub struct SyncStateAdapter {
    /// Reference to sync state for querying what we have (negentropy_items).
    sync_state: Arc<SyncStateDb>,
    /// Channel to send captured events to the collector.
    event_sender: mpsc::UnboundedSender<Event>,
}

impl Debug for SyncStateAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncStateAdapter").finish()
    }
}

impl SyncStateAdapter {
    /// Create a new adapter with an event capture channel.
    ///
    /// Returns the adapter and a receiver for captured events.
    pub fn new(sync_state: Arc<SyncStateDb>) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                sync_state,
                event_sender: tx,
            },
            rx,
        )
    }
}

impl NostrDatabase for SyncStateAdapter {
    fn backend(&self) -> Backend {
        Backend::Custom("sync-state-capture".to_string())
    }

    fn save_event<'a>(
        &'a self,
        event: &'a Event,
    ) -> BoxedFuture<'a, std::result::Result<SaveEventStatus, DatabaseError>> {
        Box::pin(async move {
            // Forward the event to the channel for processing.
            // DO NOT record to sync-state here - that happens only after
            // successful segment write in the event handler.
            //
            // This ensures that if segment write fails, the event is NOT
            // recorded in sync-state and will be re-fetched on the next cycle.
            if self.event_sender.send(event.clone()).is_err() {
                // Channel closed, receiver dropped - sync is ending
                tracing::debug!("Event channel closed, sync ending");
            }
            Ok(SaveEventStatus::Success)
        })
    }

    fn check_id<'a>(
        &'a self,
        _event_id: &'a EventId,
    ) -> BoxedFuture<'a, std::result::Result<DatabaseEventStatus, DatabaseError>> {
        // Always return NotExistent so nostr-sdk fetches events we're missing.
        // The actual deduplication happens in our pipeline via DedupeIndex.
        Box::pin(async move { Ok(DatabaseEventStatus::NotExistent) })
    }

    fn event_by_id<'a>(
        &'a self,
        _event_id: &'a EventId,
    ) -> BoxedFuture<'a, std::result::Result<Option<Event>, DatabaseError>> {
        // We don't store full events, only IDs and timestamps
        Box::pin(async move { Ok(None) })
    }

    fn count(&self, _filter: Filter) -> BoxedFuture<'_, std::result::Result<usize, DatabaseError>> {
        Box::pin(async move {
            let count = self
                .sync_state
                .approximate_count()
                .map_err(DatabaseError::backend)?;
            Ok(count as usize)
        })
    }

    fn query(&self, filter: Filter) -> BoxedFuture<'_, std::result::Result<Events, DatabaseError>> {
        // We don't store full events, return empty Events
        Box::pin(async move { Ok(Events::new(&filter)) })
    }

    fn negentropy_items(
        &self,
        filter: Filter,
    ) -> BoxedFuture<'_, std::result::Result<Vec<(EventId, Timestamp)>, DatabaseError>> {
        Box::pin(async move {
            // Get the `since` timestamp from the filter
            let since = filter.since.map(|t| t.as_secs()).unwrap_or(0);

            // Query all items since that timestamp from sync-state.
            // These are events we've successfully archived.
            let items = self
                .sync_state
                .get_items_since(since)
                .map_err(DatabaseError::backend)?;

            // Convert to (EventId, Timestamp) pairs
            let result: Vec<(EventId, Timestamp)> = items
                .into_iter()
                .map(|(id_bytes, ts)| {
                    let event_id = EventId::from_byte_array(id_bytes);
                    (event_id, Timestamp::from(ts))
                })
                .collect();

            Ok(result)
        })
    }

    fn delete(&self, _filter: Filter) -> BoxedFuture<'_, std::result::Result<(), DatabaseError>> {
        // Not implemented for sync state
        Box::pin(async move { Ok(()) })
    }

    fn wipe(&self) -> BoxedFuture<'_, std::result::Result<(), DatabaseError>> {
        // Not implemented - use prune_before instead
        Box::pin(async move { Ok(()) })
    }
}

/// Negentropy syncer for periodic reconciliation with trusted relays.
pub struct NegentropySyncer {
    config: NegentropySyncConfig,
    sync_state: Arc<SyncStateDb>,
    running: Arc<AtomicBool>,
}

impl NegentropySyncer {
    /// Create a new negentropy syncer.
    pub fn new(config: NegentropySyncConfig, sync_state: Arc<SyncStateDb>) -> Self {
        Self {
            config,
            sync_state,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Check if the syncer is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Signal the syncer to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Get the sync state database.
    pub fn sync_state(&self) -> &Arc<SyncStateDb> {
        &self.sync_state
    }

    /// Run a single sync cycle.
    ///
    /// Returns statistics about the sync and the events that were captured
    /// as they streamed through during reconciliation.
    ///
    /// Events are captured directly from the nostr-sdk `save_event()` callback,
    /// eliminating the need for a separate fetch step.
    pub async fn sync_once(&self) -> Result<(SyncStats, Vec<Event>)> {
        let start = Instant::now();
        let mut stats = SyncStats::default();

        // Mark sync as in progress
        gauge!("negentropy_sync_in_progress").set(1.0);

        tracing::info!(
            "Starting negentropy sync with {} relays, lookback {}s",
            self.config.relays.len(),
            self.config.lookback.as_secs()
        );

        // Build filter for the lookback window
        let now = Timestamp::now();
        let since = Timestamp::from(now.as_secs().saturating_sub(self.config.lookback.as_secs()));
        let filter = Filter::new().since(since);

        // Create the database adapter with event capture channel
        let (adapter, mut event_receiver) = SyncStateAdapter::new(Arc::clone(&self.sync_state));

        // Collect events as they stream through
        let collected_events = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let events_received_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let collector_events = Arc::clone(&collected_events);
        let collector_counter = Arc::clone(&events_received_count);

        // Spawn a task to collect events from the channel
        // This updates the "receiving" gauge in real-time as events stream in
        let collector_handle = tokio::spawn(async move {
            while let Some(event) = event_receiver.recv().await {
                let count = collector_counter.fetch_add(1, Ordering::Relaxed) + 1;
                // Update receiving progress gauge every 100 events
                if count.is_multiple_of(100) {
                    gauge!("negentropy_events_receiving").set(count as f64);
                }
                collector_events.lock().await.push(event);
            }
            // Final update
            let final_count = collector_counter.load(Ordering::Relaxed);
            gauge!("negentropy_events_receiving").set(final_count as f64);
        });

        // Create a client with our sync state database
        let keys = Keys::generate();
        let client = Client::builder()
            .signer(keys)
            .database(Arc::new(adapter) as Arc<dyn NostrDatabase>)
            .build();

        // Add trusted relays
        for relay_url in &self.config.relays {
            if let Err(e) = client.add_relay(relay_url).await {
                tracing::warn!("Failed to add negentropy relay {}: {}", relay_url, e);
                stats.relays_errored += 1;
            }
        }

        // Connect to relays
        client.connect().await;

        // Wait for connections to establish
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Build sync options
        let opts = SyncOptions::default().direction(self.config.direction);

        // Run negentropy reconciliation
        // Events stream through save_event() as they're fetched by nostr-sdk
        let sync_result = tokio::time::timeout(
            self.config.protocol_timeout,
            client.sync(filter.clone(), &opts),
        )
        .await;

        match sync_result {
            Ok(Ok(output)) => {
                stats.events_discovered = output.received.len();
                stats.relays_responded = self.config.relays.len() - stats.relays_errored;

                tracing::info!(
                    "Negentropy reconciliation complete: {} events discovered",
                    stats.events_discovered
                );
            }
            Ok(Err(e)) => {
                tracing::warn!("Negentropy sync failed: {}", e);
                stats.relays_errored = self.config.relays.len();
            }
            Err(_) => {
                tracing::warn!(
                    "Negentropy sync timed out after {:?}",
                    self.config.protocol_timeout
                );
                stats.relays_errored = self.config.relays.len();
            }
        }

        // Disconnect - this closes the adapter's channel sender
        client.disconnect().await;

        // Drop the client to ensure the adapter (and its sender) is dropped
        drop(client);

        // Wait for collector to finish receiving all events
        // The channel will close when the adapter is dropped
        if let Err(e) = collector_handle.await {
            tracing::warn!("Event collector task failed: {}", e);
        }

        // Get collected events
        let discovered_events = Arc::try_unwrap(collected_events)
            .expect("collector handle finished, should be sole owner")
            .into_inner();

        stats.events_received = discovered_events.len();
        stats.duration = start.elapsed();

        tracing::info!(
            "Negentropy sync captured {} events in {:?}",
            stats.events_received,
            stats.duration
        );

        // Update metrics
        counter!("negentropy_syncs_total").increment(1);
        counter!("negentropy_events_discovered_total").increment(stats.events_discovered as u64);
        counter!("negentropy_events_received_total").increment(stats.events_received as u64);
        histogram!("negentropy_sync_duration_seconds").record(stats.duration.as_secs_f64());
        gauge!("negentropy_last_sync_unix").set(Timestamp::now().as_secs() as f64);

        // Mark sync as complete
        gauge!("negentropy_sync_in_progress").set(0.0);
        gauge!("negentropy_events_receiving").set(0.0);

        Ok((stats, discovered_events))
    }

    /// Run periodic sync in the background.
    ///
    /// The `event_handler` is called for each discovered event. It should
    /// return `Ok(true)` to continue, `Ok(false)` to stop, or `Err` on error.
    ///
    /// The handler is responsible for:
    /// - Dedupe checking via DedupeIndex
    /// - Packing and writing to segments
    ///
    /// Sync state is updated ONLY after the handler returns `Ok(true)`,
    /// ensuring that failed events will be re-fetched on the next sync cycle.
    pub async fn run_periodic<F>(&self, mut event_handler: F) -> Result<()>
    where
        F: FnMut(&Event) -> Result<bool> + Send,
    {
        self.running.store(true, Ordering::SeqCst);

        tracing::info!(
            "Starting periodic negentropy sync (interval: {}s, lookback: {}s)",
            self.config.interval.as_secs(),
            self.config.lookback.as_secs()
        );

        while self.running.load(Ordering::SeqCst) {
            // Run a sync cycle
            match self.sync_once().await {
                Ok((stats, events)) => {
                    tracing::info!(
                        "Negentropy sync complete: {} discovered, {} received in {:?}",
                        stats.events_discovered,
                        stats.events_received,
                        stats.duration
                    );

                    // Track batch processing statistics
                    let total_events = events.len();
                    let mut events_succeeded = 0usize;
                    let mut events_failed = 0usize;
                    let process_start = Instant::now();

                    // Process discovered events through the handler
                    // Note: The handler is responsible for emitting detailed metrics
                    // (written vs deduplicated) since it knows the distinction.
                    // We only track success/failure here at the batch level.
                    for event in events {
                        match event_handler(&event) {
                            Ok(true) => {
                                // Event processed successfully, NOW record in sync state.
                                // This ensures we only record events that were actually archived.
                                if let Err(e) = self
                                    .sync_state
                                    .record(event.id.as_bytes(), event.created_at.as_secs())
                                {
                                    tracing::warn!("Failed to record event in sync state: {}", e);
                                }
                                events_succeeded += 1;
                            }
                            Ok(false) => {
                                tracing::info!("Event handler signaled stop");
                                self.running.store(false, Ordering::SeqCst);
                                break;
                            }
                            Err(e) => {
                                // Event failed to process - do NOT record in sync state.
                                // It will be re-fetched on the next sync cycle.
                                tracing::debug!(
                                    "Event handler error (will retry next sync): {}",
                                    e
                                );
                                events_failed += 1;
                                counter!("negentropy_events_failed_total").increment(1);
                            }
                        }
                    }

                    let process_duration = process_start.elapsed();

                    // Log batch summary
                    tracing::info!(
                        "Negentropy batch processed: {} events → {} succeeded, {} failed in {:?}",
                        total_events,
                        events_succeeded,
                        events_failed,
                        process_duration
                    );

                    // Update batch metrics (gauges for last-batch visibility)
                    // Note: succeeded includes both novel (written) and deduplicated events
                    gauge!("negentropy_last_batch_total").set(total_events as f64);
                    gauge!("negentropy_last_batch_succeeded").set(events_succeeded as f64);
                    gauge!("negentropy_last_batch_failed").set(events_failed as f64);
                    histogram!("negentropy_batch_process_duration_seconds")
                        .record(process_duration.as_secs_f64());

                    // Prune sync state entries older than the lookback window
                    // This bounds storage to approximately lookback_duration worth of events
                    let prune_before = Timestamp::now()
                        .as_secs()
                        .saturating_sub(self.config.lookback.as_secs());
                    match self.sync_state.prune_before(prune_before) {
                        Ok(pruned) => {
                            if pruned > 0 {
                                tracing::debug!("Pruned {} old entries from sync state", pruned);
                                counter!("negentropy_sync_state_pruned_total")
                                    .increment(pruned as u64);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to prune sync state: {}", e);
                        }
                    }

                    // Update sync state size metric
                    if let Ok(count) = self.sync_state.approximate_count() {
                        gauge!("negentropy_sync_state_items").set(count as f64);
                    }
                }
                Err(e) => {
                    tracing::error!("Negentropy sync cycle failed: {}", e);
                    counter!("negentropy_sync_errors_total").increment(1);
                }
            }

            // Wait for the next interval (or until stopped)
            let interval = self.config.interval;
            let start = Instant::now();
            while start.elapsed() < interval && self.running.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        tracing::info!("Negentropy sync stopped");
        Ok(())
    }
}

/// Seed the sync state database from ClickHouse.
///
/// This is used during cold-start to populate the sync state with events
/// from the lookback window. Without this, the first negentropy sync would
/// believe we have no local items and request a very large delta.
///
/// # Arguments
///
/// * `sync_state` - The sync state database to populate
/// * `clickhouse_url` - ClickHouse server URL (e.g., "http://localhost:8123")
/// * `clickhouse_db` - ClickHouse database name
/// * `lookback_days` - How many days back to seed
///
/// # Returns
///
/// The number of events seeded, or an error if seeding failed.
pub async fn seed_from_clickhouse(
    sync_state: &SyncStateDb,
    clickhouse_url: &str,
    clickhouse_db: &str,
    lookback_days: u64,
) -> Result<usize> {
    use clickhouse::{Client, Row};
    use serde::Deserialize;

    #[derive(Debug, Row, Deserialize)]
    struct SeedRow {
        id: String,
        created_at: u32,
    }

    tracing::info!(
        "Seeding sync state from ClickHouse (lookback: {} days)",
        lookback_days
    );

    let client = Client::default()
        .with_url(clickhouse_url)
        .with_database(clickhouse_db);

    // Query events from the lookback window
    let query = format!(
        "SELECT id, toUnixTimestamp(created_at) AS created_at \
         FROM events_local \
         WHERE created_at >= now() - INTERVAL {} DAY",
        lookback_days
    );

    let rows: Vec<SeedRow> = client
        .query(&query)
        .fetch_all()
        .await
        .map_err(crate::Error::ClickHouse)?;

    tracing::info!("Fetched {} events from ClickHouse for seeding", rows.len());

    // Convert and insert into sync state
    let mut count = 0usize;
    let mut batch = Vec::with_capacity(10_000);

    for row in rows {
        // Decode hex ID to bytes
        let id_bytes = match hex::decode(&row.id) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                arr
            }
            _ => {
                tracing::debug!("Skipping invalid event ID: {}", row.id);
                continue;
            }
        };

        batch.push((id_bytes, row.created_at as u64));

        if batch.len() >= 10_000 {
            let batch_count = sync_state.record_batch(batch.iter().map(|(id, ts)| (id, *ts)))?;
            count += batch_count;
            batch.clear();
        }
    }

    // Insert remaining batch
    if !batch.is_empty() {
        let batch_count = sync_state.record_batch(batch.iter().map(|(id, ts)| (id, *ts)))?;
        count += batch_count;
    }

    tracing::info!("Seeded sync state with {} events from ClickHouse", count);
    counter!("negentropy_seed_events_total").increment(count as u64);

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = NegentropySyncConfig::default();
        assert_eq!(config.relays.len(), 1);
        assert_eq!(config.interval, Duration::from_secs(1800));
        assert_eq!(config.lookback, Duration::from_secs(14 * 24 * 3600));
    }
}
