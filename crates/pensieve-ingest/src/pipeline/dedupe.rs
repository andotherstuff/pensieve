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
//!
//! # Block cache
//!
//! Every point lookup consults one SST per LSM level, and each consulted SST
//! needs its filter block and index block. With
//! `cache_index_and_filter_blocks` those blocks live in the block cache, so the
//! cache must hold all of them or every lookup re-reads megabytes of filter
//! from the OS page cache. RocksDB's default 32 MiB cache is split into 64
//! shards of 512 KiB, smaller than a single ~2 MB full filter block of a 64 MB
//! SST, so with the default nothing stays cached at all. [`DedupeIndex`]
//! therefore owns a dedicated LRU block cache, sized by
//! [`DedupeOptions::block_cache_mib`]. Index and filter blocks are inserted
//! at high priority (RocksDB's default), which reserves half of the capacity
//! for them; L0 blocks are pinned. These are table read options only: the
//! on-disk format and every SST written are unchanged, so older binaries open
//! the database as before.
//!
//! # Thread Safety
//!
//! The [`DedupeIndex::check_and_mark_pending`] method is atomic - it uses an
//! internal mutex to prevent race conditions when multiple sources (e.g., live
//! ingestion and negentropy sync) concurrently process the same event ID.

use crate::Result;
use parking_lot::Mutex;
use rocksdb::{Cache, DBWithThreadMode, MultiThreaded, Options, WriteBatch, WriteOptions};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Default dedupe block cache capacity in MiB.
///
/// Measured with these options: 1.25 bytes of full bloom filter and about 0.16
/// bytes of index block per entry. Production's index (88 GB in ~1,500 64 MB
/// SSTs, roughly 2.0-2.3 billion entries) therefore needs about 3 GB resident.
/// Index and filter blocks get the high-priority half of the cache, so 16 GiB
/// keeps all of them resident until the key count reaches roughly 2-2.5 times
/// today's; the rest holds hot data blocks, which also raises the hit rate of
/// negentropy preflight lookups. Memory is allocated only as blocks are read,
/// so small databases (tests, backfills) use little of it.
pub const DEFAULT_BLOCK_CACHE_MIB: usize = 16 * 1024;

/// Tunables for opening a [`DedupeIndex`]. None of them affect the on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DedupeOptions {
    /// Capacity of the dedicated LRU block cache, in MiB (must be non-zero).
    pub block_cache_mib: usize,
}

impl Default for DedupeOptions {
    fn default() -> Self {
        Self {
            block_cache_mib: DEFAULT_BLOCK_CACHE_MIB,
        }
    }
}

/// Point-in-time block cache accounting for the dedupe index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCacheUsage {
    /// Configured capacity in bytes.
    pub capacity_bytes: u64,
    /// Bytes currently charged to the cache (data, index and filter blocks).
    pub usage_bytes: u64,
    /// Bytes held by pinned entries (blocks in use, pinned L0 metadata).
    pub pinned_bytes: u64,
}

/// Size of the SST metadata that lookups need resident, from table properties.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableMetadataSize {
    /// Entries across all live SSTs (an overwritten key counts once per SST).
    pub entries: u64,
    /// Total size of all filter blocks in bytes.
    pub filter_bytes: u64,
    /// Total size of all index blocks in bytes.
    pub index_bytes: u64,
}

impl TableMetadataSize {
    /// Parse RocksDB's `rocksdb.aggregated-table-properties` string.
    fn parse(properties: &str) -> Self {
        let mut size = Self::default();
        for field in properties.split(';') {
            let Some((name, value)) = field.split_once('=') else {
                continue;
            };
            let Ok(value) = value.trim().parse::<u64>() else {
                continue;
            };
            let name = name.trim();
            if name == "# entries" {
                size.entries = value;
            } else if name == "filter block size" {
                size.filter_bytes = value;
            } else if name.starts_with("index block size") {
                size.index_bytes = value;
            }
        }
        size
    }
}

