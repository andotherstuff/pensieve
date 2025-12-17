//! Deduplication index using RocksDB.
//!
//! This module provides the [`DedupeIndex`] which tracks which event IDs have
//! been seen/archived. It uses RocksDB for efficient disk-backed storage that
//! can handle billions of keys.
//!
//! # Key Design
//!
//! - Keys: 32-byte event IDs (raw bytes, not hex)
//! - Values: 1 byte status flag
//! - Bloom filters for fast "not seen" lookups
//! - Rebuildable from the archive if lost/corrupted

use crate::error::Result;
use rocksdb::{DBWithThreadMode, MultiThreaded, Options, WriteBatch, WriteOptions};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info};

/// Status flags stored as values in the dedupe index.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStatus {
    /// Event is being processed (written to segment but not yet sealed).
    Pending = 1,
    /// Event has been archived (segment sealed and uploaded).
    Archived = 2,
}

impl EventStatus {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Pending),
            2 => Some(Self::Archived),
            _ => None,
        }
    }

    fn to_byte(self) -> u8 {
        self as u8
    }
}

/// RocksDB-backed deduplication index for event IDs.
///
/// Thread-safe: can be shared across multiple threads via `Arc<DedupeIndex>`.
pub struct DedupeIndex {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
}

impl DedupeIndex {
    /// Open or create a dedupe index at the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory path for the RocksDB database
    ///
    /// # Example
    ///
    /// ```no_run
    /// use pensieve_ingest::DedupeIndex;
    ///
    /// let index = DedupeIndex::open("./data/dedupe")?;
    /// # Ok::<(), pensieve_ingest::Error>(())
    /// ```
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        info!("Opening dedupe index at {}", path.display());

        let mut opts = Options::default();
        opts.create_if_missing(true);

        // Optimize for write-heavy workload
        opts.set_write_buffer_size(64 * 1024 * 1024); // 64MB write buffer
        opts.set_max_write_buffer_number(3);
        opts.set_target_file_size_base(64 * 1024 * 1024); // 64MB SST files

        // Bloom filters for fast "not found" lookups
        // 10 bits per key = ~1% false positive rate
        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        block_opts.set_cache_index_and_filter_blocks(true);
        opts.set_block_based_table_factory(&block_opts);

        // Compression for disk space efficiency
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);

        // Parallelism
        opts.increase_parallelism(num_cpus::get() as i32);
        opts.set_max_background_jobs(4);

        let db = DBWithThreadMode::<MultiThreaded>::open(&opts, path)?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Check if an event ID has been seen.
    ///
    /// Returns `Some(status)` if the event exists, `None` if not seen.
    ///
    /// # Arguments
    ///
    /// * `event_id` - 32-byte event ID (raw bytes)
    pub fn get_status(&self, event_id: &[u8; 32]) -> Result<Option<EventStatus>> {
        match self.db.get(event_id)? {
            Some(value) => {
                if value.is_empty() {
                    Ok(Some(EventStatus::Archived)) // Legacy: empty value = archived
                } else {
                    Ok(EventStatus::from_byte(value[0]))
                }
            }
            None => Ok(None),
        }
    }

    /// Check if an event ID is new (not seen before).
    ///
    /// This is optimized for the common case where events are new.
    /// Uses bloom filters for fast rejection of seen events.
    pub fn is_new(&self, event_id: &[u8; 32]) -> Result<bool> {
        Ok(self.get_status(event_id)?.is_none())
    }

    /// Check and mark an event as pending in one operation.
    ///
    /// Returns `true` if the event is new (was not seen before),
    /// `false` if it was already seen.
    ///
    /// This is the main API for deduplication during ingestion.
    pub fn check_and_mark_pending(&self, event_id: &[u8; 32]) -> Result<bool> {
        // First check if it exists (uses bloom filter for fast path)
        if self.get_status(event_id)?.is_some() {
            return Ok(false);
        }

        // Mark as pending
        self.db.put(event_id, [EventStatus::Pending.to_byte()])?;
        Ok(true)
    }

    /// Mark multiple events as archived (batch operation).
    ///
    /// Called when a segment is sealed and uploaded.
    ///
    /// # Arguments
    ///
    /// * `event_ids` - Iterator of 32-byte event IDs
    pub fn mark_archived<'a, I>(&self, event_ids: I) -> Result<()>
    where
        I: Iterator<Item = &'a [u8; 32]>,
    {
        let mut batch = WriteBatch::default();
        let mut count = 0usize;

        for event_id in event_ids {
            batch.put(event_id, [EventStatus::Archived.to_byte()]);
            count += 1;
        }

        if count > 0 {
            let mut write_opts = WriteOptions::default();
            write_opts.set_sync(true); // Ensure durability
            self.db.write_opt(batch, &write_opts)?;
            debug!("Marked {} events as archived", count);
        }

        Ok(())
    }

    /// Get the approximate number of keys in the database.
    pub fn approximate_count(&self) -> Result<u64> {
        let count = self
            .db
            .property_int_value("rocksdb.estimate-num-keys")?
            .unwrap_or(0);
        Ok(count)
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }

    /// Get statistics about the database.
    pub fn stats(&self) -> DedupeStats {
        DedupeStats {
            approximate_keys: self.approximate_count().unwrap_or(0),
        }
    }
}

/// Statistics about the dedupe index.
#[derive(Debug, Clone)]
pub struct DedupeStats {
    /// Approximate number of keys in the database.
    pub approximate_keys: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_event_id(n: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = n;
        id
    }

    #[test]
    fn test_open_and_close() {
        let tmp = TempDir::new().unwrap();
        let _index = DedupeIndex::open(tmp.path()).unwrap();
    }

    #[test]
    fn test_check_and_mark() {
        let tmp = TempDir::new().unwrap();
        let index = DedupeIndex::open(tmp.path()).unwrap();

        let id1 = test_event_id(1);
        let id2 = test_event_id(2);

        // First time should return true (is new)
        assert!(index.check_and_mark_pending(&id1).unwrap());

        // Second time should return false (already seen)
        assert!(!index.check_and_mark_pending(&id1).unwrap());

        // Different ID should return true
        assert!(index.check_and_mark_pending(&id2).unwrap());

        // Check status
        assert_eq!(
            index.get_status(&id1).unwrap(),
            Some(EventStatus::Pending)
        );
    }

    #[test]
    fn test_mark_archived() {
        let tmp = TempDir::new().unwrap();
        let index = DedupeIndex::open(tmp.path()).unwrap();

        let id1 = test_event_id(1);
        let id2 = test_event_id(2);

        // Mark as pending
        index.check_and_mark_pending(&id1).unwrap();
        index.check_and_mark_pending(&id2).unwrap();

        // Mark as archived
        index.mark_archived([&id1, &id2].into_iter()).unwrap();

        // Check status
        assert_eq!(
            index.get_status(&id1).unwrap(),
            Some(EventStatus::Archived)
        );
        assert_eq!(
            index.get_status(&id2).unwrap(),
            Some(EventStatus::Archived)
        );
    }

    #[test]
    fn test_is_new() {
        let tmp = TempDir::new().unwrap();
        let index = DedupeIndex::open(tmp.path()).unwrap();

        let id1 = test_event_id(1);

        assert!(index.is_new(&id1).unwrap());
        index.check_and_mark_pending(&id1).unwrap();
        assert!(!index.is_new(&id1).unwrap());
    }
}

