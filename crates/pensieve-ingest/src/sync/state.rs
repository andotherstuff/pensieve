//! Sync state database for negentropy reconciliation.
//!
//! This module provides a RocksDB-backed database for tracking which events have
//! been synced via negentropy. The database stores (timestamp, event_id) pairs
//! to enable efficient time-range queries for negentropy reconciliation.
//!
//! # Key Design
//!
//! Keys are structured as `[timestamp_be (8 bytes)][event_id (32 bytes)]` where:
//! - `timestamp_be` is the event's `created_at` in big-endian format
//! - `event_id` is the 32-byte event ID
//!
//! Using big-endian timestamp as the leading bytes allows RocksDB to efficiently
//! scan all events >= a given timestamp using an ordered iterator.
//!
//! # Independence from Dedupe Index
//!
//! This database is **separate** from:
//! - **RocksDB dedupe index**: Event IDs only, for deduplication
//! - **Notepack segments**: Full event storage
//! - **ClickHouse**: Analytics
//!
//! The sync state can be rebuilt from ClickHouse if lost, and can be pruned
//! independently without affecting other systems.

use crate::Result;
use rocksdb::{DBWithThreadMode, IteratorMode, MultiThreaded, Options};
use std::path::Path;

/// Maximum examined inventory IDs for one isolated reconciliation window.
pub const MAX_WINDOW_ITEMS: usize = 250_000;
// Non-event key, after every timestamp/ID key (including prune's exclusive end).
const REPLAY_CURSOR_KEY: [u8; 41] = [u8::MAX; 41];

/// A complete bounded interval or an explicit requirement to split it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivedWindow {
    /// Only durable archive-confirmed IDs, in timestamp/ID order.
    Complete(Vec<([u8; 32], u64)>),
    /// The interval exceeded the examined-row budget. No truncated set is usable.
    TooDense,
}

/// Sync state database for negentropy reconciliation.
///
/// Stores (timestamp, event_id) pairs to enable efficient time-range queries.
/// This is used by negentropy sync to compare local state with remote relays.
///
/// # Key Format
///
/// ```text
/// Key:   [timestamp_be (8 bytes)][event_id (32 bytes)]
/// Value: empty
/// ```
///
/// Big-endian timestamp enables efficient range scans.
pub struct SyncStateDb {
    db: DBWithThreadMode<MultiThreaded>,
    pub(super) replay_lock: parking_lot::Mutex<()>,
}

impl SyncStateDb {
    pub(super) fn replay_cursor(&self) -> Result<Option<Vec<u8>>> {
        let cursor = self.db.get(REPLAY_CURSOR_KEY)?;
        if cursor.as_ref().is_some_and(|bytes| bytes.len() > 8192) {
            return Err(crate::Error::Validation(
                "oversized replay cursor".to_owned(),
            ));
        }
        Ok(cursor)
    }

    pub(super) fn save_replay_cursor(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > 8192 {
            return Err(crate::Error::Config("oversized replay cursor".to_owned()));
        }
        // All earlier inventory puts reach stable WAL before its completion cursor.
        // A crash in between repeats valid puts, never skips unflushed inventory.
        self.db.flush_wal(true)?;
        let mut options = rocksdb::WriteOptions::default();
        options.set_sync(true);
        self.db.put_opt(REPLAY_CURSOR_KEY, bytes, &options)?;
        Ok(())
    }

    /// Export a fixed inclusive window for the isolated worker, verifying legacy
    /// inventory against the shared durable dedupe index. Missing/Pending entries
    /// are omitted so the relay can send them again. This does not certify coverage.
    ///
    /// Bounds both returned and examined rows; split TooDense rather than truncate.
    /// Call from the bounded database executor, not an async reactor thread.
    pub fn archived_window(
        &self,
        since: u64,
        until: u64,
        max_items: usize,
        dedupe: &crate::DedupeIndex,
    ) -> Result<ArchivedWindow> {
        if since > until
            || until > i64::MAX as u64
            || max_items == 0
            || max_items > MAX_WINDOW_ITEMS
        {
            return Err(crate::Error::Config(
                "invalid inventory window bounds".to_owned(),
            ));
        }
        let start = Self::make_key(since, &[0; 32]);
        let end = Self::make_key(until, &[u8::MAX; 32]);
        let mut items = Vec::new();
        for (examined, item) in self
            .db
            .iterator(IteratorMode::From(&start, rocksdb::Direction::Forward))
            .enumerate()
        {
            let (key, _) = item?;
            // Timestamp-first ordering allows stopping without reading the rest
            // of history. Reserved replay metadata sorts beyond all valid times.
            if key.as_ref() > end.as_slice() {
                break;
            }
            let (id, timestamp) = Self::parse_key(&key)
                .ok_or_else(|| crate::Error::Validation("invalid inventory key".to_owned()))?;
            if examined == max_items {
                return Ok(ArchivedWindow::TooDense);
            }
            if dedupe.get_status(&id)? == Some(crate::EventStatus::Archived) {
                items.push((id, timestamp));
            }
        }
        Ok(ArchivedWindow::Complete(items))
    }