/// Status flags stored as values in the dedupe index.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStatus {
    /// Legacy on-disk status written by older builds. New builds track in-flight
    /// events in memory and only ever persist `Archived`; this variant is kept so
    /// that existing databases (which contain `Pending` values) still read as
    /// "already seen".
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
/// The `check_and_mark_pending` method uses an internal mutex to ensure atomicity
/// when multiple sources concurrently attempt to claim the same event ID.
pub struct DedupeIndex {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
    /// Dedicated block cache (also referenced by `db`'s table options).
    block_cache: Cache,
    /// Durable point lookups performed, so tests can prove a path skipped them.
    #[cfg(test)]
    point_lookups: std::sync::atomic::AtomicU64,
    /// In-flight event IDs: claimed (written to the current, unsealed segment) but
    /// not yet durably archived. Tracked in memory ONLY — never persisted.
    ///
    /// These reservations disappear on a crash, but some unsealed bytes can remain
    /// in an `.open` file. Preserve that file: startup blocks until the recovery
    /// gate in `docs/archive_failure_recovery.md` is satisfied. Never delete an
    /// orphan merely to force refetching. Only durably-sealed events are
    /// written to disk as `Archived`, and that on-disk state is what actually
    /// suppresses re-fetching.
    ///
    /// The mutex also serializes check-and-mark so two sources (live and negentropy)
    /// cannot both claim the same novel event ID.
    // false = revocable reservation; true = retained archive obligation.
    pending: Mutex<HashMap<[u8; 32], bool>>,
}

/// An exclusive in-memory admission claim, released on pre-admission failure.
///
/// Call [`Self::retain`] before transferring bytes to a writer whose error may
/// mean partial admission. Never release a claim on an ambiguous write failure.
pub struct PendingAdmission<'a> {
    index: &'a DedupeIndex,
    event_id: [u8; 32],
    retained: bool,
}

impl PendingAdmission<'_> {
    /// Transfer ownership to the archive writer until durable segment sealing.
    pub fn retain(mut self) {
        if let Some(owned) = self.index.pending.lock().get_mut(&self.event_id) {
            *owned = true;
        }
        self.retained = true;
    }
}

impl Drop for PendingAdmission<'_> {
    fn drop(&mut self) {
        if !self.retained {
            self.index.pending.lock().remove(&self.event_id);
        }
    }
}

