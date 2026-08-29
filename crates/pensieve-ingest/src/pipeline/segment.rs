//! Segment writer for notepack archive format.
//!
//! This module provides the [`SegmentWriter`] which writes events to
//! length-prefixed notepack segments and handles sealing/rotation.
//!
//! # Segment Format
//!
//! Each segment file contains:
//! ```text
//! [u32 little-endian length][notepack bytes]
//! [u32 little-endian length][notepack bytes]
//! ...
//! ```
//!
//! # Sealing
//!
//! Segments are sealed (finalized) when they exceed a size threshold.
//! On seal:
//! 1. The open file is fsync'd and atomically renamed to its sealed name
//! 2. Optional compression runs
//! 3. All downstream consumers are notified
//! 4. A new open segment is created on the next write

use crate::logging::compact_error;
use crate::{Error, Result};

use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;
use flate2::Compression;
use flate2::write::GzEncoder;
use parking_lot::Mutex;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use super::dedupe::DedupeIndex;
pub use pensieve_core::{LatestEventWatermark, read_latest_event_watermark};

/// Configuration for the segment writer.
#[derive(Debug, Clone)]
pub struct SegmentConfig {
    /// Directory to write segments to.
    pub output_dir: PathBuf,

    /// Maximum segment size in bytes before sealing.
    /// Default: 256 MB
    pub max_segment_size: usize,

    /// Prefix for segment file names.
    /// Default: "segment"
    pub segment_prefix: String,

    /// Compress sealed segments with gzip.
    /// Default: true
    pub compress: bool,

    /// Optional canonical JSON watermark updated after each durable segment seal.
    ///
    /// Publication is best-effort so an analytics sidecar can never prevent the
    /// archive from sealing. Readers validate the file strictly and fail closed.
    pub latest_event_watermark_path: Option<PathBuf>,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("./segments"),
            max_segment_size: 256 * 1024 * 1024, // 256 MB
            segment_prefix: "segment".to_string(),
            compress: true,
            latest_event_watermark_path: None,
        }
    }
}

/// Information about a sealed segment.
#[derive(Debug, Clone)]
pub struct SealedSegment {
    /// Path to the segment file (will be .notepack.gz if compressed).
    pub path: PathBuf,

    /// Segment number.
    pub segment_number: u64,

    /// Number of events in the segment.
    pub event_count: usize,

    /// Uncompressed size of the segment in bytes.
    pub size_bytes: usize,

    /// Compressed size in bytes (same as size_bytes if not compressed).
    pub compressed_size_bytes: usize,

    /// Event IDs in this segment (for marking as archived).
    pub event_ids: Vec<[u8; 32]>,

    /// When the segment was sealed.
    pub sealed_at: DateTime<Utc>,
}

/// A packed event ready to write.
pub struct PackedEvent {
    /// The 32-byte event ID.
    pub event_id: [u8; 32],

    /// Event creation timestamp in Unix seconds.
    pub created_at: u64,

    /// The notepack-encoded bytes (without length prefix).
    pub data: Vec<u8>,
}

/// Pack a nostr_sdk Event into notepack format.
///
/// This serializes the event using the notepack format for archival storage.
/// Call this **after** deduplication checks to avoid wasted work on duplicates.
pub fn pack_nostr_event(event: &nostr_sdk::Event) -> crate::Result<PackedEvent> {
    use notepack::{NoteBuf, pack_note_into};

    let mut buf = Vec::with_capacity(512);

    // Convert tags to the format notepack expects
    let tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect())
        .collect();

    // Format signature as hex
    let sig_bytes = event.sig.serialize();
    let sig_hex = hex::encode(sig_bytes);

    let note = NoteBuf {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16() as u64,
        tags,
        content: event.content.clone(),
        sig: sig_hex,
    };

    pack_note_into(&note, &mut buf).map_err(|e| crate::Error::Serialization(e.to_string()))?;

    Ok(PackedEvent {
        event_id: *event.id.as_bytes(),
        created_at: event.created_at.as_secs(),
        data: buf,
    })
}

/// Internal state for the current segment being written.
struct CurrentSegment {
    /// The file writer.
    writer: BufWriter<File>,

    /// Path to the current segment file.
    path: PathBuf,

