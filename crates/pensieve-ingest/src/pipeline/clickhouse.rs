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
//! previous run (killed, timed out, stopped early, or failed after retries).
//!
//! Downstream materialized views on `events_local` are not duplicate-safe, so
//! recovery never re-inserts a committed row: batches before the recorded
//! offset are skipped, and the batch that may have been in flight is checked
//! against `events_local` so only missing IDs are inserted. Retries within a
//! run use the same rule.
//!
//! A shutdown request stops the indexer at the next batch boundary and leaves
//! the rest of the segment (and any segments still queued) in the journal.

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
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// IDs per existence-check query. 1,000 hex IDs keep the query text (~68 KB)
/// well under ClickHouse's default 256 KiB `max_query_size`.
const EXISTS_QUERY_IDS: usize = 1_000;

/// Attempts per segment before it is left in the journal for the next start.
const MAX_ATTEMPTS: u32 = 3;

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
    /// segments are journaled at seal time.
    pub index_queue: Option<Arc<IndexQueue>>,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_string(),
            database: "nostr".to_string(),
            table: "events_local".to_string(),
            batch_size: 10000,
            index_queue: None,
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
    /// boundary and leaves unfinished work journaled; without one it finishes
    /// the current segment first.
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
}

/// State shared by the indexer thread's helpers.
struct Worker {
    client: Client,
    config: ClickHouseConfig,
    rt: tokio::runtime::Runtime,
    stop: Arc<AtomicBool>,
    segments_indexed: Arc<AtomicUsize>,
    events_indexed: Arc<AtomicUsize>,
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

        self.running.store(true, Ordering::SeqCst);

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
            };
            worker.run(&receiver);
            running.store(false, Ordering::SeqCst);
            tracing::info!("ClickHouse indexer thread stopped");
        });

        ClickHouseIndexerHandle {
            thread,
            stop: Arc::clone(&self.stop),
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

    fn run(&self, receiver: &Receiver<SealedSegment>) {
        // Finish what a previous run left behind before consuming new
        // segments, so ClickHouse catches up with the archive in order.
        if !self.resume_previous_run() {
            loop {
                if self.stop_requested() {
                    break;
                }
                match receiver.recv_timeout(Duration::from_secs(1)) {
                    Ok(sealed) => {
                        tracing::info!(
                            "Indexing segment {}: {} events from {}",
                            sealed.segment_number,
                            sealed.event_count,
                            sealed.path.display()
                        );
                        let progress = self
                            .queue()
                            .and_then(|queue| queue.progress(&sealed.path))
                            .unwrap_or_default();
                        if self.index_segment(&sealed.path, progress) == Step::Interrupted {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        tracing::info!("Segment channel disconnected, stopping indexer");
                        return;
                    }
                }
            }
        }

        // Stopping early: make sure every segment still waiting on the channel
        // is journaled (the writer normally did this at seal time already).
        let Some(queue) = self.queue() else {
            return;
        };
        while let Ok(sealed) = receiver.try_recv() {
            if let Err(e) = queue.record_sealed(&sealed.path) {
                tracing::error!(
                    segment = sealed.segment_number,
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

    /// Index one segment with bounded retries, keeping the journal in step.
    fn index_segment(&self, path: &Path, mut progress: IndexProgress) -> Step {
        let queue = self.queue();
        let events = match ClickHouseIndexer::read_segment(path) {
            Ok(events) => events,
            Err(e) => {
                counter!("clickhouse_insert_errors_total").increment(1);
                tracing::error!(segment = %path.display(), error = %compact_error(&e), "failed to read segment for clickhouse; left queued");
                self.keep_queued(path, progress);
                return Step::Failed;
            }
        };

        let mut attempt = 0;
        loop {
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
                    attempt += 1;
                    if attempt >= MAX_ATTEMPTS {
                        counter!("clickhouse_insert_errors_total").increment(1);
                        tracing::error!(
                            segment = %path.display(),
                            error = %compact_error(&e),
                            "failed to index segment after retries; left queued for next start"
                        );
                        self.keep_queued(path, progress);
                        return Step::Failed;
                    }
                    let backoff = Duration::from_secs(2u64.pow(attempt));
                    tracing::warn!(
                        segment = %path.display(),
                        attempt,
                        error = %compact_error(&e),
                        "clickhouse index attempt failed; retrying in {:?}",
                        backoff
                    );
                    if self.sleep_unless_stopped(backoff) {
                        self.keep_queued(path, progress);
                        return Step::Interrupted;
                    }
                }
            }
        }
    }

    /// Sleep, waking early for a shutdown request when a queue can hold the
    /// remaining work. Returns true if interrupted.
    fn sleep_unless_stopped(&self, duration: Duration) -> bool {
        let deadline = std::time::Instant::now() + duration;
        loop {
            if self.queue().is_some() && self.stop_requested() {
                return true;
            }
            let now = std::time::Instant::now();
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

    // Integration tests would require a running ClickHouse instance
}
