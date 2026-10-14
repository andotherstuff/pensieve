//! ClickHouse indexer for Nostr events.
//!
//! This module provides the [`ClickHouseIndexer`] which reads sealed segments
//! and inserts events into ClickHouse.
//!
//! # Architecture
//!
//! The indexer is a derived consumer of the notepack archive:
//! - It receives notifications of sealed segments
//! - Reads each segment and batch-inserts events
//! - Records per-batch progress in an [`IndexQueue`] for crash recovery
//!
//! # Recovery
//!
//! When an [`IndexQueue`] is configured, the segment writer journals every
//! segment as it is sealed, and the indexer records its offset before each
//! insert batch, removing the entry once the segment is fully indexed. On
//! start, the indexer first resumes every segment left in the journal by a
//! previous run (killed, timed out, or stopped early).
//!
//! Downstream materialized views on `events_local` are not duplicate-safe, so
//! recovery never re-inserts a committed row: batches before the recorded
//! offset are skipped, and the batch that may have been in flight is checked
//! against `events_local` so only missing IDs are inserted. Retries within a
//! run use the same rule.
//!
//! # ClickHouse outages
//!
//! With an [`IndexQueue`], the indexer never gives up on a segment while it
//! runs: a failed attempt is retried with capped exponential backoff
//! ([`IndexRetryPolicy`], 2 s doubling to 60 s by default) until ClickHouse
//! accepts it. A ClickHouse restart, a reboot that starts the ingester first,
//! or an overload therefore heals without restarting the ingester.
//!
//! Segments are indexed one at a time in seal order. Seal notifications that
//! arrive while a segment is being retried are moved off the channel into a
//! small in-memory backlog (path and event count only, ordered by segment
//! number) and indexed after it. The writer never waits on the indexer: the
//! channel is unbounded and drained continuously, and every segment is
//! journaled at seal time, so archive admission does not depend on ClickHouse.
//!
//! A segment that cannot be *read* is not a ClickHouse problem: it stays in
//! the journal for the next start and the indexer moves on. Without an index
//! queue (the backfill tools) a segment is given up after
//! [`UNJOURNALED_ATTEMPTS`] attempts, since nothing would remember it.
//!
//! A shutdown request stops the indexer at the next batch boundary, or wakes
//! it from a backoff sleep within 200 ms, and leaves the rest of the segment
//! (and any segments still queued) in the journal.
//!
//! # Metrics
//!
//! - `clickhouse_index_queue_segments`: segments in the journal.
//! - `clickhouse_index_oldest_pending_seconds`: how long the oldest of them
//!   has waited (0 when empty), refreshed every few seconds.
//! - `clickhouse_index_retries_total`: failed attempts that were retried.
//! - `clickhouse_insert_errors_total`: segments given up for this run
//!   (unreadable, or no queue).

use super::index_queue::{IndexProgress, IndexQueue, resolve_segment};
use super::segment::SealedSegment;
use crate::logging::compact_error;
use crate::{Error, Result};
use clickhouse::{Client, Row};
use crossbeam_channel::Receiver;
use flate2::read::GzDecoder;
use metrics::counter;
use pensieve_parquet::CanonicalEvent;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// IDs per existence-check query. 1,000 hex IDs keep the query text (~68 KB)
/// well under ClickHouse's default 256 KiB `max_query_size`.
const EXISTS_QUERY_IDS: usize = 1_000;

/// Attempts per segment before an indexer without an [`IndexQueue`] gives it
/// up. With a queue there is no limit.
pub const UNJOURNALED_ATTEMPTS: u32 = 3;

/// Once the backoff is capped, log a failed attempt at WARN at most this often.
const RETRY_WARN_INTERVAL: Duration = Duration::from_secs(600);

/// How often the backlog gauges are refreshed while the indexer runs.
const METRICS_INTERVAL: Duration = Duration::from_secs(5);

/// Backoff between attempts to index a segment.
///
/// The first retry waits `initial_backoff`; each further consecutive failure
/// doubles the wait, up to `max_backoff`. A failed attempt that still
/// committed batches starts over at `initial_backoff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRetryPolicy {
    /// Wait after the first failure.
    pub initial_backoff: Duration,
    /// Longest wait between attempts.
    pub max_backoff: Duration,
}

impl Default for IndexRetryPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(60),
        }
    }
}

