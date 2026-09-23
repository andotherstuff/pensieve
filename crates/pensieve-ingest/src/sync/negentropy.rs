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
//! 4. `sync_once()` streams a bounded queue directly to archive admission
//! 5. Each relay has an independent deadline and terminal result
//! 6. Only after successful segment write is the event recorded in sync-state
//!
//! This ensures that sync-state only contains events we've actually archived,
//! so failed events will be re-fetched on the next sync cycle.

use super::SyncStateDb;
use crate::Result;
use crate::logging::compact_error;
use crate::pipeline::{DedupeIndex, EventStatus};
use futures_util::{FutureExt, StreamExt, stream};
use metrics::{counter, gauge, histogram};
use nostr_sdk::prelude::*;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

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

    /// Per-relay deadline including setup, connection, reconciliation and shutdown.
    pub protocol_timeout: Duration,

    /// Maximum simultaneously active relay lifecycles.
    pub max_concurrent_relays: usize,

    /// Maximum queued events across all relay workers.
    pub event_queue_capacity: usize,

    /// Maximum normalized event JSON size admitted by the adapter.
    pub max_event_bytes: usize,

    /// Maximum local inventory IDs per relay request. Overflow fails closed.
    pub max_inventory_items: usize,

    /// Direction for sync (default: Down = receive only).
    pub direction: SyncDirection,
}

impl Default for NegentropySyncConfig {
    fn default() -> Self {
        Self {
            // Relays confirmed to support NIP-77:
            //   - relay.damus.io
            //   - relay.divine.video
            //   - nos.lol
            // Primal does NOT support NIP-77 (connects but ignores NEG-OPEN)
            relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.divine.video".to_string(),
                "wss://nos.lol".to_string(),
            ],
            interval: Duration::from_secs(1800), // 30 minutes
            lookback: Duration::from_secs(14 * 24 * 3600), // 14 days
            protocol_timeout: Duration::from_secs(900), // 15 min for full sync
            max_concurrent_relays: 3,
            event_queue_capacity: 128,
            max_event_bytes: 1024 * 1024,
            max_inventory_items: 250_000,
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
    /// Independent terminal result for each attempted relay.
    pub relay_results: Vec<RelaySyncResult>,
}

/// Terminal result for one relay; protocol completion is not durable admission.
#[derive(Debug, Clone)]
pub struct RelaySyncResult {
    /// Relay URL.
    pub relay_url: String,
    /// Received IDs reported by the SDK on successful reconciliation.
    pub received: usize,
    /// Failure, timeout, or panic description; absent only on completion.
    pub error: Option<String>,
}

// No detached collector or worker tasks: dropping the cycle drops its futures.
struct CycleGuard;
impl Drop for CycleGuard {
    fn drop(&mut self) {
        gauge!("negentropy_sync_in_progress").set(0.0);
        gauge!("negentropy_events_receiving").set(0.0);
    }
}

struct RunningGuard<'a>(&'a AtomicBool);
impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// SDK disconnect signals termination synchronously, including its notification loops.
struct RelayGuard(Relay);
impl Drop for RelayGuard {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}

async fn bounded_relay<F>(relay_url: String, deadline: Duration, work: F) -> RelaySyncResult
where
    F: std::future::Future<Output = std::result::Result<usize, String>>,
{
    let result =
        tokio::time::timeout(deadline, std::panic::AssertUnwindSafe(work).catch_unwind()).await;
    let result = match result {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("relay worker panicked".to_string()),
        Err(_) => Err("relay lifecycle deadline exceeded".to_string()),
    };
    let (received, error) = match result {
        Ok(count) => (count, None),
        Err(error) => (0, Some(error)),
    };
    tracing::info!(%relay_url, phase = "finished", ?error, received, "Negentropy relay result");
    RelaySyncResult {
        relay_url,
        received,
        error,
    }
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
    event_sender: mpsc::Sender<Event>,
    admission_failed: Arc<AtomicBool>,
    max_event_bytes: usize,
    max_inventory_items: usize,
}

impl Debug for SyncStateAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncStateAdapter").finish()
    }
}