    /// Number of events written to this segment.
    event_count: usize,

    /// Current size in bytes.
    size_bytes: usize,

    /// Event IDs in this segment.
    event_ids: Vec<[u8; 32]>,

    /// Event timestamps retained until seal for an exact eligible maximum.
    created_ats: Option<Vec<u64>>,
}

/// Segment writer for the notepack archive.
///
/// Thread-safe: uses internal locking for writes.
pub struct SegmentWriter {
    config: SegmentConfig,
    current: Mutex<Option<CurrentSegment>>,
    segment_number: AtomicU64,
    total_events: AtomicUsize,
    total_bytes: AtomicUsize,
    /// Wrapped in Arc for sharing with background compression threads.
    total_compressed_bytes: Arc<AtomicUsize>,
    /// Compression jobs must be joined before a finite writer process exits.
    compression_threads: Mutex<Vec<JoinHandle<()>>>,
    sealed_senders: Vec<Sender<SealedSegment>>,
    /// Optional dedupe index. When present, every sealed segment's events are
    /// marked durably `Archived` here (after the segment file is fsync'd), which is
    /// what makes mid-stream seals crash-safe. Backfill binaries pass `None` and
    /// mark archived themselves.
    dedupe: Option<Arc<DedupeIndex>>,
    /// Serializes cumulative watermark read/merge/write publication.
    watermark_publication: Mutex<()>,
}

impl SegmentWriter {
    /// Create a new segment writer.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the writer
    /// * `sealed_sender` - Optional channel to send sealed segment notifications
    /// * `dedupe` - Optional dedupe index; when set, each sealed segment's events
    ///   are durably marked `Archived` after the file is fsync'd
    pub fn new(
        config: SegmentConfig,
        sealed_sender: Option<Sender<SealedSegment>>,
        dedupe: Option<Arc<DedupeIndex>>,
    ) -> Result<Self> {
        Self::new_with_senders(config, sealed_sender.into_iter().collect(), dedupe)
    }

    /// Create a writer that fans each sealed segment out to all downstream consumers.
    pub fn new_with_senders(
        config: SegmentConfig,
        sealed_senders: Vec<Sender<SealedSegment>>,
        dedupe: Option<Arc<DedupeIndex>>,
    ) -> Result<Self> {
        // Create output directory if it doesn't exist
        fs::create_dir_all(&config.output_dir)?;

        // Find the next segment number by scanning existing files
        let next_segment = Self::find_next_segment_number(&config)?;

        tracing::info!(
            "SegmentWriter initialized: output_dir={}, max_size={}, starting at segment {}",
            config.output_dir.display(),
            config.max_segment_size,
            next_segment
        );

        Ok(Self {
            config,
            current: Mutex::new(None),
            segment_number: AtomicU64::new(next_segment),
            total_events: AtomicUsize::new(0),
            total_bytes: AtomicUsize::new(0),
            total_compressed_bytes: Arc::new(AtomicUsize::new(0)),
            compression_threads: Mutex::new(Vec::new()),
            sealed_senders,
            dedupe,
            watermark_publication: Mutex::new(()),
        })
    }

    /// Find the next segment number by scanning existing files.
    fn find_next_segment_number(config: &SegmentConfig) -> Result<u64> {
        let mut max_num = None;

        if config.output_dir.exists() {
            for entry in fs::read_dir(&config.output_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let name_str = name.to_string_lossy();

                // Parse sealed names and the crash-recoverable open-file name.
                if let Some(rest) = name_str.strip_prefix(&format!("{}-", config.segment_prefix)) {
                    let num_str = rest
                        .strip_suffix(".notepack.gz")
                        .or_else(|| rest.strip_suffix(".notepack"))
                        .or_else(|| rest.strip_suffix(".notepack.open"));

                    if let Some(num_str) = num_str
                        && let Ok(num) = num_str.parse::<u64>()
                    {
                        max_num = Some(max_num.map_or(num, |current: u64| current.max(num)));
                    }
                }
            }
        }

        // Start at the next number after the highest found
        max_num.map_or(Ok(0), |number| {
            number
                .checked_add(1)
                .ok_or_else(|| Error::Segment("segment number space exhausted".to_string()))
        })
    }