impl IndexRetryPolicy {
    /// Wait after `failures` consecutive failed attempts (1 for the first).
    pub fn backoff(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(31);
        self.initial_backoff
            .saturating_mul(1u32 << doublings)
            .min(self.max_backoff)
    }
}

/// Configuration for the ClickHouse indexer.
#[derive(Debug, Clone)]
pub struct ClickHouseConfig {
    /// ClickHouse server URL (e.g., "http://localhost:8123")
    pub url: String,

    /// Database name
    pub database: String,

    /// Table name for events
    pub table: String,

    /// Batch size for inserts
    pub batch_size: usize,

    /// Optional durable journal of segments pending index. When set, the
    /// indexer records per-batch progress there, resumes unfinished segments
    /// on start, and may stop mid-segment on shutdown. The segment writer
    /// should share the same queue (see `SegmentWriter::with_index_queue`) so
    /// segments are journaled at seal time. With a queue, failed segments are
    /// retried until they succeed or shutdown is requested.
    pub index_queue: Option<Arc<IndexQueue>>,

    /// Backoff between attempts to index a segment.
    pub retry: IndexRetryPolicy,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_string(),
            database: "nostr".to_string(),
            table: "events_local".to_string(),
            batch_size: 10000,
            index_queue: None,
            retry: IndexRetryPolicy::default(),
        }
    }
}

/// Row structure matching the ClickHouse events_local table.
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct EventRow {
    pub id: String,
    pub pubkey: String,
    pub created_at: u32, // DateTime is stored as Unix timestamp
    pub kind: u16,
    pub content: String,
    pub sig: String,
    pub tags: Vec<Vec<String>>,
    pub relay_source: String,
}

/// Result of indexing one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOutcome {
    /// Every event is in ClickHouse; `inserted` rows were written by this call
    /// and `already_present` were skipped because ClickHouse had them.
    Complete {
        /// Rows inserted.
        inserted: usize,
        /// Rows skipped because their ID was already in the table.
        already_present: usize,
    },
    /// Shutdown was requested; progress is recorded in the index queue.
    Interrupted,
}