#[cfg(test)]
impl SyncStateAdapter {
    /// Create a new adapter with an event capture channel.
    ///
    /// Returns the adapter and a receiver for captured events.
    pub fn new(sync_state: Arc<SyncStateDb>) -> (Self, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(128);
        (
            Self {
                sync_state,
                event_sender: tx,
                admission_failed: Arc::new(AtomicBool::new(false)),
                max_event_bytes: 1024 * 1024,
                max_inventory_items: 250_000,
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
            // Backpressure only this relay's connection task. Its outer lifecycle
            // deadline still bounds the wait; ordinary bursts must not drop events.
            let result = async {
                if event.as_json().len() > self.max_event_bytes {
                    return Err(DatabaseError::backend(std::io::Error::other(
                        "negentropy event exceeds admission size cap",
                    )));
                }
                crate::pipeline::validate_archive_event(event).map_err(DatabaseError::backend)?;
                self.event_sender.send(event.clone()).await.map_err(|_| {
                    DatabaseError::backend(std::io::Error::other(
                        "negentropy admission queue closed",
                    ))
                })
            }
            .await;
            if result.is_err() {
                self.admission_failed.store(true, Ordering::SeqCst);
            }
            result?;
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
            // This adapter only supports finite, unqualified time intervals.
            // Refuse richer filters rather than advertising IDs outside them.
            let since = filter
                .since
                .ok_or_else(|| {
                    DatabaseError::backend(std::io::Error::other("inventory requires since"))
                })?
                .as_secs();
            let until = filter
                .until
                .ok_or_else(|| {
                    DatabaseError::backend(std::io::Error::other("inventory requires until"))
                })?
                .as_secs();
            if filter
                != Filter::new()
                    .since(Timestamp::from(since))
                    .until(Timestamp::from(until))
            {
                return Err(DatabaseError::backend(std::io::Error::other(
                    "unsupported inventory filter",
                )));
            }
            let items = self
                .sync_state
                .get_items_range(since, until, self.max_inventory_items)
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

/// A source of additional reconciliation targets, re-queried each sync cycle
/// (e.g. NIP-77 relays from the NIP-66 catalog).
pub type TargetProvider = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Negentropy syncer for periodic reconciliation with trusted relays.
pub struct NegentropySyncer {
    config: NegentropySyncConfig,
    sync_state: Arc<SyncStateDb>,
    running: Arc<AtomicBool>,
    stop_signal: watch::Sender<bool>,
    /// Optional dedupe index, used to confirm an event is DURABLY archived before
    /// advancing sync-state. Without this gate, sync-state could record an event
    /// that was only written to an unsealed segment; a crash would then drop it
    /// from the archive *and* stop negentropy from ever re-fetching it (H4).
    dedupe: Option<Arc<DedupeIndex>>,
    /// Dynamic reconciliation targets, merged with `config.relays` each cycle.
    target_provider: Option<TargetProvider>,
}

impl NegentropySyncer {
    /// Create a new negentropy syncer.
    ///
    /// `dedupe` should be the shared dedupe index so sync-state is only advanced
    /// for events that are durably archived. Pass `None` only in tests.
    pub fn new(
        config: NegentropySyncConfig,
        sync_state: Arc<SyncStateDb>,
        dedupe: Option<Arc<DedupeIndex>>,
    ) -> Self {
        Self {
            config,
            sync_state,
            running: Arc::new(AtomicBool::new(false)),
            stop_signal: watch::channel(false).0,
            dedupe,
            target_provider: None,
        }
    }

    /// Provide a dynamic source of additional reconciliation targets, re-queried
    /// each sync cycle and merged (deduped) with the configured base relays.
    pub fn with_target_provider(mut self, provider: TargetProvider) -> Self {
        self.target_provider = Some(provider);
        self
    }

    /// Effective relay set for a cycle: configured base relays plus any dynamic
    /// targets, deduped. Recomputed each cycle so catalog growth is followed
    /// without a restart.
    fn effective_relays(&self) -> Vec<String> {
        let mut relays = self.config.relays.clone();
        if let Some(provider) = &self.target_provider {
            for t in provider() {
                if !relays.contains(&t) {
                    relays.push(t);
                }
            }
        }
        relays
    }

    /// Check if the syncer is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Signal the syncer to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.stop_signal.send_replace(true);
    }

    /// Get the sync state database.
    pub fn sync_state(&self) -> &Arc<SyncStateDb> {
        &self.sync_state
    }

    async fn relay_worker(
        &self,
        relay_url: String,
        filter: Filter,
        sender: mpsc::Sender<Event>,
    ) -> RelaySyncResult {
        let url = relay_url.clone();
        bounded_relay(relay_url, self.config.protocol_timeout, async {
            tracing::info!(relay_url = %url, phase = "setup", "Negentropy relay phase");
            let failed = Arc::new(AtomicBool::new(false));
            let adapter = SyncStateAdapter {
                sync_state: Arc::clone(&self.sync_state),
                event_sender: sender,
                admission_failed: Arc::clone(&failed),
                max_event_bytes: self.config.max_event_bytes,
                max_inventory_items: self.config.max_inventory_items,
            };
            let client = Client::builder()
                .signer(Keys::generate())
                .database(Arc::new(adapter) as Arc<dyn NostrDatabase>)
                .build();
            client
                .add_relay(&url)
                .await
                .map_err(|e| compact_error(&e))?;
            let relay = RelayGuard(client.relay(&url).await.map_err(|e| compact_error(&e))?);
            tracing::info!(relay_url = %url, phase = "connect", "Negentropy relay phase");
            relay.0.connect();
            relay.0.wait_for_connection(Duration::from_secs(10)).await;
            tracing::info!(relay_url = %url, phase = "reconcile", "Negentropy relay phase");
            let opts = SyncOptions::default().direction(self.config.direction);
            // Single relay API: no pool-wide join_all and no misleading Output.success.
            let output = relay
                .0
                .sync(filter, &opts)
                .await
                .map_err(|e| compact_error(&e))?;
            if !output.remote.is_subset(&output.received) {
                return Err("relay reconciliation left remote events unfetched".to_string());
            }
            tracing::info!(relay_url = %url, phase = "disconnect", "Negentropy relay phase");
            relay.0.disconnect();
            client.shutdown().await;
            if failed.load(Ordering::SeqCst) {
                return Err(
                    "relay admission incomplete: rejected, oversized, or backpressured event"
                        .to_string(),
                );
            }
            Ok(output.received.len())
        })
        .await
    }

    /// Reconcile independently while streaming events directly to archive admission.
    ///
    /// The synchronous handler must return promptly; async deadlines cannot preempt
    /// blocking filesystem calls. Queued events are not proof of durable storage.
    pub async fn sync_once<F>(&self, mut event_handler: F) -> Result<SyncStats>
    where
        F: FnMut(&Event) -> Result<bool> + Send,
    {
        if self.config.max_concurrent_relays == 0
            || self.config.event_queue_capacity == 0
            || self.config.max_event_bytes == 0
            || self.config.max_inventory_items == 0
            || self.config.protocol_timeout.is_zero()
        {
            return Err(crate::Error::Config(
                "negentropy limits must be positive".to_string(),
            ));
        }
        let mut stop = self.stop_signal.subscribe();
        if *stop.borrow() {
            return Err(crate::Error::Config("negentropy stopped".to_string()));
        }
        let start = Instant::now();
        let mut stats = SyncStats::default();
        let relays = self.effective_relays();
        gauge!("ingest_negentropy_targets").set(relays.len() as f64);
        gauge!("negentropy_sync_in_progress").set(1.0);
        let _guard = CycleGuard;
        let now = Timestamp::now();
        let since = Timestamp::from(now.as_secs().saturating_sub(self.config.lookback.as_secs()));
        let filter = Filter::new().since(since).until(now);
        let (sender, mut receiver) = mpsc::channel(self.config.event_queue_capacity);
        let workers = stream::iter(
            relays
                .into_iter()
                .map(|url| self.relay_worker(url, filter.clone(), sender.clone())),
        )
        .buffer_unordered(self.config.max_concurrent_relays);
        self.drive_workers(
            workers,
            &mut receiver,
            &mut stop,
            &mut event_handler,
            &mut stats,
        )
        .await?;
        stats.duration = start.elapsed();
        counter!("negentropy_syncs_total").increment(1);
        counter!("negentropy_events_discovered_total").increment(stats.events_discovered as u64);
        counter!("negentropy_events_received_total").increment(stats.events_received as u64);
        histogram!("negentropy_sync_duration_seconds").record(stats.duration.as_secs_f64());
        gauge!("negentropy_last_cycle_unix").set(Timestamp::now().as_secs() as f64);
        if stats.relays_responded > 0 && stats.relays_errored == 0 {
            gauge!("negentropy_last_sync_unix").set(Timestamp::now().as_secs() as f64);
        }
        gauge!("negentropy_last_batch_total").set(stats.events_received as f64);
        Ok(stats)
    }

    async fn drive_workers<S, F>(
        &self,
        workers: S,
        receiver: &mut mpsc::Receiver<Event>,
        stop: &mut watch::Receiver<bool>,
        event_handler: &mut F,
        stats: &mut SyncStats,
    ) -> Result<()>
    where
        S: futures_util::Stream<Item = RelaySyncResult>,
        F: FnMut(&Event) -> Result<bool> + Send,
    {
        tokio::pin!(workers);
        loop {
            tokio::select! {
                biased;
                _ = stop.changed() => return Err(crate::Error::Config("negentropy cancelled".to_string())),
                result = workers.next() => {
                    match result {
                        Some(result) => {
                            if result.error.is_some() {
                                stats.relays_errored += 1;
                            } else {
                                stats.relays_responded += 1;
                                stats.events_discovered += result.received;
                            }
                            stats.relay_results.push(result);
                        }
                        None => break,
                    }
                }
                Some(event) = receiver.recv() => self.admit_event(&event, event_handler, stats)?,
            }
        }
        // Explicitly close, then drain only the bounded backlog. Do not wait for
        // SDK background tasks to release their database/channel references.
        receiver.close();
        while let Ok(event) = receiver.try_recv() {
            if *stop.borrow() {
                return Err(crate::Error::Config("negentropy cancelled".to_string()));
            }
            self.admit_event(&event, event_handler, stats)?;
        }
        Ok(())
    }

    fn admit_event<F>(&self, event: &Event, handler: &mut F, stats: &mut SyncStats) -> Result<()>
    where
        F: FnMut(&Event) -> Result<bool>,
    {
        stats.events_received += 1;
        gauge!("negentropy_events_receiving").set(stats.events_received as f64);
        if !handler(event)? {
            self.stop();
            return Err(crate::Error::Config(
                "negentropy handler requested stop".to_string(),
            ));
        }
        let durable = match &self.dedupe {
            Some(dedupe) => matches!(
                dedupe.get_status(event.id.as_bytes())?,
                Some(EventStatus::Archived)
            ),
            None => true,
        };
        if durable {
            self.sync_state
                .record(event.id.as_bytes(), event.created_at.as_secs())?;
        }
        Ok(())
    }

    /// Run cycles until stopped. Admission errors fail the task closed.
    pub async fn run_periodic<F>(&self, mut event_handler: F) -> Result<()>
    where
        F: FnMut(&Event) -> Result<bool> + Send,
    {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(crate::Error::Config(
                "negentropy already running".to_string(),
            ));
        }
        let _guard = RunningGuard(&self.running);
        let mut stop = self.stop_signal.subscribe();
        loop {
            if *stop.borrow() {
                break;
            }
            let stats = match self.sync_once(&mut event_handler).await {
                Ok(stats) => stats,
                Err(_) if *stop.borrow() => break,
                Err(error) => {
                    counter!("negentropy_sync_errors_total").increment(1);
                    return Err(error);
                }
            };
            tracing::info!(
                received = stats.events_received,
                completed = stats.relays_responded,
                failed = stats.relays_errored,
                elapsed_secs = stats.duration.as_secs_f64(),
                "Negentropy cycle finished"
            );
            // Inventory pruning/coverage policy is a separate slice. Do not prune
            // automatically after a partial or failed reconciliation cycle.
            tokio::select! {
                _ = stop.changed() => break,
                _ = tokio::time::sleep(self.config.interval) => {}
            }
        }
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

    sync_state.begin_seed()?;

    // Query events from the lookback window
    let query = format!(
        "SELECT id, toUnixTimestamp(created_at) AS created_at \
         FROM events_local \
         WHERE created_at >= now() - INTERVAL {} DAY",
        lookback_days
    );

    let mut rows = client
        .query(&query)
        .fetch::<SeedRow>()
        .map_err(crate::Error::ClickHouse)?;

    // Convert and insert into sync state
    let mut count = 0usize;
    let mut batch = Vec::with_capacity(10_000);

    while let Some(row) = rows.next().await.map_err(crate::Error::ClickHouse)? {
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

    sync_state.complete_seed()?;
    tracing::info!("Seeded sync state with {} events from ClickHouse", count);
    counter!("negentropy_seed_events_total").increment(count as u64);

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn test_syncer(dir: &tempfile::TempDir) -> NegentropySyncer {
        NegentropySyncer::new(
            NegentropySyncConfig::default(),
            Arc::new(SyncStateDb::open(dir.path()).unwrap()),
            None,
        )
    }

    #[tokio::test]
    async fn adapter_requires_exact_finite_interval_and_rejects_inventory_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SyncStateDb::open(dir.path()).unwrap());
        state.record(&[1; 32], 100).unwrap();
        state.record(&[2; 32], 101).unwrap();
        state.record(&[3; 32], 101).unwrap();
        state.record(&[4; 32], 102).unwrap();
        let (mut adapter, _rx) = SyncStateAdapter::new(state);
        adapter.max_inventory_items = 2;
        let filter = Filter::new()
            .since(Timestamp::from(101))
            .until(Timestamp::from(101));
        assert_eq!(
            adapter
                .negentropy_items(filter.clone())
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            adapter
                .negentropy_items(filter.clone().limit(1))
                .await
                .is_err()
        );
        assert!(
            adapter
                .negentropy_items(filter.clone().kind(Kind::TextNote))
                .await
                .is_err()
        );
        assert!(
            adapter
                .negentropy_items(Filter::new().since(Timestamp::from(101)))
                .await
                .is_err()
        );
        assert!(
            adapter
                .negentropy_items(Filter::new().until(Timestamp::from(101)))
                .await
                .is_err()
        );
        adapter.max_inventory_items = 1;
        assert!(adapter.negentropy_items(filter).await.is_err());
    }

    #[tokio::test]
    async fn silent_loopback_relay_times_out_and_disconnects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut saw_neg_open = false;
            while let Some(message) = websocket.next().await {
                match message {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                        saw_neg_open |= text.contains("NEG-OPEN");
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            saw_neg_open
        });
        let dir = tempfile::tempdir().unwrap();
        let mut syncer = test_syncer(&dir);
        syncer.config.relays = vec![format!("ws://{address}")];
        syncer.config.protocol_timeout = Duration::from_secs(1);
        let stats = tokio::time::timeout(Duration::from_secs(3), syncer.sync_once(|_| Ok(true)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.relays_responded, 0);
        assert_eq!(stats.relays_errored, 1);
        assert!(
            stats.relay_results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("deadline")
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(3), peer)
                .await
                .unwrap()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn lifecycle_deadline_and_panic_drop_owned_work() {
        for phase in ["connect", "reconcile", "disconnect"] {
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = DropFlag(Arc::clone(&dropped));
            let result = bounded_relay(phase.to_string(), Duration::from_millis(10), async move {
                let _guard = guard;
                std::future::pending().await
            })
            .await;
            assert!(result.error.unwrap().contains("deadline"));
            assert!(dropped.load(Ordering::SeqCst));
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&dropped));
        let result = bounded_relay("panic".to_string(), Duration::from_secs(1), async move {
            let _guard = guard;
            panic!("injected relay panic");
        })
        .await;
        assert!(result.error.unwrap().contains("panicked"));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn healthy_event_is_admitted_before_silent_relay_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let syncer = test_syncer(&dir);
        let (tx, mut rx) = mpsc::channel(2);
        let silent_done = Arc::new(AtomicBool::new(false));
        let silent_guard = DropFlag(Arc::clone(&silent_done));
        let silent = bounded_relay(
            "silent".to_string(),
            Duration::from_millis(100),
            async move {
                let _guard = silent_guard;
                std::future::pending().await
            },
        )
        .boxed();
        let healthy = bounded_relay("healthy".to_string(), Duration::from_secs(1), async move {
            tx.send(
                EventBuilder::text_note("streamed")
                    .sign_with_keys(&Keys::generate())
                    .unwrap(),
            )
            .await
            .unwrap();
            Ok(1)
        })
        .boxed();
        let workers = stream::iter([silent, healthy]).buffer_unordered(2);
        let mut stats = SyncStats::default();
        syncer
            .drive_workers(
                workers,
                &mut rx,
                &mut syncer.stop_signal.subscribe(),
                &mut |_| {
                    assert!(!silent_done.load(Ordering::SeqCst));
                    Ok(true)
                },
                &mut stats,
            )
            .await
            .unwrap();
        assert_eq!(stats.events_received, 1);
        assert_eq!(stats.relays_responded, 1);
        assert_eq!(stats.relays_errored, 1);
        assert!(silent_done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_drops_workers_without_waiting_for_sender_closure() {
        let dir = tempfile::tempdir().unwrap();
        let syncer = test_syncer(&dir);
        let (_tx, mut rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&dropped));
        let worker = bounded_relay("silent".to_string(), Duration::from_secs(60), async move {
            let _guard = guard;
            std::future::pending().await
        });
        let mut stats = SyncStats::default();
        let mut stop = syncer.stop_signal.subscribe();
        let mut handler = |_: &Event| Ok(true);
        let work = syncer.drive_workers(
            stream::once(worker),
            &mut rx,
            &mut stop,
            &mut handler,
            &mut stats,
        );
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            syncer.stop();
        };
        let (result, ()) = tokio::join!(work, cancel);
        assert!(result.is_err());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn adapter_backpressure_waits_and_oversize_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut adapter, mut receiver) =
            SyncStateAdapter::new(Arc::new(SyncStateDb::open(dir.path()).unwrap()));
        let (tx, rx) = mpsc::channel(1);
        adapter.event_sender = tx;
        receiver.close();
        receiver = rx;
        let event = EventBuilder::text_note("valid")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        adapter.save_event(&event).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), adapter.save_event(&event))
                .await
                .is_err()
        );
        assert!(!adapter.admission_failed.load(Ordering::SeqCst));
        assert_eq!(receiver.len(), 1);
        receiver.recv().await.unwrap();
        // Two SDK-sized batches must flow through even a single-slot queue.
        let producer = async {
            for _ in 0..200 {
                adapter.save_event(&event).await.unwrap();
            }
        };
        let consumer = async {
            for _ in 0..200 {
                assert_eq!(receiver.recv().await.unwrap().id, event.id);
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(producer, consumer);
        })
        .await
        .unwrap();
        assert!(!adapter.admission_failed.load(Ordering::SeqCst));
        adapter.max_event_bytes = 1;
        assert!(adapter.save_event(&event).await.is_err());
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn admission_failure_cancels_workers_without_recording_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let syncer = test_syncer(&dir);
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(
            EventBuilder::text_note("not archived")
                .sign_with_keys(&Keys::generate())
                .unwrap(),
        )
        .await
        .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&dropped));
        let worker = bounded_relay("silent".to_string(), Duration::from_secs(60), async move {
            let _guard = guard;
            std::future::pending().await
        });
        let result = syncer
            .drive_workers(
                stream::once(worker),
                &mut rx,
                &mut syncer.stop_signal.subscribe(),
                &mut |_| Err(crate::Error::Segment("injected archive fault".to_string())),
                &mut SyncStats::default(),
            )
            .await;
        assert!(matches!(result, Err(crate::Error::Segment(_))));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(syncer.sync_state.get_items_since(0).unwrap().is_empty());
    }

    #[tokio::test]
    async fn periodic_abort_clears_running_and_can_start_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut syncer = test_syncer(&dir);
        syncer.config.relays.clear();
        let syncer = Arc::new(syncer);
        for _ in 0..2 {
            let worker = Arc::clone(&syncer);
            let handle = tokio::spawn(async move { worker.run_periodic(|_| Ok(true)).await });
            tokio::time::timeout(Duration::from_secs(1), async {
                while !syncer.is_running() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            handle.abort();
            assert!(handle.await.unwrap_err().is_cancelled());
            assert!(!syncer.is_running());
        }
    }

    #[tokio::test]
    async fn adapter_rejects_forgery_and_closed_admission_channel() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SyncStateDb::open(dir.path()).unwrap());
        let (adapter, mut receiver) = SyncStateAdapter::new(state);
        let event = EventBuilder::text_note("valid")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        let mut forged = event.clone();
        forged.content = "forged".to_string();
        for _ in 0..2 {
            assert!(adapter.save_event(&forged).await.is_err());
        }
        assert!(receiver.try_recv().is_err());
        adapter.save_event(&event).await.unwrap();
        assert_eq!(receiver.recv().await.unwrap().id, event.id);
        drop(receiver);
        assert!(adapter.save_event(&event).await.is_err());
    }

    #[test]
    fn test_config_defaults() {
        let config = NegentropySyncConfig::default();
        assert_eq!(
            config.relays,
            vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.divine.video".to_string(),
                "wss://nos.lol".to_string(),
            ]
        );
        assert_eq!(config.interval, Duration::from_secs(1800));
        assert_eq!(config.lookback, Duration::from_secs(14 * 24 * 3600));
    }
}