    /// Open or create a sync state database at the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory path for the RocksDB database
    pub fn open<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        tracing::info!("Opening sync state database at {}", path.display());

        let mut opts = Options::default();
        opts.create_if_missing(true);

        // LZ4 compression for space efficiency
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);

        // Optimize for write-heavy workload with moderate reads
        opts.set_write_buffer_size(32 * 1024 * 1024); // 32MB write buffer
        opts.set_max_write_buffer_number(2);

        // Set prefix extractor for timestamp prefix (8 bytes)
        // This enables efficient prefix-based iteration, though we use
        // IteratorMode::From for range scans rather than prefix_iterator().
        opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(8));

        // Parallelism
        opts.increase_parallelism(num_cpus::get().min(4) as i32);

        let db = DBWithThreadMode::<MultiThreaded>::open(&opts, path)?;

        Ok(Self {
            db,
            replay_lock: parking_lot::Mutex::new(()),
        })
    }

    /// Build a 40-byte key from timestamp and event_id.
    ///
    /// Key format: `[timestamp_be (8 bytes)][event_id (32 bytes)]`
    fn make_key(created_at: u64, event_id: &[u8; 32]) -> [u8; 40] {
        let mut key = [0u8; 40];
        key[0..8].copy_from_slice(&created_at.to_be_bytes()); // Big-endian for sorting
        key[8..40].copy_from_slice(event_id);
        key
    }

    /// Parse a 40-byte key into (event_id, timestamp).
    fn parse_key(key: &[u8]) -> Option<([u8; 32], u64)> {
        if key.len() != 40 {
            return None;
        }
        let ts = u64::from_be_bytes(key[0..8].try_into().ok()?);
        let mut event_id = [0u8; 32];
        event_id.copy_from_slice(&key[8..40]);
        Some((event_id, ts))
    }

    /// Get all events since a given timestamp.
    ///
    /// Returns a vector of (event_id, created_at) pairs for all events
    /// with `created_at >= since`. This is used by negentropy to build
    /// the local item set for reconciliation.
    ///
    /// # Arguments
    ///
    /// * `since` - Unix timestamp to start from (inclusive)
    pub fn get_items_since(&self, since: u64) -> Result<Vec<([u8; 32], u64)>> {
        let mut items = Vec::new();

        // Start key: [since_be][0x00...] (earliest possible event at this timestamp)
        let start_key = Self::make_key(since, &[0u8; 32]);

        // Iterate from start_key to end
        let iter = self
            .db
            .iterator(IteratorMode::From(&start_key, rocksdb::Direction::Forward));

        for item in iter {
            let (key, _value) = item?;
            if let Some((event_id, ts)) = Self::parse_key(&key) {
                items.push((event_id, ts));
            }
        }

        Ok(items)
    }

    /// Record that we've processed an event.
    ///
    /// This should be called after successfully writing an event to the archive.
    ///
    /// # Arguments
    ///
    /// * `event_id` - 32-byte event ID
    /// * `created_at` - Event's `created_at` timestamp
    pub fn record(&self, event_id: &[u8; 32], created_at: u64) -> Result<()> {
        let key = Self::make_key(created_at, event_id);
        self.db.put(key, [])?; // Empty value
        Ok(())
    }

    /// Record multiple events in a batch.
    ///
    /// More efficient than individual `record()` calls when processing
    /// many events (e.g., seeding from ClickHouse).
    ///
    /// # Arguments
    ///
    /// * `events` - Iterator of (event_id, created_at) pairs
    pub fn record_batch<'a, I>(&self, events: I) -> Result<usize>
    where
        I: Iterator<Item = (&'a [u8; 32], u64)>,
    {
        let mut batch = rocksdb::WriteBatch::default();
        let mut count = 0usize;

        for (event_id, created_at) in events {
            let key = Self::make_key(created_at, event_id);
            batch.put(key, []);
            count += 1;
        }

        if count > 0 {
            self.db.write(batch)?;
        }

        Ok(count)
    }

    /// Prune entries older than the given timestamp.
    ///
    /// This removes all entries with `created_at < before` to bound storage.
    ///
    /// # Arguments
    ///
    /// * `before` - Unix timestamp; entries older than this will be removed
    ///
    /// Returns the number of entries deleted.
    pub fn prune_before(&self, before: u64) -> Result<usize> {
        // End key: [before_be][0x00...] (exclusive upper bound)
        let end_key = Self::make_key(before, &[0u8; 32]);

        // Iterate from start and delete entries before the threshold
        let mut batch = rocksdb::WriteBatch::default();
        let mut count = 0usize;

        let iter = self.db.iterator(IteratorMode::Start);
        for item in iter {
            let (key, _) = item?;
            if key.as_ref() >= end_key.as_slice() {
                // Reached or passed the threshold
                break;
            }
            batch.delete(&key);
            count += 1;
        }

        if count > 0 {
            self.db.write(batch)?;
        }

        tracing::debug!(
            "Pruned {} sync state entries before timestamp {}",
            count,
            before
        );
        Ok(count)
    }

    /// Get the approximate number of keys in the database.
    pub fn approximate_count(&self) -> Result<u64> {
        let count = self
            .db
            .property_int_value("rocksdb.estimate-num-keys")?
            .unwrap_or(0);
        Ok(count)
    }

    /// Check if the database is empty or nearly empty.
    ///
    /// This is used to detect cold-start situations where we should
    /// seed from ClickHouse before the first sync.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.approximate_count()? < 100)
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }
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
        let _db = SyncStateDb::open(tmp.path()).unwrap();
    }

    #[test]
    fn test_record_and_get() {
        let tmp = TempDir::new().unwrap();
        let db = SyncStateDb::open(tmp.path()).unwrap();

        let id1 = test_event_id(1);
        let id2 = test_event_id(2);
        let id3 = test_event_id(3);

        // Record events at different timestamps
        db.record(&id1, 1000).unwrap();
        db.record(&id2, 2000).unwrap();
        db.record(&id3, 3000).unwrap();

        // Get all items since timestamp 0
        let items = db.get_items_since(0).unwrap();
        assert_eq!(items.len(), 3);

        // Get items since timestamp 2000 (should include id2 and id3)
        let items = db.get_items_since(2000).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, id2);
        assert_eq!(items[0].1, 2000);
        assert_eq!(items[1].0, id3);
        assert_eq!(items[1].1, 3000);

        // Get items since timestamp 3001 (should be empty)
        let items = db.get_items_since(3001).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_record_batch() {
        let tmp = TempDir::new().unwrap();
        let db = SyncStateDb::open(tmp.path()).unwrap();

        let id1 = test_event_id(1);
        let id2 = test_event_id(2);

        let events = vec![(&id1, 1000u64), (&id2, 2000u64)];
        let count = db.record_batch(events.into_iter()).unwrap();
        assert_eq!(count, 2);

        let items = db.get_items_since(0).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_prune() {
        let tmp = TempDir::new().unwrap();
        let db = SyncStateDb::open(tmp.path()).unwrap();

        let id1 = test_event_id(1);
        let id2 = test_event_id(2);
        let id3 = test_event_id(3);

        db.record(&id1, 1000).unwrap();
        db.record(&id2, 2000).unwrap();
        db.record(&id3, 3000).unwrap();

        // Prune entries before timestamp 2000
        db.prune_before(2000).unwrap();

        // Should only have id2 and id3 left
        let items = db.get_items_since(0).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, id2);
        assert_eq!(items[1].0, id3);
    }

    #[test]
    fn test_is_empty() {
        let tmp = TempDir::new().unwrap();
        let db = SyncStateDb::open(tmp.path()).unwrap();

        assert!(db.is_empty().unwrap());

        let id1 = test_event_id(1);
        db.record(&id1, 1000).unwrap();

        // Still "empty" since threshold is 100
        assert!(db.is_empty().unwrap());
    }

    #[test]
    fn test_ordering() {
        let tmp = TempDir::new().unwrap();
        let db = SyncStateDb::open(tmp.path()).unwrap();

        // Insert in random order
        let id1 = test_event_id(1);
        let id2 = test_event_id(2);
        let id3 = test_event_id(3);

        db.record(&id3, 3000).unwrap();
        db.record(&id1, 1000).unwrap();
        db.record(&id2, 2000).unwrap();

        // Should be returned in timestamp order
        let items = db.get_items_since(0).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].1, 1000);
        assert_eq!(items[1].1, 2000);
        assert_eq!(items[2].1, 3000);
    }
}