/// Handle to a running indexer thread.
pub struct ClickHouseIndexerHandle {
    thread: thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl ClickHouseIndexerHandle {
    /// Ask the indexer to stop. With an index queue it stops at the next batch
    /// boundary (or wakes from a retry backoff) and leaves unfinished work
    /// journaled; without one it finishes the current segment first.
    pub fn request_shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether the indexer thread has exited.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Wait for the indexer thread to exit.
    pub fn join(self) -> thread::Result<()> {
        self.thread.join()
    }
}

/// ClickHouse indexer that consumes sealed segments.
pub struct ClickHouseIndexer {
    client: Client,
    config: ClickHouseConfig,
    running: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    segments_indexed: Arc<AtomicUsize>,
    events_indexed: Arc<AtomicUsize>,
    retries: Arc<AtomicUsize>,
}

/// State owned by the indexer thread.
struct Worker {
    client: Client,
    config: ClickHouseConfig,
    rt: tokio::runtime::Runtime,
    stop: Arc<AtomicBool>,
    segments_indexed: Arc<AtomicUsize>,
    events_indexed: Arc<AtomicUsize>,
    retries: Arc<AtomicUsize>,
    receiver: Receiver<SealedSegment>,
    /// Segments handed over by the writer and not indexed yet, by segment
    /// number (seal order).
    backlog: RefCell<BTreeMap<u64, Pending>>,
    /// Set from the first failed attempt until the backlog is empty again.
    outage: Cell<Option<Outage>>,
}

impl ClickHouseIndexer {
    /// Create a new ClickHouse indexer.
    pub fn new(config: ClickHouseConfig) -> Result<Self> {
        let client = Client::default()
            .with_url(&config.url)
            .with_database(&config.database);

        tracing::info!(
            "ClickHouse indexer initialized: url={}, database={}, table={}",
            config.url,
            config.database,
            config.table
        );

        Ok(Self {
            client,
            config,
            running: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            segments_indexed: Arc::new(AtomicUsize::new(0)),
            events_indexed: Arc::new(AtomicUsize::new(0)),
            retries: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Start the indexer, consuming from the segment channel.
    ///
    /// This runs in a background thread until the channel disconnects or a
    /// shutdown is requested through the returned handle (or `stop()`).
    pub fn start(&self, receiver: Receiver<SealedSegment>) -> ClickHouseIndexerHandle {
        let client = self.client.clone();
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let stop = Arc::clone(&self.stop);
        let segments_indexed = Arc::clone(&self.segments_indexed);
        let events_indexed = Arc::clone(&self.events_indexed);
        let retries = Arc::clone(&self.retries);

        self.running.store(true, Ordering::SeqCst);
        if let Some(queue) = &config.index_queue {
            Self::start_metrics_thread(Arc::clone(queue), Arc::clone(&self.running));
        }

        let thread = thread::spawn(move || {
            tracing::info!("ClickHouse indexer thread started");

            // Create a runtime for async operations
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            let worker = Worker {
                client,
                config,
                rt,
                stop,
                segments_indexed,
                events_indexed,
                retries,
                receiver,
                backlog: RefCell::new(BTreeMap::new()),
                outage: Cell::new(None),
            };
            worker.run();
            running.store(false, Ordering::SeqCst);
            tracing::info!("ClickHouse indexer thread stopped");
        });

        ClickHouseIndexerHandle {
            thread,
            stop: Arc::clone(&self.stop),
        }
    }

    /// Keep the backlog gauges current while the indexer runs. A separate
    /// thread, so the age keeps growing even while an insert hangs.
    fn start_metrics_thread(queue: Arc<IndexQueue>, running: Arc<AtomicBool>) {
        let spawned = thread::Builder::new()
            .name("clickhouse-index-metrics".to_string())
            .spawn(move || {
                while running.load(Ordering::SeqCst) {
                    queue.publish_metrics();
                    let deadline = Instant::now() + METRICS_INTERVAL;
                    while running.load(Ordering::SeqCst) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            });
        if let Err(e) = spawned {
            // The gauges are still updated whenever the journal changes.
            tracing::warn!(error = %e, "failed to start clickhouse index metrics thread");
        }
    }

    /// Request the indexer to stop (see [`ClickHouseIndexerHandle::request_shutdown`]).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Insert `events`, resuming from `progress`.
    ///
    /// Batches before `progress.committed` are skipped; batches overlapping
    /// `progress.committed..progress.verify_end` insert only IDs missing from
    /// the table. When `queue` is set, progress is made durable before every
    /// batch, and a set `stop` flag ends the call at a batch boundary.
    /// `progress` is updated in place, so a failed call can be retried with it.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_resumable(
        client: &Client,
        config: &ClickHouseConfig,
        queue: Option<&IndexQueue>,
        stop: &AtomicBool,
        segment: &Path,
        events: &[EventRow],
        progress: &mut IndexProgress,
    ) -> Result<IndexOutcome> {
        let batch = config.batch_size.max(1);
        let mut inserted = 0;
        let mut already_present = 0;
        while progress.committed < events.len() {
            if let Some(queue) = queue
                && stop.load(Ordering::SeqCst)
            {
                queue.record_progress(segment, *progress)?;
                return Ok(IndexOutcome::Interrupted);
            }
            let start = progress.committed;
            let end = start.saturating_add(batch).min(events.len());
            let chunk = &events[start..end];
            let verify = progress.needs_verify(start);

            // Durable before the insert: if we die mid-batch, recovery knows
            // this batch is the only one whose outcome is unknown.
            progress.verify_end = progress.verify_end.max(end);
            if let Some(queue) = queue {
                queue.record_progress(segment, *progress)?;
            }

            let rows: Vec<&EventRow> = if verify {
                let ids: Vec<&str> = chunk.iter().map(|row| row.id.as_str()).collect();
                let present = Self::existing_ids(client, &config.table, &ids).await?;
                chunk
                    .iter()
                    .filter(|row| !present.contains(row.id.as_str()))
                    .collect()
            } else {
                chunk.iter().collect()
            };
            let skipped = chunk.len() - rows.len();

            if !rows.is_empty() {
                let mut inserter = client.insert(&config.table)?;
                for row in &rows {
                    inserter.write(*row).await?;
                }
                inserter.end().await?;
            }
            progress.committed = end;
            inserted += rows.len();
            already_present += skipped;
            if skipped > 0 {
                counter!("clickhouse_index_rows_already_present_total").increment(skipped as u64);
            }
        }
        Ok(IndexOutcome::Complete {
            inserted,
            already_present,
        })
    }

    /// IDs from `ids` that are already in `table`.
    pub async fn existing_ids(
        client: &Client,
        table: &str,
        ids: &[&str],
    ) -> Result<HashSet<String>> {
        let mut present = HashSet::new();
        for chunk in ids.chunks(EXISTS_QUERY_IDS) {
            let mut list = String::with_capacity(chunk.len() * 67);
            for (i, id) in chunk.iter().enumerate() {
                // IDs come from hex::encode, so this never fires; it keeps the
                // hand-built literal list safe regardless.
                if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(Error::Serialization(format!("non-hex event id: {id:?}")));
                }
                if i > 0 {
                    list.push(',');
                }
                list.push('\'');
                list.push_str(id);
                list.push('\'');
            }
            let query = format!("SELECT id FROM {table} WHERE id IN ({list})");
            present.extend(client.query(&query).fetch_all::<String>().await?);
        }
        Ok(present)
    }

    /// Read a notepack segment file and parse into EventRows.
    ///
    /// Automatically detects and handles gzip-compressed segments (.notepack.gz).
    fn read_segment(path: &Path) -> Result<Vec<EventRow>> {
        let file = File::open(path)?;

        // Detect gzip by extension
        let is_gzip = path.extension().is_some_and(|ext| ext == "gz");

        let mut reader: Box<dyn Read> = if is_gzip {
            Box::new(BufReader::new(GzDecoder::new(BufReader::new(file))))
        } else {
            Box::new(BufReader::new(file))
        };

        let mut events = Vec::new();

        loop {
            // Read length prefix (u32 little-endian)
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            let len = u32::from_le_bytes(len_buf) as usize;

            // Guard against a torn/corrupt frame (e.g. a crash mid-write that left a
            // partial length prefix): a single packed event is never this large, so
            // treat an absurd length as end-of-data instead of allocating gigabytes.
            const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
            if len > MAX_EVENT_BYTES {
                tracing::warn!(
                    len,
                    "segment frame length exceeds sane maximum; stopping read (likely torn segment)"
                );
                break;
            }

            // Read notepack bytes
            let mut data = vec![0u8; len];
            reader.read_exact(&mut data)?;

            // Parse notepack
            match Self::parse_notepack(&data) {
                Ok(row) => events.push(row),
                Err(e) => {
                    tracing::warn!(error = %compact_error(&e), "failed to parse archived event for clickhouse");
                    // Continue with next event
                }
            }
        }

        Ok(events)
    }

    /// Parse notepack bytes into an EventRow.
    fn parse_notepack(data: &[u8]) -> Result<EventRow> {
        let event =
            CanonicalEvent::from_notepack(data).map_err(|e| Error::Serialization(e.to_string()))?;

        Ok(EventRow {
            id: hex::encode(event.id()),
            pubkey: hex::encode(event.pubkey()),
            created_at: event.created_at() as u32,
            kind: event.kind(),
            content: event.content().to_owned(),
            sig: hex::encode(event.signature()),
            tags: event.tags().to_owned(),
            relay_source: String::new(), // Backfill doesn't have relay source
        })
    }

    /// Index a segment file directly (for batch operations without channel).
    ///
    /// Inserts every event in the file; there is no journal or existence check.
    pub async fn index_segment_file(&self, path: &Path) -> Result<usize> {
        let events = Self::read_segment(path)?;
        let mut progress = IndexProgress::default();
        let outcome = Self::insert_resumable(
            &self.client,
            &self.config,
            None,
            &AtomicBool::new(false),
            path,
            &events,
            &mut progress,
        )
        .await?;
        let IndexOutcome::Complete { inserted, .. } = outcome else {
            unreachable!("insert without a queue is never interrupted");
        };
        self.events_indexed.fetch_add(inserted, Ordering::Relaxed);
        self.segments_indexed.fetch_add(1, Ordering::Relaxed);
        Ok(inserted)
    }

    /// Get statistics about the indexer.
    pub fn stats(&self) -> IndexerStats {
        IndexerStats {
            segments_indexed: self.segments_indexed.load(Ordering::Relaxed),
            events_indexed: self.events_indexed.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            is_running: self.running.load(Ordering::Relaxed),
        }
    }

    /// Check if ClickHouse is reachable.
    pub async fn health_check(&self) -> Result<bool> {
        let result: u8 = self.client.query("SELECT 1").fetch_one().await?;
        Ok(result == 1)
    }

    /// Get the count of events in the table.
    pub async fn event_count(&self) -> Result<u64> {
        let query = format!("SELECT count() FROM {}", self.config.table);
        let count: u64 = self.client.query(&query).fetch_one().await?;
        Ok(count)
    }
}

impl Worker {
    fn queue(&self) -> Option<&IndexQueue> {
        self.config.index_queue.as_deref()
    }

    fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn run(&self) {
        // Finish what a previous run left behind before consuming new
        // segments, so ClickHouse catches up with the archive in order.
        if !self.resume_previous_run() && !self.consume_backlog() {
            return;
        }

        // Stopping early: make sure every segment handed over by the writer
        // but not indexed yet is journaled (the writer normally did this at
        // seal time already).
        let Some(queue) = self.queue() else {
            return;
        };
        self.absorb_notifications();
        for pending in self.backlog.borrow().values() {
            if let Err(e) = queue.record_sealed(&pending.path) {
                tracing::error!(
                    segment = %pending.path.display(),
                    error = %compact_error(&e),
                    "failed to journal unindexed segment at shutdown"
                );
            }
        }
        if !queue.is_empty() {
            tracing::info!(
                pending = queue.len(),
                queue = %queue.path().display(),
                "clickhouse indexer stopped early; pending segments resume on next start"
            );
        }
    }

    /// Index segments handed over by the writer, oldest first, until the
    /// channel disconnects (returns false) or a shutdown request stops the
    /// indexer (returns true).
    fn consume_backlog(&self) -> bool {
        loop {
            if self.stop_requested() {
                return true;
            }
            self.absorb_notifications();
            let next = self.backlog.borrow_mut().pop_first();
            if let Some((number, pending)) = next {
                tracing::info!(
                    "Indexing segment {}: {} events from {}",
                    number,
                    pending.event_count,
                    pending.path.display()
                );
                let progress = self
                    .queue()
                    .and_then(|queue| queue.progress(&pending.path))
                    .unwrap_or_default();
                if self.index_segment(&pending.path, progress) == Step::Interrupted {
                    return true;
                }
                continue;
            }
            self.note_backlog_cleared();
            match self.receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(sealed) => self.accept(sealed),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    tracing::info!("Segment channel disconnected, stopping indexer");
                    return false;
                }
            }
        }
    }

