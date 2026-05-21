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
//! - Tracks checkpoints for crash recovery
//!
//! # Checkpoint Strategy
//!
//! Checkpoints are per-segment: when a segment is fully indexed, we record
//! "segment N fully processed". On restart, we query ClickHouse to reconcile
//! any partially-indexed segment.

use super::segment::SealedSegment;
use crate::logging::compact_error;
use crate::{Error, Result};
use clickhouse::{Client, Row};
use crossbeam_channel::Receiver;
use flate2::read::GzDecoder;
use metrics::counter;
use notepack::NoteParser;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

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

    /// Optional path to a newline-delimited file of segment paths that failed to
    /// index. Failed segments are appended here and re-indexed on the next start,
    /// so ClickHouse self-heals from the canonical archive.
    pub reindex_queue_path: Option<PathBuf>,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_string(),
            database: "nostr".to_string(),
            table: "events_local".to_string(),
            batch_size: 10000,
            reindex_queue_path: None,
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

/// ClickHouse indexer that consumes sealed segments.
pub struct ClickHouseIndexer {
    client: Client,
    config: ClickHouseConfig,
    running: Arc<AtomicBool>,
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
            segments_indexed: Arc::new(AtomicUsize::new(0)),
            events_indexed: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Start the indexer, consuming from the segment channel.
    ///
    /// This runs in a background thread until `stop()` is called.
    pub fn start(&self, receiver: Receiver<SealedSegment>) -> thread::JoinHandle<()> {
        let client = self.client.clone();
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let segments_indexed = Arc::clone(&self.segments_indexed);
        let events_indexed = Arc::clone(&self.events_indexed);

        self.running.store(true, Ordering::SeqCst);

        thread::spawn(move || {
            tracing::info!("ClickHouse indexer thread started");

            // Create a runtime for async operations
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            // Re-index any segments that failed in a previous run before consuming
            // new ones, so ClickHouse catches up with the archive.
            Self::drain_reindex_queue(&client, &config, &rt, &segments_indexed, &events_indexed);

            while running.load(Ordering::SeqCst) {
                match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                    Ok(sealed) => {
                        tracing::info!(
                            "Indexing segment {}: {} events from {}",
                            sealed.segment_number,
                            sealed.event_count,
                            sealed.path.display()
                        );

                        match Self::index_with_retry(&client, &config, &rt, &sealed) {
                            Ok(count) => {
                                segments_indexed.fetch_add(1, Ordering::Relaxed);
                                events_indexed.fetch_add(count, Ordering::Relaxed);
                                tracing::info!(
                                    "Indexed segment {}: {} events",
                                    sealed.segment_number,
                                    count
                                );
                            }
                            Err(e) => {
                                // Retries exhausted: record the failure and queue the
                                // segment for later reindex (the archive is canonical).
                                counter!("clickhouse_insert_errors_total").increment(1);
                                tracing::error!(
                                    segment = sealed.segment_number,
                                    error = %compact_error(&e),
                                    "failed to index segment after retries; queued for reindex"
                                );
                                Self::enqueue_failed(&config, &sealed.path);
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Continue waiting
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        tracing::info!("Segment channel disconnected, stopping indexer");
                        break;
                    }
                }
            }

            tracing::info!("ClickHouse indexer thread stopped");
        })
    }

    /// Stop the indexer.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Read a segment file and insert its events into ClickHouse (in batches).
    async fn index_path(client: &Client, config: &ClickHouseConfig, path: &Path) -> Result<usize> {
        let events = Self::read_segment(path)?;
        Self::insert_events(client, config, &events).await
    }

    /// Insert events in `batch_size`-sized chunks so a single insert request never
    /// holds an entire 256 MB segment, and a transient failure only affects one
    /// chunk (the whole segment is re-indexed idempotently via ReplacingMergeTree).
    async fn insert_events(
        client: &Client,
        config: &ClickHouseConfig,
        events: &[EventRow],
    ) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        let batch = config.batch_size.max(1);
        for chunk in events.chunks(batch) {
            let mut inserter = client.insert(&config.table)?;
            for event in chunk {
                inserter.write(event).await?;
            }
            inserter.end().await?;
        }
        Ok(events.len())
    }

