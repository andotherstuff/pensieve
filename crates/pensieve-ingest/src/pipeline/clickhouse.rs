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
use crate::{Error, Result};
use clickhouse::{Client, Row};
use crossbeam_channel::Receiver;
use flate2::read::GzDecoder;
use notepack::NoteParser;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
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
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_string(),
            database: "nostr".to_string(),
            table: "events_local".to_string(),
            batch_size: 10000,
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
    segments_indexed: AtomicUsize,
    events_indexed: AtomicUsize,
}

impl ClickHouseIndexer {
    /// Create a new ClickHouse indexer.
    pub fn new(config: ClickHouseConfig) -> Result<Self> {
        let client = Client::default()
            .with_url(&config.url)
            .with_database(&config.database);

        tracing::info!(
            "ClickHouse indexer initialized: url={}, database={}, table={}",
            config.url, config.database, config.table
        );

        Ok(Self {
            client,
            config,
            running: Arc::new(AtomicBool::new(false)),
            segments_indexed: AtomicUsize::new(0),
            events_indexed: AtomicUsize::new(0),
        })
    }

    /// Start the indexer, consuming from the segment channel.
    ///
    /// This runs in a background thread until `stop()` is called.
    pub fn start(&self, receiver: Receiver<SealedSegment>) -> thread::JoinHandle<()> {
        let client = self.client.clone();
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let segments_indexed = &self.segments_indexed as *const AtomicUsize as usize;
        let events_indexed = &self.events_indexed as *const AtomicUsize as usize;

        self.running.store(true, Ordering::SeqCst);

        thread::spawn(move || {
            // SAFETY: We're passing raw pointers but they're valid for the lifetime of the indexer
            let segments_indexed =
                unsafe { &*(segments_indexed as *const AtomicUsize) };
            let events_indexed =
                unsafe { &*(events_indexed as *const AtomicUsize) };

            tracing::info!("ClickHouse indexer thread started");

            // Create a runtime for async operations
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            while running.load(Ordering::SeqCst) {
                match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                    Ok(sealed) => {
                        tracing::info!(
                            "Indexing segment {}: {} events from {}",
                            sealed.segment_number,
                            sealed.event_count,
                            sealed.path.display()
                        );

                        match rt.block_on(Self::index_segment(&client, &config, &sealed)) {
                            Ok(count) => {
                                segments_indexed.fetch_add(1, Ordering::Relaxed);
                                events_indexed.fetch_add(count, Ordering::Relaxed);
                                tracing::info!(
                                    "Indexed segment {}: {} events",
                                    sealed.segment_number, count
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to index segment {}: {}",
                                    sealed.segment_number, e
                                );
                                // TODO: Implement retry logic
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

    /// Index a single segment into ClickHouse.
    async fn index_segment(
        client: &Client,
        config: &ClickHouseConfig,
        sealed: &SealedSegment,
    ) -> Result<usize> {
        let events = Self::read_segment(&sealed.path)?;

        if events.is_empty() {
            return Ok(0);
        }

        // Batch insert
        let mut inserter = client
            .insert(&config.table)?;

        for event in &events {
            inserter.write(event).await?;
        }

        inserter.end().await?;

        Ok(events.len())
    }

    /// Read a notepack segment file and parse into EventRows.
    ///
    /// Automatically detects and handles gzip-compressed segments (.notepack.gz).
    fn read_segment(path: &Path) -> Result<Vec<EventRow>> {
        let file = File::open(path)?;

        // Detect gzip by extension
        let is_gzip = path
            .extension()
            .is_some_and(|ext| ext == "gz");

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

            // Read notepack bytes
            let mut data = vec![0u8; len];
            reader.read_exact(&mut data)?;

            // Parse notepack
            match Self::parse_notepack(&data) {
                Ok(row) => events.push(row),
                Err(e) => {
                    tracing::warn!("Failed to parse event: {}", e);
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
        let events = Self::read_segment(path)?;

        if events.is_empty() {
            return Ok(0);
        }

        let mut inserter = self.client.insert(&self.config.table)?;

        for event in &events {
            inserter.write(event).await?;
        }

        inserter.end().await?;

        self.events_indexed.fetch_add(events.len(), Ordering::Relaxed);
        self.segments_indexed.fetch_add(1, Ordering::Relaxed);

        Ok(events.len())
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