    /// Move every notification waiting on the channel into the backlog, so
    /// the channel stays empty while a segment is being retried.
    fn absorb_notifications(&self) {
        while let Ok(sealed) = self.receiver.try_recv() {
            self.accept(sealed);
        }
    }

    /// Keep only what indexing needs: the notification also carries every
    /// event ID of the segment, which a long outage must not pile up.
    fn accept(&self, sealed: SealedSegment) {
        self.backlog.borrow_mut().insert(
            sealed.segment_number,
            Pending {
                path: sealed.path,
                event_count: sealed.event_count,
            },
        );
    }

    /// Resume segments journaled by a previous run. Returns true if a shutdown
    /// request interrupted it.
    fn resume_previous_run(&self) -> bool {
        let Some(queue) = self.queue() else {
            return false;
        };
        let pending = queue.previous_run();
        if pending.is_empty() {
            return false;
        }
        tracing::info!(
            count = pending.len(),
            "resuming segments left unindexed by a previous run"
        );
        for (path, progress) in pending {
            if self.stop_requested() {
                return true;
            }
            let Some(resolved) = resolve_segment(&path) else {
                tracing::warn!(segment = %path.display(), "queued segment no longer exists; dropping from queue");
                if let Err(e) = queue.complete(&path) {
                    tracing::warn!(error = %compact_error(&e), "failed to update clickhouse index queue");
                }
                continue;
            };
            tracing::info!(
                segment = %resolved.display(),
                committed = progress.committed,
                verify_end = progress.verify_end,
                "resuming queued segment"
            );
            if self.index_segment(&resolved, progress) == Step::Interrupted {
                return true;
            }
        }
        false
    }

