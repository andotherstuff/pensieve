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
//! 1. The segment file is closed
//! 2. (Future) Upload to S3
//! 3. Notify ClickHouse indexer via callback
//! 4. Start a new segment

use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;
use flate2::write::GzEncoder;
use flate2::Compression;
use parking_lot::Mutex;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tracing::{debug, info, warn};

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
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("./segments"),
            max_segment_size: 256 * 1024 * 1024, // 256 MB
            segment_prefix: "segment".to_string(),
            compress: true,
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

    /// The notepack-encoded bytes (without length prefix).
    pub data: Vec<u8>,
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
    total_compressed_bytes: AtomicUsize,
    sealed_sender: Option<Sender<SealedSegment>>,
}

impl SegmentWriter {
    /// Create a new segment writer.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the writer
    /// * `sealed_sender` - Optional channel to send sealed segment notifications
    pub fn new(config: SegmentConfig, sealed_sender: Option<Sender<SealedSegment>>) -> Result<Self> {
        // Create output directory if it doesn't exist
        fs::create_dir_all(&config.output_dir)?;

        // Find the next segment number by scanning existing files
        let next_segment = Self::find_next_segment_number(&config)?;

        info!(
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
            total_compressed_bytes: AtomicUsize::new(0),
            sealed_sender,
        })
    }

    /// Find the next segment number by scanning existing files.
    fn find_next_segment_number(config: &SegmentConfig) -> Result<u64> {
        let mut max_num = 0u64;

        if config.output_dir.exists() {
            for entry in fs::read_dir(&config.output_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let name_str = name.to_string_lossy();

                // Parse segment-NNNNNNNNN.notepack or segment-NNNNNNNNN.notepack.gz
                if let Some(rest) = name_str.strip_prefix(&format!("{}-", config.segment_prefix)) {
                    let num_str = rest
                        .strip_suffix(".notepack.gz")
                        .or_else(|| rest.strip_suffix(".notepack"));

                    if let Some(num_str) = num_str
                        && let Ok(num) = num_str.parse::<u64>()
                    {
                        max_num = max_num.max(num);
                    }
                }
            }
        }

        // Start at the next number after the highest found
        Ok(if max_num > 0 { max_num + 1 } else { 0 })
    }

    /// Generate the path for a segment file.
    ///
    /// Uses 9-digit zero-padded numbering for lexicographic sorting
    /// and headroom up to ~1 billion segments (~256 PB at 256MB each).
    fn segment_path(&self, segment_number: u64) -> PathBuf {
        self.config.output_dir.join(format!(
            "{}-{:09}.notepack",
            self.config.segment_prefix, segment_number
        ))
    }

    /// Get or create the current segment.
    fn ensure_current_segment(&self) -> Result<()> {
        let mut current = self.current.lock();

        if current.is_none() {
            let segment_num = self.segment_number.load(Ordering::SeqCst);
            let path = self.segment_path(segment_num);

            debug!("Creating new segment: {}", path.display());

            let file = File::create(&path)?;
            let writer = BufWriter::with_capacity(8 * 1024 * 1024, file); // 8MB buffer

            *current = Some(CurrentSegment {
                writer,
                path,
                event_count: 0,
                size_bytes: 0,
                event_ids: Vec::with_capacity(10000),
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
        let segment = current.as_mut().ok_or_else(|| {
            Error::Segment("No current segment".to_string())
        })?;

        // Write length-prefixed format: [u32 length][notepack bytes]
        let len_bytes = (event.data.len() as u32).to_le_bytes();
        segment.writer.write_all(&len_bytes)?;
        segment.writer.write_all(&event.data)?;

        // Update stats
        let written_bytes = 4 + event.data.len();
        segment.event_count += 1;
        segment.size_bytes += written_bytes;
        segment.event_ids.push(event.event_id);

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

    /// Seal the current segment.
    ///
    /// This finalizes the current segment and prepares for a new one.
    /// If compression is enabled, the segment is gzipped and renamed to `.notepack.gz`.
    pub fn seal(&self) -> Result<Option<SealedSegment>> {
        let mut current = self.current.lock();

        let segment = match current.take() {
            Some(s) => s,
            None => return Ok(None),
        };

        // Flush and close the writer
        let CurrentSegment {
            mut writer,
            path,
            event_count,
            size_bytes,
            event_ids,
        } = segment;

        writer.flush()?;
        drop(writer);

        let segment_number = self.segment_number.fetch_add(1, Ordering::SeqCst);

        // Compress if enabled
        let (final_path, compressed_size) = if self.config.compress {
            let gz_path = path.with_extension("notepack.gz");
            let compressed_bytes = self.compress_file(&path, &gz_path)?;

            // Remove the uncompressed file
            if let Err(e) = fs::remove_file(&path) {
                warn!("Failed to remove uncompressed segment: {}", e);
            }

            info!(
                "Sealed segment {}: {} events, {} bytes -> {} bytes ({:.1}%) at {}",
                segment_number,
                event_count,
                size_bytes,
                compressed_bytes,
                (compressed_bytes as f64 / size_bytes as f64) * 100.0,
                gz_path.display()
            );

            (gz_path, compressed_bytes)
        } else {
            info!(
                "Sealed segment {}: {} events, {} bytes at {}",
                segment_number,
                event_count,
                size_bytes,
                path.display()
            );
            (path, size_bytes)
        };

        // Track compressed bytes
        self.total_compressed_bytes
            .fetch_add(compressed_size, Ordering::Relaxed);

        let sealed = SealedSegment {
            path: final_path,
            segment_number,
            event_count,
            size_bytes,
            compressed_size_bytes: compressed_size,
            event_ids,
            sealed_at: Utc::now(),
        };

        // Notify the indexer (if channel is configured)
        if let Some(sender) = &self.sealed_sender
            && let Err(e) = sender.send(sealed.clone())
        {
            warn!("Failed to send sealed segment notification: {}", e);
        }

        Ok(Some(sealed))
    }

    /// Compress a file with gzip, returning the compressed size.
    fn compress_file(&self, src: &PathBuf, dst: &PathBuf) -> Result<usize> {
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

        encoder.finish()?.flush()?;

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
            warn!("Error sealing segment on drop: {}", e);
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
        let mut event_id = [0u8; 32];
        event_id[0] = n;

        PackedEvent {
            event_id,
            data: vec![1, 2, 3, 4, 5], // Dummy notepack data
        }
    }

    #[test]
    fn test_write_single_event() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };

        let writer = SegmentWriter::new(config, None).unwrap();
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

        let writer = SegmentWriter::new(config, None).unwrap();

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

        let writer = SegmentWriter::new(config, None).unwrap();
        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();

        // Check file exists (9-digit format)
        let segment_path = tmp.path().join("segment-000000000.notepack");
        assert!(segment_path.exists());
    }

    #[test]
    fn test_segment_file_compressed() {
        let tmp = TempDir::new().unwrap();
        let config = SegmentConfig {
            output_dir: tmp.path().to_path_buf(),
            compress: true,
            ..Default::default()
        };

        let writer = SegmentWriter::new(config, None).unwrap();
        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();

        // Check compressed file exists (9-digit format)
        let segment_path = tmp.path().join("segment-000000000.notepack.gz");
        assert!(segment_path.exists());

        // Uncompressed should not exist
        let uncompressed_path = tmp.path().join("segment-000000000.notepack");
        assert!(!uncompressed_path.exists());
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
        let writer = SegmentWriter::new(config, Some(sender)).unwrap();

        writer.write(test_event(1)).unwrap();
        writer.seal().unwrap();

        // Should receive notification
        let sealed = receiver.try_recv().unwrap();
        assert_eq!(sealed.event_count, 1);
        assert_eq!(sealed.segment_number, 0);
    }
}

