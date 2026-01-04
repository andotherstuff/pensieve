//! Negentropy sync implementation for NIP-77 reconciliation.
//!
//! This module provides periodic sync with trusted relays using the negentropy
//! set reconciliation protocol (NIP-77).

use super::SyncStateDb;
use crate::Result;
use metrics::{counter, gauge, histogram};
use nostr_sdk::prelude::*;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

    /// Maximum events to fetch per sync cycle.
    /// Limits memory usage during large syncs.
    pub max_events_per_cycle: usize,

    /// Timeout for the negentropy protocol messages.
    pub protocol_timeout: Duration,

    /// Timeout for fetching missing events after reconciliation.
    pub fetch_timeout: Duration,

    /// Direction for sync (default: Down = receive only).
    pub direction: SyncDirection,
}

impl Default for NegentropySyncConfig {
    fn default() -> Self {
        Self {
            relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.primal.net".to_string(),
            ],
            interval: Duration::from_secs(600), // 10 minutes
            lookback: Duration::from_secs(7 * 24 * 3600), // 7 days
            max_events_per_cycle: 1_000_000, // 1 million
            protocol_timeout: Duration::from_secs(60),
            fetch_timeout: Duration::from_secs(120),
            direction: SyncDirection::Down,
        }
    }
}

/// Statistics from a single sync cycle.
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    /// Number of events discovered during reconciliation.
    pub events_discovered: usize,
    /// Number of events successfully fetched.
    pub events_fetched: usize,
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

/// NostrDatabase adapter that wraps SyncStateDb.
///
/// This provides a minimal implementation of the NostrDatabase trait,
/// storing only (event_id, timestamp) pairs for negentropy reconciliation.
/// It does NOT store full events.
pub struct SyncStateAdapter {
    inner: Arc<SyncStateDb>,
}

impl Debug for SyncStateAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncStateAdapter").finish()
    }
}

impl SyncStateAdapter {
    /// Create a new adapter wrapping a SyncStateDb.
    pub fn new(db: Arc<SyncStateDb>) -> Self {
        Self { inner: db }
    }
}

impl NostrDatabase for SyncStateAdapter {
    fn backend(&self) -> Backend {
        Backend::Custom("sync-state".to_string())
    }

    fn save_event<'a>(
        &'a self,
        event: &'a Event,
    ) -> BoxedFuture<'a, std::result::Result<SaveEventStatus, DatabaseError>> {
        Box::pin(async move {
            // Record the event in sync state (just id + timestamp)
            self.inner
                .record(event.id.as_bytes(), event.created_at.as_secs())
                .map_err(DatabaseError::backend)?;
            Ok(SaveEventStatus::Success)
        })
    }

    fn check_id<'a>(
        &'a self,
        _event_id: &'a EventId,
    ) -> BoxedFuture<'a, std::result::Result<DatabaseEventStatus, DatabaseError>> {
        // We don't track deleted events in sync state
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
                .inner
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

            // Query all items since that timestamp
            let items = self
                .inner
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
    /// Returns statistics about the sync and the events that were discovered
    /// but need to be processed by the caller.
    pub async fn sync_once(&self) -> Result<(SyncStats, Vec<Event>)> {
        let start = Instant::now();
        let mut stats = SyncStats::default();

        tracing::info!(
            "Starting negentropy sync with {} relays, lookback {}s",
            self.config.relays.len(),
            self.config.lookback.as_secs()
        );

        // Build filter for the lookback window
        let now = Timestamp::now();
        let since = Timestamp::from(now.as_secs().saturating_sub(self.config.lookback.as_secs()));
        let filter = Filter::new().since(since);

        // Create the database adapter
        let adapter = SyncStateAdapter::new(Arc::clone(&self.sync_state));

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
        let sync_result = tokio::time::timeout(
            self.config.protocol_timeout + self.config.fetch_timeout,
            client.sync(filter.clone(), &opts),
        )
        .await;

        // Collect discovered events
        let mut discovered_events = Vec::new();

        match sync_result {
            Ok(Ok(output)) => {
                stats.events_discovered = output.received.len();
                stats.relays_responded = self.config.relays.len() - stats.relays_errored;

                tracing::info!(
                    "Negentropy reconciliation complete: {} events discovered",
                    stats.events_discovered
                );

                // Fetch the actual events for the discovered IDs
                if !output.received.is_empty() {
                    // The events should already be fetched by nostr-sdk during sync
                    // Query them from the database (they were saved during sync)
                    // Actually, nostr-sdk fetches events after reconciliation automatically
                    // and calls save_event on the database. But our adapter doesn't store
                    // full events, so we need to fetch them again.

                    // Build a filter for the received event IDs
                    let ids: Vec<EventId> = output.received.iter().cloned().collect();
                    let id_filter = Filter::new().ids(ids.clone());

                    // Fetch events using REQ
                    match tokio::time::timeout(
                        self.config.fetch_timeout,
                        client.fetch_events(id_filter, self.config.fetch_timeout),
                    )
                    .await
                    {
                        Ok(Ok(events)) => {
                            stats.events_fetched = events.len();
                            discovered_events = events.into_iter().collect();
                            tracing::info!("Fetched {} events from relays", stats.events_fetched);
                        }
                        Ok(Err(e)) => {
                            tracing::warn!("Failed to fetch events: {}", e);
                        }
                        Err(_) => {
                            tracing::warn!("Timeout fetching events");
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("Negentropy sync failed: {}", e);
                stats.relays_errored = self.config.relays.len();
            }
            Err(_) => {
                tracing::warn!("Negentropy sync timed out");
                stats.relays_errored = self.config.relays.len();
            }
        }

        // Disconnect
        client.disconnect().await;

        stats.duration = start.elapsed();

        // Update metrics
        counter!("negentropy_syncs_total").increment(1);
        counter!("negentropy_events_discovered_total").increment(stats.events_discovered as u64);
        counter!("negentropy_events_fetched_total").increment(stats.events_fetched as u64);
        histogram!("negentropy_sync_duration_seconds").record(stats.duration.as_secs_f64());
        gauge!("negentropy_last_sync_unix").set(Timestamp::now().as_secs() as f64);

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
    /// - Recording in sync state (already done by this syncer)
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
                        "Negentropy sync complete: {} discovered, {} fetched in {:?}",
                        stats.events_discovered,
                        stats.events_fetched,
                        stats.duration
                    );

                    // Process discovered events through the handler
                    for event in events {
                        match event_handler(&event) {
                            Ok(true) => {
                                // Event processed, record in sync state
                                if let Err(e) = self
                                    .sync_state
                                    .record(event.id.as_bytes(), event.created_at.as_secs())
                                {
                                    tracing::warn!("Failed to record event in sync state: {}", e);
                                }
                            }
                            Ok(false) => {
                                tracing::info!("Event handler signaled stop");
                                self.running.store(false, Ordering::SeqCst);
                                break;
                            }
                            Err(e) => {
                                tracing::error!("Event handler error: {}", e);
                                // Continue processing other events
                            }
                        }
                    }

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
        assert_eq!(config.relays.len(), 2);
        assert_eq!(config.interval, Duration::from_secs(600));
        assert_eq!(config.lookback, Duration::from_secs(7 * 24 * 3600));
    }
}