    /// Index one segment, keeping the journal in step.
    ///
    /// With an index queue a failed attempt is retried, with capped
    /// exponential backoff, until it succeeds or shutdown is requested; later
    /// segments wait in the backlog meanwhile. Without one the segment is
    /// given up after [`UNJOURNALED_ATTEMPTS`].
    fn index_segment(&self, path: &Path, mut progress: IndexProgress) -> Step {
        let queue = self.queue();
        let events = match ClickHouseIndexer::read_segment(path) {
            Ok(events) => events,
            Err(e) => {
                // Not a ClickHouse problem: retrying in this run would not
                // help, and waiting on it would stall every later segment.
                counter!("clickhouse_insert_errors_total").increment(1);
                tracing::error!(segment = %path.display(), error = %compact_error(&e), "failed to read segment for clickhouse; left queued for next start");
                self.keep_queued(path, progress);
                return Step::Failed;
            }
        };

        let mut failures = 0;
        loop {
            let committed_before = progress.committed;
            let result = self.rt.block_on(ClickHouseIndexer::insert_resumable(
                &self.client,
                &self.config,
                queue,
                &self.stop,
                path,
                &events,
                &mut progress,
            ));
            match result {
                Ok(IndexOutcome::Complete {
                    inserted,
                    already_present,
                }) => {
                    self.segments_indexed.fetch_add(1, Ordering::Relaxed);
                    self.events_indexed.fetch_add(inserted, Ordering::Relaxed);
                    if let Some(queue) = queue
                        && let Err(e) = queue.complete(path)
                    {
                        // Harmless: a later resume finds every row present.
                        tracing::warn!(error = %compact_error(&e), "failed to remove indexed segment from queue");
                    }
                    tracing::info!(
                        segment = %path.display(),
                        inserted,
                        already_present,
                        "indexed segment"
                    );
                    self.note_recovered();
                    return Step::Done;
                }
                Ok(IndexOutcome::Interrupted) => {
                    tracing::info!(
                        segment = %path.display(),
                        committed = progress.committed,
                        total = events.len(),
                        "clickhouse indexing stopped mid-segment for shutdown; progress saved"
                    );
                    return Step::Interrupted;
                }
                Err(e) => {
                    // Batches went in before this failure, so the server is
                    // up: start the backoff over.
                    if progress.committed > committed_before {
                        failures = 0;
                    }
                    failures += 1;
                    if queue.is_none() && failures >= UNJOURNALED_ATTEMPTS {
                        counter!("clickhouse_insert_errors_total").increment(1);
                        tracing::error!(
                            segment = %path.display(),
                            error = %compact_error(&e),
                            "failed to index segment after retries; no index queue, so it is not retried"
                        );
                        return Step::Failed;
                    }
                    let backoff = self.config.retry.backoff(failures);
                    counter!("clickhouse_index_retries_total").increment(1);
                    self.retries.fetch_add(1, Ordering::Relaxed);
                    self.note_failure(path, failures, backoff, &e);
                    if self.sleep_unless_stopped(backoff) {
                        self.keep_queued(path, progress);
                        return Step::Interrupted;
                    }
                }
            }
        }
    }