    /// Generate the path for a segment file.
    ///
    /// Uses 9-digit zero-padded numbering for lexicographic sorting
    /// and headroom up to ~1 billion segments (~256 PB at 256MB each).
    fn segment_path(&self, segment_number: u64) -> PathBuf {
        self.config.output_dir.join(format!(
            "{}-{:09}.notepack.open",
            self.config.segment_prefix, segment_number
        ))
    }

    fn sealed_segment_path(path: &Path) -> Result<PathBuf> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".open"))
            .ok_or_else(|| {
                Error::Segment(format!("invalid open segment path: {}", path.display()))
            })?;
        Ok(path.with_file_name(name))
    }

    /// Get or create the current segment.
    fn ensure_current_segment(&self) -> Result<()> {
        let mut current = self.current.lock();

        if current.is_none() {
            let segment_num = self.segment_number.load(Ordering::SeqCst);
            let path = self.segment_path(segment_num);

            tracing::debug!("Creating new segment: {}", path.display());

            let file = File::create(&path)?;
            let writer = BufWriter::with_capacity(8 * 1024 * 1024, file); // 8MB buffer

            *current = Some(CurrentSegment {
                writer,
                path,
                event_count: 0,
                size_bytes: 0,
                event_ids: Vec::with_capacity(10000),
                created_ats: self
                    .config
                    .latest_event_watermark_path
                    .as_ref()
                    .map(|_| Vec::with_capacity(10000)),
            });
        }

        Ok(())
    }

    /// Write a packed event to the current segment.
    ///
    /// Returns `true` if a segment was sealed as a result.
    pub fn write(&self, event: PackedEvent) -> Result<bool> {
        self.ensure_current_segment()?;

        let mut current = self.current.lock();
        let segment = current
            .as_mut()
            .ok_or_else(|| Error::Segment("No current segment".to_string()))?;

        // Write length-prefixed format: [u32 length][notepack bytes]
        let len_bytes = (event.data.len() as u32).to_le_bytes();
        segment.writer.write_all(&len_bytes)?;
        segment.writer.write_all(&event.data)?;

        // Update stats
        let written_bytes = 4 + event.data.len();
        segment.event_count += 1;
        segment.size_bytes += written_bytes;
        segment.event_ids.push(event.event_id);
        if let Some(created_ats) = &mut segment.created_ats {
            created_ats.push(event.created_at);
        }

        self.total_events.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_add(written_bytes, Ordering::Relaxed);

        // Check if we need to seal
        let should_seal = segment.size_bytes >= self.config.max_segment_size;

        drop(current);

        if should_seal {
            self.seal()?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Write multiple packed events.
    ///
    /// Returns the number of segments sealed.
    pub fn write_batch(&self, events: Vec<PackedEvent>) -> Result<usize> {
        let mut seals = 0;
        for event in events {
            if self.write(event)? {
                seals += 1;
            }
        }
        Ok(seals)
    }

    /// Remove the current segment and reserve the next segment number atomically.
    ///
    /// Reserving the number while holding `current` prevents a concurrent writer
    /// from recreating the same `.open` path while this segment is being flushed
    /// and renamed.
    fn take_current_for_seal(&self) -> Option<(CurrentSegment, u64)> {
        let mut current = self.current.lock();
        let segment = current.take()?;
        let segment_number = self.segment_number.fetch_add(1, Ordering::SeqCst);
        Some((segment, segment_number))
    }

    /// Seal the current segment.
    ///
    /// This finalizes the current segment and prepares for a new one.
    /// If compression is enabled, it's done in a background thread to avoid
    /// blocking the async runtime.
    ///
    /// Returns the sealed segment info (event_ids are immediately available
    /// for marking as archived). Downstream notifications are sent after
    /// compression completes (from the background thread).
    pub fn seal(&self) -> Result<Option<SealedSegment>> {
        let (segment, segment_number) = match self.take_current_for_seal() {
            Some(segment) => segment,
            None => return Ok(None),
        };

        let CurrentSegment {
            writer,
            path: open_path,
            event_count,
            size_bytes,
            event_ids,
            created_ats,
        } = segment;

        // Flush the buffer and fsync so the segment bytes are durable on disk
        // BEFORE we record its events as archived. Otherwise a machine crash could
        // leave events marked `Archived` (hence never re-fetched) while their bytes
        // were still only in the OS page cache.
        let file = writer
            .into_inner()
            .map_err(|e| Error::Segment(format!("failed to flush segment on seal: {e}")))?;
        file.sync_all()?;
        drop(file);

        // Only the final name is discoverable as a sealed work unit. A crash can
        // therefore leave an `.open` file, but can never make a partial segment
        // look ready to downstream consumers.
        let path = Self::sealed_segment_path(&open_path)?;
        if path.exists() {
            return Err(Error::Segment(format!(
                "refusing to replace existing sealed segment: {}",
                path.display()
            )));
        }
        fs::rename(&open_path, &path)?;
        File::open(
            path.parent()
                .ok_or_else(|| Error::Segment("segment has no parent directory".to_string()))?,
        )?
        .sync_all()?;

        // Record the segment's events as durably archived (and clear their in-flight
        // markers). Doing this on EVERY seal — not just the final one — is what makes
        // mid-stream sealed events durable; a crash before this point leaves them
        // re-fetchable rather than silently lost.
        if let Some(dedupe) = &self.dedupe {
            dedupe.mark_archived(event_ids.iter())?;
        }

        let sealed_at = Utc::now();

        self.publish_latest_event_watermark(
            segment_number,
            sealed_at,
            created_ats
                .into_iter()
                .flatten()
                .filter(|created_at| {
                    *created_at <= sealed_at.timestamp() as u64
                        && *created_at <= u64::from(u32::MAX)
                })
                .max(),
        );

        // Build the sealed segment info (returned immediately)
        // If compressing, the path/size will be updated in the background
        let sealed = SealedSegment {
            path: path.clone(),
            segment_number,
            event_count,
            size_bytes,
            compressed_size_bytes: size_bytes, // Will be updated if compressed
            event_ids,
            sealed_at,
        };

        if self.config.compress {
            self.reap_finished_compression_threads();

            // Spawn background thread for compression to avoid blocking async runtime
            let gz_path = path.with_extension("notepack.gz");
            let senders = self.sealed_senders.clone();
            let total_compressed_bytes = self.total_compressed_bytes.clone();
            let sealed_for_notify = sealed.clone();

            let compression_thread = std::thread::spawn(move || {
                match Self::compress_file_static(&path, &gz_path) {
                    Ok(compressed_bytes) => {
                        // Guard against divide-by-zero on an empty/forced seal.
                        let ratio_pct = if size_bytes > 0 {
                            (compressed_bytes as f64 / size_bytes as f64) * 100.0
                        } else {
                            0.0
                        };
                        tracing::info!(
                            "Sealed segment {}: {} events, {} bytes -> {} bytes ({:.1}%) at {}",
                            segment_number,
                            event_count,
                            size_bytes,
                            compressed_bytes,
                            ratio_pct,
                            gz_path.display()
                        );

                        // Track compressed bytes
                        total_compressed_bytes.fetch_add(compressed_bytes, Ordering::Relaxed);

                        let compressed_sealed = SealedSegment {
                            path: gz_path,
                            compressed_size_bytes: compressed_bytes,
                            ..sealed_for_notify
                        };
                        Self::notify_all(&senders, &compressed_sealed);
                    }
                    Err(e) => {
                        tracing::error!(
                            segment_number,
                            path = %path.display(),
                            error = %compact_error(&e),
                            "failed to compress segment"
                        );
                        // If the final gzip exists, prefer it even when a later
                        // directory sync/cleanup step reported an error.
                        if let Ok(metadata) = fs::metadata(&gz_path) {
                            let compressed_bytes =
                                usize::try_from(metadata.len()).unwrap_or(usize::MAX);
                            total_compressed_bytes.fetch_add(compressed_bytes, Ordering::Relaxed);
                            let compressed_sealed = SealedSegment {
                                path: gz_path,
                                compressed_size_bytes: compressed_bytes,
                                ..sealed_for_notify
                            };
                            Self::notify_all(&senders, &compressed_sealed);
                        } else {
                            // Otherwise the authoritative uncompressed segment remains.
                            total_compressed_bytes.fetch_add(size_bytes, Ordering::Relaxed);
                            Self::notify_all(&senders, &sealed_for_notify);
                        }
                    }
                }
            });
            self.compression_threads.lock().push(compression_thread);
        } else {
            tracing::info!(
                "Sealed segment {}: {} events, {} bytes at {}",
                segment_number,
                event_count,
                size_bytes,
                path.display()
            );

            // Track bytes (no compression)
            self.total_compressed_bytes
                .fetch_add(size_bytes, Ordering::Relaxed);

            // Notify the indexer immediately (no compression to wait for)
            Self::notify_all(&self.sealed_senders, &sealed);
        }

        Ok(Some(sealed))
    }

    fn publish_latest_event_watermark(
        &self,
        segment_number: u64,
        sealed_at: DateTime<Utc>,
        candidate_created_at: Option<u64>,
    ) {
        let Some(path) = &self.config.latest_event_watermark_path else {
            return;
        };
        let sealed_at_epoch = u64::try_from(sealed_at.timestamp()).unwrap_or(0);
        let candidate_created_at = candidate_created_at
            .filter(|created_at| *created_at <= sealed_at_epoch)
            .filter(|created_at| *created_at <= u64::from(u32::MAX));
        let _guard = self.watermark_publication.lock();
        let result = (|| -> Result<()> {
            let previous = match path.try_exists() {
                Ok(true) => Some(
                    read_latest_event_watermark(path)
                        .map_err(|error| Error::Validation(error.to_string()))?,
                ),
                Ok(false) => None,
                Err(error) => return Err(error.into()),
            };
            let published_at_epoch = u64::try_from(Utc::now().timestamp()).unwrap_or(0);
            let watermark = LatestEventWatermark {
                schema_version: 1,
                status: "published".to_owned(),
                max_sealed_segment_number: previous.as_ref().map_or(segment_number, |state| {
                    state.max_sealed_segment_number.max(segment_number)
                }),
                max_eligible_created_at: previous
                    .as_ref()
                    .and_then(|state| state.max_eligible_created_at)
                    .into_iter()
                    .chain(candidate_created_at)
                    .max(),
                max_sealed_at_epoch: previous.as_ref().map_or(sealed_at_epoch, |state| {
                    state.max_sealed_at_epoch.max(sealed_at_epoch)
                }),
                published_at_epoch,
            };
            watermark
                .validate()
                .map_err(|error| Error::Validation(error.to_string()))?;
            pensieve_lake::write_catalog_atomically(path, &watermark)
                .map_err(|error| Error::Serialization(error.to_string()))?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                metrics::counter!("ingest_latest_event_watermark_publications_total").increment(1);
            }
            Err(error) => {
                metrics::counter!("ingest_latest_event_watermark_failures_total").increment(1);
                tracing::warn!(
                    path = %path.display(),
                    error = %compact_error(&error),
                    "failed to publish latest-event watermark; segment remains durably sealed"
                );
            }
        }
    }

    fn reap_finished_compression_threads(&self) {
        let finished = {
            let mut threads = self.compression_threads.lock();
            let mut finished = Vec::new();
            let mut index = 0;
            while index < threads.len() {
                if threads[index].is_finished() {
                    finished.push(threads.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        };

        for handle in finished {
            if handle.join().is_err() {
                tracing::warn!("segment compression thread panicked");
            }
        }
    }

    /// Wait for all compression jobs started by this writer.
    ///
    /// Call this after the final [`Self::seal`] and before reading final
    /// compression statistics or waiting for downstream consumers. The caller
    /// must ensure no concurrent writes or seals can start new jobs.
    pub fn wait_for_compression(&self) -> Result<()> {
        let handles = std::mem::take(&mut *self.compression_threads.lock());
        let mut panicked = false;

        for handle in handles {
            if handle.join().is_err() {
                panicked = true;
            }
        }

        if panicked {
            return Err(Error::Segment(
                "one or more segment compression threads panicked".to_string(),
            ));
        }

        Ok(())
    }

    fn notify_all(senders: &[Sender<SealedSegment>], sealed: &SealedSegment) {
        for sender in senders {
            if let Err(error) = sender.send(sealed.clone()) {
                tracing::warn!(
                    error = %compact_error(&error),
                    path = %sealed.path.display(),
                    "failed to send sealed segment notification"
                );
            }
        }
    }

    /// Static version of compress_file for use in background threads.
    fn compress_file_static(src: &Path, dst: &Path) -> Result<usize> {
        let temporary = dst.with_extension("gz.open");
        if temporary.exists() {
            fs::remove_file(&temporary)?;
        }
        if dst.exists() {
            return Err(Error::Segment(format!(
                "refusing to replace existing compressed segment: {}",
                dst.display()
            )));
        }

        let result = Self::compress_file_to(src, &temporary).and_then(|compressed_bytes| {
            fs::rename(&temporary, dst)?;
            let parent = dst.parent().ok_or_else(|| {
                Error::Segment("compressed segment has no parent directory".to_string())
            })?;
            File::open(parent)?.sync_all()?;
            if let Err(error) = fs::remove_file(src) {
                tracing::warn!(
                    path = %src.display(),
                    error = %compact_error(&error),
                    "compressed segment is durable but uncompressed source remains"
                );
            }
            File::open(parent)?.sync_all()?;
            Ok(compressed_bytes)
        });
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn compress_file_to(src: &Path, dst: &Path) -> Result<usize> {
        let input = File::open(src)?;
        let mut reader = BufReader::new(input);

        let output = File::create(dst)?;
        let mut encoder = GzEncoder::new(BufWriter::new(output), Compression::default());

        let mut buffer = [0u8; 64 * 1024]; // 64KB buffer
        loop {
            let bytes_read = reader.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            encoder.write_all(&buffer[..bytes_read])?;
        }

        let mut writer = encoder.finish()?;
        writer.flush()?;
        let file = writer
            .into_inner()
            .map_err(|error| Error::Segment(format!("failed to flush gzip segment: {error}")))?;
        file.sync_all()?;

        // Get compressed size
        let metadata = fs::metadata(dst)?;
        Ok(metadata.len() as usize)
    }

    /// Flush the current segment without sealing.
    pub fn flush(&self) -> Result<()> {
        let mut current = self.current.lock();
        if let Some(ref mut segment) = *current {
            segment.writer.flush()?;
        }
        Ok(())
    }

    /// Get statistics about the writer.
    pub fn stats(&self) -> SegmentStats {
        let current = self.current.lock();
        let (current_events, current_bytes) = current
            .as_ref()
            .map(|s| (s.event_count, s.size_bytes))
            .unwrap_or((0, 0));

        SegmentStats {
            segment_number: self.segment_number.load(Ordering::Relaxed),
            total_events: self.total_events.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            total_compressed_bytes: self.total_compressed_bytes.load(Ordering::Relaxed),
            current_segment_events: current_events,
            current_segment_bytes: current_bytes,
        }
    }
}

impl Drop for SegmentWriter {
    fn drop(&mut self) {
        // Seal any remaining segment on drop
        if let Err(e) = self.seal() {
            tracing::warn!(error = %compact_error(&e), "error sealing segment on drop");
        }
        if let Err(e) = self.wait_for_compression() {
            tracing::warn!(
                error = %compact_error(&e),
                "error waiting for segment compression on drop"
            );
        }
    }
}

/// Statistics about the segment writer.
#[derive(Debug, Clone)]
pub struct SegmentStats {
    /// Current segment number.
    pub segment_number: u64,

    /// Total events written across all segments.
    pub total_events: usize,

    /// Total uncompressed bytes written across all segments.
    pub total_bytes: usize,

    /// Total compressed bytes across all sealed segments (0 if compression disabled).
    pub total_compressed_bytes: usize,

    /// Events in the current (unsealed) segment.
    pub current_segment_events: usize,

    /// Bytes in the current (unsealed) segment.
    pub current_segment_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_event(n: u8) -> PackedEvent {
        test_event_at(n, u64::from(n))
    }

    fn test_event_at(n: u8, created_at: u64) -> PackedEvent {
        let mut event_id = [0u8; 32];
        event_id[0] = n;

        PackedEvent {
            event_id,
            created_at,
            data: vec![1, 2, 3, 4, 5], // Dummy notepack data
        }
    }

    fn watermark_config(tmp: &TempDir) -> (SegmentConfig, PathBuf) {
        let watermark = tmp.path().join("state/latest-event.json");
        (
            SegmentConfig {
                output_dir: tmp.path().join("segments"),
                compress: false,
                latest_event_watermark_path: Some(watermark.clone()),
                ..Default::default()
            },
            watermark,
        )
    }

    #[test]
    fn latest_event_watermark_is_canonical_and_monotonic() {
        let tmp = TempDir::new().unwrap();
        let (config, path) = watermark_config(&tmp);
        let writer = SegmentWriter::new(config, None, None).unwrap();
        let now = u64::try_from(Utc::now().timestamp()).unwrap();

        writer.write(test_event_at(1, now - 20)).unwrap();
        writer.seal().unwrap().expect("first seal");
        let first = read_latest_event_watermark(&path).unwrap();
        assert_eq!(first.max_sealed_segment_number, 0);
        assert_eq!(first.max_eligible_created_at, Some(now - 20));

        writer.write(test_event_at(2, now - 10)).unwrap();
        writer.seal().unwrap().expect("second seal");
        let second = read_latest_event_watermark(&path).unwrap();
        assert_eq!(second.max_sealed_segment_number, 1);
        assert_eq!(second.max_eligible_created_at, Some(now - 10));
        let mut canonical = serde_json::to_vec_pretty(&second).unwrap();
        canonical.push(b'\n');
        assert_eq!(fs::read(path).unwrap(), canonical);
    }

    #[test]
    fn latest_event_watermark_excludes_future_events() {
        let tmp = TempDir::new().unwrap();
        let (config, path) = watermark_config(&tmp);
        let writer = SegmentWriter::new(config, None, None).unwrap();
        let now = u64::try_from(Utc::now().timestamp()).unwrap();

        writer.write(test_event_at(1, now - 5)).unwrap();
        writer.write(test_event_at(2, now + 86_400)).unwrap();
        writer.seal().unwrap().expect("seal");

        assert_eq!(
            read_latest_event_watermark(path)
                .unwrap()
                .max_eligible_created_at,
            Some(now - 5)
        );
    }

    #[test]
    fn latest_event_watermark_never_regresses() {
        let tmp = TempDir::new().unwrap();
        let (config, path) = watermark_config(&tmp);
        let writer = SegmentWriter::new(config, None, None).unwrap();
        let now = u64::try_from(Utc::now().timestamp()).unwrap();

        writer.write(test_event_at(1, now - 1)).unwrap();
        writer.seal().unwrap().expect("first seal");
        writer.write(test_event_at(2, now - 100)).unwrap();
        writer.seal().unwrap().expect("second seal");

        assert_eq!(
            read_latest_event_watermark(path)
                .unwrap()
                .max_eligible_created_at,
            Some(now - 1)
        );
    }

    #[test]
    fn malformed_watermark_does_not_prevent_segment_seal() {
        let tmp = TempDir::new().unwrap();
        let (config, path) = watermark_config(&tmp);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}\n").unwrap();
        let writer = SegmentWriter::new(config, None, None).unwrap();

        writer.write(test_event(1)).unwrap();
        let sealed = writer.seal().unwrap().expect("seal remains authoritative");

        assert!(sealed.path.exists());
        assert!(read_latest_event_watermark(&path).is_err());
        assert_eq!(fs::read(path).unwrap(), b"{}\n");
    }

    #[test]
    fn test_write_single_event() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };

        let writer = SegmentWriter::new(config, None, None).unwrap();
        writer.write(test_event(1)).unwrap();

        let stats = writer.stats();
        assert_eq!(stats.total_events, 1);
        assert_eq!(stats.current_segment_events, 1);
    }

    #[test]
    fn test_seal_segment() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            max_segment_size: 100, // Very small for testing
            ..Default::default()
        };

        let writer = SegmentWriter::new(config, None, None).unwrap();

        // Write enough events to trigger seal
        for i in 0..20 {
            writer.write(test_event(i)).unwrap();
        }

        let stats = writer.stats();
        assert!(stats.segment_number > 0); // Should have sealed at least once
    }

    #[test]
    fn test_segment_file_created() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            compress: false, // Disable compression for simpler test
            ..Default::default()
        };

        let writer = SegmentWriter::new(config, None, None).unwrap();
        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();

        // Check file exists (9-digit format)
        let segment_path = tmp.path().join("segment-000000000.notepack");
        assert!(segment_path.exists());
    }

    #[test]
    fn taking_segment_for_seal_reserves_the_next_open_path() {
        let tmp = TempDir::new().unwrap();
        let writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: tmp.path().to_path_buf(),
                compress: false,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();

        writer.write(test_event(1)).unwrap();
        let (sealing, segment_number) = writer.take_current_for_seal().unwrap();
        assert_eq!(segment_number, 0);
        assert_eq!(
            sealing.path,
            tmp.path().join("segment-000000000.notepack.open")
        );

        // A write can arrive while the detached segment is still being flushed.
        // It must create the next path rather than truncating the sealing path.
        writer.write(test_event(2)).unwrap();
        let current = writer.current.lock();
        let current = current.as_ref().unwrap();
        assert_eq!(
            current.path,
            tmp.path().join("segment-000000001.notepack.open")
        );
        assert_ne!(current.path, sealing.path);
    }

    #[test]
    fn test_segment_file_compressed() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            compress: true,
            ..Default::default()
        };

        let writer = SegmentWriter::new(config, None, None).unwrap();
        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();
        writer.wait_for_compression().unwrap();

        let segment_path = tmp.path().join("segment-000000000.notepack.gz");
        let uncompressed_path = tmp.path().join("segment-000000000.notepack");

        // Check compressed file exists (9-digit format)
        assert!(segment_path.exists(), "Compressed file should exist");

        // Uncompressed should not exist (deleted after compression)
        assert!(
            !uncompressed_path.exists(),
            "Uncompressed file should be deleted"
        );
        assert!(writer.stats().total_compressed_bytes > 0);
    }

    #[test]
    fn drop_waits_for_background_compression() {
        let tmp = TempDir::new().unwrap();
        {
            let writer = SegmentWriter::new(
                SegmentConfig {
                    output_dir: tmp.path().to_path_buf(),
                    compress: true,
                    ..Default::default()
                },
                None,
                None,
            )
            .unwrap();
            writer.write(test_event(1)).unwrap();
        }

        assert!(tmp.path().join("segment-000000000.notepack.gz").exists());
        assert!(
            !tmp.path()
                .join("segment-000000000.notepack.gz.open")
                .exists()
        );
        assert!(!tmp.path().join("segment-000000000.notepack").exists());
    }

    #[test]
    fn test_sealed_channel_notification() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            compress: false, // Disable for simpler test
            ..Default::default()
        };

        let (sender, receiver) = crossbeam_channel::unbounded();
        let writer = SegmentWriter::new(config, Some(sender), None).unwrap();

        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();

        // Should receive notification
        let sealed = receiver.try_recv().unwrap();
        assert_eq!(sealed.event_count, 1);
        assert_eq!(sealed.segment_number, 0);
    }

    #[test]
    fn test_sealed_channel_fanout() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            compress: false,
            ..Default::default()
        };
        let (first_sender, first_receiver) = crossbeam_channel::unbounded();
        let (second_sender, second_receiver) = crossbeam_channel::unbounded();
        let writer =
            SegmentWriter::new_with_senders(config, vec![first_sender, second_sender], None)
                .unwrap();

        writer.write(test_event(1)).unwrap();
        let open_path = tmp.path().join("segment-000000000.notepack.open");
        assert!(open_path.exists());
        writer.seal().unwrap();

        assert!(!open_path.exists());
        assert!(tmp.path().join("segment-000000000.notepack").exists());
        assert_eq!(first_receiver.try_recv().unwrap().event_count, 1);
        assert_eq!(second_receiver.try_recv().unwrap().event_count, 1);
    }

    #[test]
    fn existing_zero_segment_advances_to_one() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("segment-000000000.notepack.gz"), []).unwrap();
        let writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: tmp.path().to_path_buf(),
                compress: false,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();

        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();

        assert!(tmp.path().join("segment-000000001.notepack").exists());
        assert!(tmp.path().join("segment-000000000.notepack.gz").exists());
    }
}