impl DedupeIndex {
    /// Reserve an event until the writer takes responsibility for its bytes.
    /// A duplicate never owns a guard and therefore cannot release another claim.
    pub fn reserve(&self, event_id: &[u8; 32]) -> Result<Option<PendingAdmission<'_>>> {
        Ok(self
            .check_and_mark_pending(event_id)?
            .then(|| PendingAdmission {
                index: self,
                event_id: *event_id,
                retained: false,
            }))
    }

    /// Re-admit a legacy on-disk Pending ID without deleting or trusting its marker.
    ///
    /// Only use after the archive startup recovery gate. Shares the live admission
    /// mutex, so a concurrent source cannot acquire the same ID. A new durable seal
    /// overwrites Pending with Archived; pre-admission failure leaves it retryable.
    /// This may preserve a duplicate archive copy, never invent archive proof.
    pub fn reserve_unarchived(&self, event_id: &[u8; 32]) -> Result<Option<PendingAdmission<'_>>> {
        let mut pending = self.pending.lock();
        if self.get_status(event_id)? == Some(EventStatus::Archived)
            || pending.contains_key(event_id)
        {
            return Ok(None);
        }
        pending.insert(*event_id, false);
        Ok(Some(PendingAdmission {
            index: self,
            event_id: *event_id,
            retained: false,
        }))
    }

    /// Whether bytes have a non-revocable archive owner (not merely a reservation).
    ///
    /// A retained claim is still not durability proof. Only Archived satisfies
    /// completion; a failed writer preserves retained claims behind its recovery gate.
    pub fn has_archive_owner(&self, event_id: &[u8; 32]) -> Result<bool> {
        let pending = self.pending.lock();
        Ok(pending.get(event_id) == Some(&true)
            || self.get_status(event_id)? == Some(EventStatus::Archived))
    }
    /// Open or create a dedupe index at the given path with [`DedupeOptions::default`].
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
    pub fn open<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        Self::open_with_options(path, DedupeOptions::default())
    }

    /// Open or create a dedupe index with explicit cache tunables.
    ///
    /// The options only change how tables are read; the database stays
    /// compatible with builds that open it with other options.
    pub fn open_with_options<P>(path: P, options: DedupeOptions) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let cache_bytes = options
            .block_cache_mib
            .checked_mul(1024 * 1024)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| {
                crate::Error::Config(format!(
                    "invalid dedupe block cache size: {} MiB",
                    options.block_cache_mib
                ))
            })?;
        tracing::info!("Opening dedupe index at {}", path.display());

        // Bloom filters for fast "not found" lookups
        // 10 bits per key = ~1% false positive rate
        let block_cache = Cache::new_lru_cache(cache_bytes);
        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        // Filter and index blocks are charged to (and bounded by) the dedicated
        // cache, at high priority (RocksDB's default for these blocks).
        block_opts.set_cache_index_and_filter_blocks(true);
        block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
        block_opts.set_block_cache(&block_cache);
        let opts = Self::db_options(&block_opts);

        let db = DBWithThreadMode::<MultiThreaded>::open(&opts, path)?;
        let index = Self {
            db: Arc::new(db),
            block_cache,
            #[cfg(test)]
            point_lookups: std::sync::atomic::AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
        };

        let metadata = index.table_metadata_size().unwrap_or_default();
        let resident = metadata.filter_bytes + metadata.index_bytes;
        tracing::info!(
            path = %path.display(),
            block_cache_mib = options.block_cache_mib,
            cache_index_and_filter_blocks = true,
            pin_l0_filter_and_index_blocks = true,
            bloom_bits_per_key = 10,
            sst_entries = metadata.entries,
            filter_bytes = metadata.filter_bytes,
            index_bytes = metadata.index_bytes,
            "dedupe block cache configured"
        );
        // Index and filter blocks get the high-priority half of an LRU cache.
        // Beyond that they compete with data blocks and lookups start
        // re-reading them from disk or the page cache.
        if resident > cache_bytes as u64 / 2 {
            tracing::warn!(
                resident_bytes = resident,
                block_cache_bytes = cache_bytes,
                "dedupe filter and index blocks exceed half the block cache; raise --dedupe-block-cache-mib"
            );
        }
        Ok(index)
    }

    /// Database options shared by every build: write buffers, SST size,
    /// compression and parallelism. `table` carries the read-side tunables.
    fn db_options(table: &rocksdb::BlockBasedOptions) -> Options {
        let mut opts = Options::default();
        opts.create_if_missing(true);

        // Optimize for write-heavy workload
        opts.set_write_buffer_size(64 * 1024 * 1024); // 64MB write buffer
        opts.set_max_write_buffer_number(3);
        opts.set_target_file_size_base(64 * 1024 * 1024); // 64MB SST files

        opts.set_block_based_table_factory(table);

        // Compression for disk space efficiency
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);

        // Parallelism
        opts.increase_parallelism(num_cpus::get() as i32);
        opts.set_max_background_jobs(4);
        opts
    }

    /// Current block cache capacity, usage and pinned usage.
    pub fn block_cache_usage(&self) -> BlockCacheUsage {
        let capacity_bytes = self
            .db
            .property_int_value("rocksdb.block-cache-capacity")
            .ok()
            .flatten()
            .unwrap_or(0);
        BlockCacheUsage {
            capacity_bytes,
            usage_bytes: self.block_cache.get_usage() as u64,
            pinned_bytes: self.block_cache.get_pinned_usage() as u64,
        }
    }

    /// Total filter and index block sizes over all live SSTs.
    pub fn table_metadata_size(&self) -> Result<TableMetadataSize> {
        Ok(self
            .db
            .property_value("rocksdb.aggregated-table-properties")?
            .map(|properties| TableMetadataSize::parse(&properties))
            .unwrap_or_default())
    }

    /// Check if an event ID has been seen.
    ///
    /// Returns `Some(status)` if the event exists, `None` if not seen.
    ///
    /// # Arguments
    ///
    /// * `event_id` - 32-byte event ID (raw bytes)
    pub fn get_status(&self, event_id: &[u8; 32]) -> Result<Option<EventStatus>> {
        #[cfg(test)]
        self.point_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // An event is "not new" if it's either durably on disk or currently in-flight.
        if self.pending.lock().contains_key(event_id) {
            return Ok(false);
        }
        Ok(self.get_status(event_id)?.is_none())
    }

    /// Check and mark an event as pending in one atomic operation.
    ///
    /// Returns `true` if the event is new (was not seen before),
    /// `false` if it was already seen.
    ///
    /// This is the main API for deduplication during ingestion. The operation
    /// is atomic: a mutex ensures that concurrent calls from different sources
    /// (e.g., live ingestion and negentropy sync) cannot both "win" for the
    /// same event ID.
    pub fn check_and_mark_pending(&self, event_id: &[u8; 32]) -> Result<bool> {
        // Hold the lock for the entire check-and-mark operation to prevent races.
        // Without this, two sources could both see "not exists" and both write.
        let mut pending = self.pending.lock();

        // Already durably recorded on disk? (`Archived`, or a legacy `Pending`
        // value from an older build — both mean "we already have this".)
        if self.get_status(event_id)?.is_some() {
            return Ok(false);
        }

        // Claim it in-flight. This is in memory only and becomes a durable
        // `Archived` entry when the segment is sealed (see `mark_archived`).
        match pending.entry(*event_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(false);
                Ok(true)
            }
            std::collections::hash_map::Entry::Occupied(_) => Ok(false),
        }
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
        let mut archived: Vec<[u8; 32]> = Vec::new();

        for event_id in event_ids {
            batch.put(event_id, [EventStatus::Archived.to_byte()]);
            archived.push(*event_id);
        }

        if !archived.is_empty() {
            let mut write_opts = WriteOptions::default();
            write_opts.set_sync(true); // Durable before we drop the in-flight markers
            self.db.write_opt(batch, &write_opts)?;

            // Now that the IDs are durably `Archived`, remove them from the
            // in-flight set so the in-memory set only ever holds unsealed events.
            let mut pending = self.pending.lock();
            for id in &archived {
                pending.remove(id);
            }
            tracing::debug!("Marked {} events as archived", archived.len());
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

    /// Durable point lookups performed so far (test builds only).
    #[cfg(test)]
    pub(crate) fn point_lookups(&self) -> u64 {
        self.point_lookups
            .load(std::sync::atomic::Ordering::Relaxed)
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

        // In-flight events are tracked in memory (not persisted), so they read as
        // "not new" but have no on-disk status until the segment is sealed.
        assert!(!index.is_new(&id1).unwrap());
        assert_eq!(index.get_status(&id1).unwrap(), None);
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
        assert_eq!(index.get_status(&id1).unwrap(), Some(EventStatus::Archived));
        assert_eq!(index.get_status(&id2).unwrap(), Some(EventStatus::Archived));
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

    #[test]
    fn test_pending_not_persisted_but_archived_is() {
        let tmp = TempDir::new().unwrap();
        let id1 = test_event_id(1);
        let id2 = test_event_id(2);

        {
            let index = DedupeIndex::open(tmp.path()).unwrap();
            // Claim both in-flight, then durably archive only id1.
            assert!(index.check_and_mark_pending(&id1).unwrap());
            assert!(index.check_and_mark_pending(&id2).unwrap());
            index.mark_archived([&id1].into_iter()).unwrap();
        }

        // Re-opening simulates a restart: in-memory in-flight state is gone, so an
        // unsealed event (id2) becomes re-fetchable while an archived one (id1)
        // stays suppressed. This is the crash-safety guarantee.
        let index = DedupeIndex::open(tmp.path()).unwrap();
        assert!(
            !index.is_new(&id1).unwrap(),
            "archived event must stay seen"
        );
        assert!(
            index.is_new(&id2).unwrap(),
            "unsealed event must be re-fetchable after restart"
        );
    }

    /// Table options of builds before the dedicated block cache: RocksDB's
    /// default 32 MiB LRU cache, nothing pinned.
    fn legacy_table_options() -> rocksdb::BlockBasedOptions {
        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        block_opts.set_cache_index_and_filter_blocks(true);
        block_opts
    }

    fn open_legacy(path: &Path) -> DBWithThreadMode<MultiThreaded> {
        let opts = DedupeIndex::db_options(&legacy_table_options());
        DBWithThreadMode::<MultiThreaded>::open(&opts, path).unwrap()
    }

    /// Pseudo-random 32-byte IDs (splitmix64), shaped like SHA-256 event IDs.
    fn random_id(n: u64) -> [u8; 32] {
        let mut state = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut id = [0u8; 32];
        for chunk in id.chunks_mut(8) {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            chunk.copy_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        id
    }

    #[test]
    fn zero_block_cache_is_rejected() {
        let tmp = TempDir::new().unwrap();
        assert!(
            DedupeIndex::open_with_options(tmp.path(), DedupeOptions { block_cache_mib: 0 })
                .is_err()
        );
    }

    #[test]
    fn dedicated_block_cache_is_configured_and_accounted() {
        let tmp = TempDir::new().unwrap();
        let index = DedupeIndex::open_with_options(
            tmp.path(),
            DedupeOptions {
                block_cache_mib: 16,
            },
        )
        .unwrap();
        let ids: Vec<[u8; 32]> = (0..10_000).map(random_id).collect();
        index.mark_archived(ids.iter()).unwrap();
        index.flush().unwrap();
        for id in &ids {
            assert_eq!(index.get_status(id).unwrap(), Some(EventStatus::Archived));
        }
        let usage = index.block_cache_usage();
        assert_eq!(usage.capacity_bytes, 16 * 1024 * 1024);
        assert!(usage.usage_bytes > 0, "lookups must populate the cache");
        assert!(usage.usage_bytes <= usage.capacity_bytes);
        let metadata = index.table_metadata_size().unwrap();
        assert_eq!(metadata.entries, 10_000);
        // 10 bits/key bloom: about 1.25 bytes per key.
        assert!((10_000..=15_000).contains(&metadata.filter_bytes));
        assert!(metadata.index_bytes > 0);
    }

    #[test]
    fn aggregated_table_properties_parse() {
        let parsed = TableMetadataSize::parse(
            "# data blocks=12; # entries=1000; # deletions=0; raw key size=40000; \
             index block size (user-key? 0, delta-value? 1)=345; filter block size=1280; \
             (estimated) table size=99",
        );
        assert_eq!(
            parsed,
            TableMetadataSize {
                entries: 1000,
                filter_bytes: 1280,
                index_bytes: 345,
            }
        );
        assert_eq!(TableMetadataSize::parse(""), TableMetadataSize::default());
    }

    /// Rollback safety: the cache options are read-side only, so a database
    /// written under either option set reads identically under the other.
    #[test]
    fn database_round_trips_between_legacy_and_dedicated_cache_options() {
        let tmp = TempDir::new().unwrap();
        let new_ids: Vec<[u8; 32]> = (0..1_000).map(random_id).collect();
        let old_ids: Vec<[u8; 32]> = (1_000..2_000).map(random_id).collect();
        {
            let index = DedupeIndex::open(tmp.path()).unwrap();
            index.mark_archived(new_ids.iter()).unwrap();
            index.flush().unwrap();
        }
        {
            let legacy = open_legacy(tmp.path());
            for id in &new_ids {
                assert_eq!(
                    legacy.get(id).unwrap().as_deref(),
                    Some(&[EventStatus::Archived.to_byte()][..])
                );
            }
            for id in &old_ids {
                legacy.put(id, [EventStatus::Archived.to_byte()]).unwrap();
            }
            legacy.flush().unwrap();
        }
        let index = DedupeIndex::open(tmp.path()).unwrap();
        for id in new_ids.iter().chain(&old_ids) {
            assert_eq!(index.get_status(id).unwrap(), Some(EventStatus::Archived));
        }
        assert_eq!(index.get_status(&random_id(5_000)).unwrap(), None);
    }

    /// Lookup throughput, legacy options vs the dedicated cache. Not run in CI:
    ///
    /// ```text
    /// DEDUPE_BENCH_KEYS=20000000 cargo test --release -p pensieve-ingest \
    ///     --lib dedupe_lookup_benchmark -- --ignored --nocapture
    /// ```
    ///
    /// Uses production's 64 MB SSTs so each full filter block (~2 MB) has the
    /// production size; a 512 KiB shard of the default cache cannot hold one.
    #[test]
    #[ignore = "benchmark; run explicitly with --ignored --nocapture"]
    fn dedupe_lookup_benchmark() {
        let keys: u64 = std::env::var("DEDUPE_BENCH_KEYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000_000);
        let lookups: u64 = std::env::var("DEDUPE_BENCH_LOOKUPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500_000);
        let dir = match std::env::var_os("DEDUPE_BENCH_DIR") {
            Some(dir) => TempDir::new_in(dir).unwrap(),
            None => TempDir::new().unwrap(),
        };
        let started = std::time::Instant::now();
        {
            let index = DedupeIndex::open(dir.path()).unwrap();
            let mut batch = Vec::with_capacity(100_000);
            for n in 0..keys {
                batch.push(random_id(n));
                if batch.len() == batch.capacity() {
                    index.mark_archived(batch.iter()).unwrap();
                    batch.clear();
                }
            }
            index.mark_archived(batch.iter()).unwrap();
            index.flush().unwrap();
            index.db.compact_range::<&[u8], &[u8]>(None, None);
            let metadata = index.table_metadata_size().unwrap();
            let ssts = index.db.live_files().map(|files| files.len()).unwrap_or(0);
            println!(
                "built {keys} keys in {:.1}s: {ssts} SSTs, {} entries, filter {:.2} B/entry, index {:.2} B/entry",
                started.elapsed().as_secs_f64(),
                metadata.entries,
                metadata.filter_bytes as f64 / metadata.entries.max(1) as f64,
                metadata.index_bytes as f64 / metadata.entries.max(1) as f64,
            );
        }
        // Half hits, half misses, like live ingest's mix of duplicates and new IDs.
        let probe = |n: u64| {
            if n.is_multiple_of(2) {
                random_id((n.wrapping_mul(7_919)) % keys)
            } else {
                random_id(keys + n)
            }
        };
        let measure = |label: &str, get: &dyn Fn(&[u8; 32]) -> bool| {
            for pass in 0..2 {
                let started = std::time::Instant::now();
                let mut hits = 0u64;
                for n in 0..lookups {
                    hits += u64::from(get(&probe(n)));
                }
                let secs = started.elapsed().as_secs_f64();
                println!(
                    "{label} pass {pass}: {lookups} gets in {secs:.2}s = {:.0} gets/s ({hits} hits)",
                    lookups as f64 / secs
                );
            }
        };
        {
            let legacy = open_legacy(dir.path());
            measure("legacy 32 MiB default cache", &|id| {
                legacy.get(id).unwrap().is_some()
            });
        }
        {
            let index = DedupeIndex::open(dir.path()).unwrap();
            measure("dedicated 16 GiB cache", &|id| {
                index.get_status(id).unwrap().is_some()
            });
            let usage = index.block_cache_usage();
            println!(
                "dedicated cache usage {} B, pinned {} B",
                usage.usage_bytes, usage.pinned_bytes
            );
        }
    }
}