    /// Log a failed attempt: one WARN per backoff step while the backoff
    /// grows, then at most one per [`RETRY_WARN_INTERVAL`] once it is capped.
    fn note_failure(&self, path: &Path, failures: u32, backoff: Duration, error: &Error) {
        let now = Instant::now();
        let mut outage = self.outage.get().unwrap_or(Outage {
            since: now,
            failures: 0,
            last_warn: None,
            recovered: false,
        });
        outage.failures += 1;
        outage.recovered = false;
        let capped = backoff >= self.config.retry.max_backoff;
        let warn = !capped
            || outage
                .last_warn
                .is_none_or(|last| now.duration_since(last) >= RETRY_WARN_INTERVAL);
        let pending = self.queue().map_or(0, IndexQueue::len);
        let outage_secs = now.duration_since(outage.since).as_secs();
        if warn {
            outage.last_warn = Some(now);
            tracing::warn!(
                segment = %path.display(),
                attempt = failures,
                pending,
                outage_secs,
                error = %compact_error(error),
                "clickhouse index attempt failed; retrying in {:?}",
                backoff
            );
        } else {
            tracing::debug!(
                segment = %path.display(),
                attempt = failures,
                pending,
                outage_secs,
                error = %compact_error(error),
                "clickhouse index attempt failed; retrying in {:?}",
                backoff
            );
        }
        self.outage.set(Some(outage));
    }