    /// Index a sealed segment with bounded retries + exponential backoff.
    fn index_with_retry(
        client: &Client,
        config: &ClickHouseConfig,
        rt: &tokio::runtime::Runtime,
        sealed: &SealedSegment,
    ) -> Result<usize> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        loop {
            match rt.block_on(Self::index_path(client, config, &sealed.path)) {
                Ok(count) => return Ok(count),
                Err(e) => {
                    attempt += 1;
                    if attempt >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                    let backoff = std::time::Duration::from_secs(2u64.pow(attempt));
                    tracing::warn!(
                        segment = sealed.segment_number,
                        attempt,
                        error = %compact_error(&e),
                        "clickhouse index attempt failed; retrying in {:?}",
                        backoff
                    );
                    std::thread::sleep(backoff);
                }
            }
        }
    }

    /// Append a failed segment's path to the reindex queue (best-effort).
    fn enqueue_failed(config: &ClickHouseConfig, path: &Path) {
        let Some(queue) = &config.reindex_queue_path else {
            return;
        };
        use std::io::Write;
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(queue)
            .and_then(|mut f| writeln!(f, "{}", path.display()));
        match result {
            Ok(()) => tracing::info!(segment = %path.display(), "queued segment for reindex"),
            Err(e) => {
                tracing::warn!(error = %e, queue = %queue.display(), "failed to enqueue segment for reindex")
            }
        }
    }

    /// Re-index every segment listed in the reindex queue, rewriting the queue with
    /// the ones that still fail. Runs once at indexer start.
    fn drain_reindex_queue(
        client: &Client,
        config: &ClickHouseConfig,
        rt: &tokio::runtime::Runtime,
        segments_indexed: &AtomicUsize,
        events_indexed: &AtomicUsize,
    ) {
        let Some(queue) = &config.reindex_queue_path else {
            return;
        };
        if !queue.exists() {
            return;
        }
        let contents = match std::fs::read_to_string(queue) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, queue = %queue.display(), "failed to read reindex queue");
                return;
            }
        };
        let mut paths: Vec<String> = contents
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            let _ = std::fs::remove_file(queue);
            return;
        }

        tracing::info!(count = paths.len(), "draining clickhouse reindex queue");
        let mut still_failing: Vec<String> = Vec::new();
        for p in paths {
            let path = Path::new(&p);
            if !path.exists() {
                tracing::warn!(segment = %p, "queued segment no longer exists; dropping from queue");
                continue;
            }
            match rt.block_on(Self::index_path(client, config, path)) {
                Ok(count) => {
                    segments_indexed.fetch_add(1, Ordering::Relaxed);
                    events_indexed.fetch_add(count, Ordering::Relaxed);
                    tracing::info!(segment = %p, count, "reindexed queued segment");
                }
                Err(e) => {
                    tracing::error!(segment = %p, error = %compact_error(&e), "reindex still failing");
                    still_failing.push(p);
                }
            }
        }

        let write_result = if still_failing.is_empty() {
            std::fs::remove_file(queue).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            })
        } else {
            std::fs::write(queue, format!("{}\n", still_failing.join("\n")))
        };
        if let Err(e) = write_result {
            tracing::warn!(error = %e, queue = %queue.display(), "failed to update reindex queue");
        }
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
        use notepack::StringType;

        let parser = NoteParser::new(data);
        let note = parser
            .into_note()
            .map_err(|e| Error::Serialization(e.to_string()))?;

        // Convert tags to Vec<Vec<String>>
        let mut tags: Vec<Vec<String>> = Vec::new();
        let mut tags_iter = note.tags;
        while let Some(tag_result) = tags_iter
            .next_tag()
            .map_err(|e| Error::Serialization(e.to_string()))?
        {
            let mut tag_vec: Vec<String> = Vec::new();
            for elem in tag_result {
                let elem = elem.map_err(|e| Error::Serialization(e.to_string()))?;
                let s = match elem {
                    StringType::Str(s) => s.to_string(),
                    StringType::Bytes(b) => hex::encode(b),
                };
                tag_vec.push(s);
            }
            tags.push(tag_vec);
        }

        Ok(EventRow {
            id: hex::encode(note.id),
            pubkey: hex::encode(note.pubkey),
            created_at: note.created_at as u32,
            kind: note.kind as u16,
            content: note.content.to_string(),
            sig: hex::encode(note.sig),
            tags,
            relay_source: String::new(), // Backfill doesn't have relay source
        })
    }

    /// Index a segment file directly (for batch operations without channel).
    pub async fn index_segment_file(&self, path: &Path) -> Result<usize> {
        let count = Self::index_path(&self.client, &self.config, path).await?;
        self.events_indexed.fetch_add(count, Ordering::Relaxed);
        self.segments_indexed.fetch_add(1, Ordering::Relaxed);
        Ok(count)
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

    #[test]
    fn test_config_default() {
        let config = ClickHouseConfig::default();
        assert_eq!(config.database, "nostr");
        assert_eq!(config.table, "events_local");
    }

    // Integration tests would require a running ClickHouse instance
}