    /// First success after failed attempts: say so once.
    fn note_recovered(&self) {
        let Some(mut outage) = self.outage.get() else {
            return;
        };
        if outage.recovered {
            return;
        }
        outage.recovered = true;
        self.outage.set(Some(outage));
        tracing::info!(
            failed_attempts = outage.failures,
            outage_secs = outage.since.elapsed().as_secs(),
            pending = self.queue().map_or(0, IndexQueue::len),
            "clickhouse indexing recovered; catching up on queued segments"
        );
    }

    /// Nothing left to index: close out a run of failures.
    fn note_backlog_cleared(&self) {
        if let Some(outage) = self.outage.take() {
            tracing::info!(
                failed_attempts = outage.failures,
                outage_secs = outage.since.elapsed().as_secs(),
                "clickhouse index backlog cleared"
            );
        }
    }

    /// Sleep, waking early for a shutdown request when a queue can hold the
    /// remaining work. Notifications that arrive meanwhile join the backlog.
    /// Returns true if interrupted.
    fn sleep_unless_stopped(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            self.absorb_notifications();
            if self.queue().is_some() && self.stop_requested() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            thread::sleep((deadline - now).min(Duration::from_millis(200)));
        }
    }

    fn keep_queued(&self, path: &Path, progress: IndexProgress) {
        if let Some(queue) = self.queue()
            && let Err(e) = queue.record_progress(path, progress)
        {
            tracing::error!(segment = %path.display(), error = %compact_error(&e), "failed to journal unindexed segment");
        }
    }
}

/// A sealed segment waiting in the indexer's in-memory backlog.
struct Pending {
    path: PathBuf,
    event_count: usize,
}

/// A run of failed index attempts, for logging.
#[derive(Debug, Clone, Copy)]
struct Outage {
    since: Instant,
    failures: u64,
    last_warn: Option<Instant>,
    recovered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Done,
    Failed,
    Interrupted,
}

/// Statistics about the ClickHouse indexer.
#[derive(Debug, Clone)]
pub struct IndexerStats {
    /// Number of segments indexed.
    pub segments_indexed: usize,

    /// Number of events indexed.
    pub events_indexed: usize,

    /// Failed index attempts that were retried.
    pub retries: usize,

    /// Whether the indexer is running.
    pub is_running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{EventBuilder, Keys, Kind, Timestamp};
    use notepack::NoteBinary;

    #[test]
    fn test_config_default() {
        let config = ClickHouseConfig::default();
        assert_eq!(config.database, "nostr");
        assert_eq!(config.table, "events_local");
    }

    #[test]
    fn clickhouse_parser_accepts_frame_bounded_large_content() {
        let keys = Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
            .expect("valid test secret key");
        let event = EventBuilder::new(Kind::TextNote, "x".repeat(700_000))
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&keys)
            .expect("large event should sign");
        let tags: Vec<Vec<String>> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
            .collect();
        let payload = NoteBinary {
            id: event.id.as_bytes(),
            pubkey: event.pubkey.as_bytes(),
            sig: event.sig.as_ref(),
            content: &event.content,
            created_at: event.created_at.as_secs(),
            kind: u64::from(event.kind.as_u16()),
            tags: &tags,
        }
        .pack();

        let row = ClickHouseIndexer::parse_notepack(&payload)
            .expect("frame-bounded content should decode");
        assert_eq!(row.id, event.id.to_hex());
        assert_eq!(row.content.len(), 700_000);
    }

    #[test]
    fn retry_backoff_doubles_up_to_the_cap() {
        let policy = IndexRetryPolicy::default();
        let waits: Vec<u64> = (1..=8).map(|n| policy.backoff(n).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(policy.backoff(u32::MAX), Duration::from_secs(60));
        assert_eq!(policy.backoff(0), Duration::from_secs(2));

        let fast = IndexRetryPolicy {
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(25),
        };
        assert_eq!(fast.backoff(2), Duration::from_millis(20));
        assert_eq!(fast.backoff(3), Duration::from_millis(25));
    }

    // Outage, ordering and shutdown behaviour is covered in
    // tests/clickhouse_index_recovery.rs.
}
