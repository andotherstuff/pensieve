//! Protobuf event source adapter.
//!
//! Reads Nostr events from length-delimited protobuf files (optionally gzipped),
//! validates each event's ID and signature, and emits packed events
//! ready for the ingestion pipeline.

use super::{EventSource, SourceMetadata, SourceStats};
use crate::pipeline::PackedEvent;
use crate::{Error, Result};
use flate2::read::GzDecoder;
use pensieve_core::{decode_length_delimited_with_size, pack_event_binary_into, validate_proto_event};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::PathBuf;

/// Configuration for the Protobuf source.
#[derive(Debug, Clone)]
pub struct ProtoConfig {
    /// Input file or directory path.
    pub input: PathBuf,

    /// Force gzip decompression (auto-detected from .gz extension).
    pub gzip: bool,

    /// Skip signature/ID validation.
    pub skip_validation: bool,

    /// Continue processing on errors.
    pub continue_on_error: bool,

    /// Limit number of files to process.
    pub limit: Option<usize>,

    /// Progress reporting interval (events).
    pub progress_interval: usize,
}

impl Default for ProtoConfig {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            gzip: false,
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100_000,
        }
    }
}

/// Protobuf file event source.
pub struct ProtoSource {
    config: ProtoConfig,
}

impl ProtoSource {
    /// Create a new Protobuf source with the given configuration.
    pub fn new(config: ProtoConfig) -> Self {
        Self { config }
    }

    /// Get the configuration.
    pub fn config(&self) -> &ProtoConfig {
        &self.config
    }

    /// Collect files to process based on input path.
    fn collect_files(&self) -> Result<Vec<PathBuf>> {
        let input = &self.config.input;
        let mut files = Vec::new();

        if input.is_file() {
            files.push(input.clone());
        } else if input.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(input)?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|ext| ext == "pb" || ext == "gz")
                })
                .map(|e| e.path())
                .collect();

            // Sort for deterministic processing order
            entries.sort();
            files = entries;
        } else {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Input path does not exist: {}", input.display()),
            )));
        }

        // Apply limit if specified
        if let Some(limit) = self.config.limit {
            files.truncate(limit);
        }

        Ok(files)
    }

    /// Process a single protobuf file.
    fn process_file<F>(
        &self,
        file_path: &PathBuf,
        handler: &mut F,
        stats: &mut ProtoStats,
    ) -> Result<bool>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        let file = File::open(file_path)?;
        let file_size = file.metadata()?.len() as usize;
        stats.compressed_proto_bytes += file_size;

        let is_gzip = self.config.gzip || file_path.extension().is_some_and(|ext| ext == "gz");

        // Create reader (with optional gzip decompression)
        let mut reader: Box<dyn Read> = if is_gzip {
            Box::new(BufReader::new(GzDecoder::new(BufReader::new(file))))
        } else {
            Box::new(BufReader::new(file))
        };

        // Reusable buffer for notepack encoding
        let mut pack_buf: Vec<u8> = Vec::with_capacity(4096);

        loop {
            stats.total_events += 1;

            // Read the next length-delimited protobuf event
            let (proto_event, proto_bytes) = match decode_length_delimited_with_size(&mut reader) {
                Ok(Some((event, bytes))) => (event, bytes),
                Ok(None) => {
                    stats.total_events -= 1; // Don't count EOF as an event
                    break;
                }
                Err(e) => {
                    tracing::warn!("Protobuf decode error: {}", e);
                    stats.invalid_events += 1;
                    stats.proto_errors += 1;
                    if self.config.continue_on_error {
                        break;
                    } else {
                        return Err(Error::Protobuf(e.to_string()));
                    }
                }
            };

            stats.decompressed_proto_bytes += proto_bytes;
            pack_buf.clear();

            // Validate and pack
            let event_id = if self.config.skip_validation {
                // Just pack without full validation
                match proto_to_notepack_unvalidated(&proto_event, &mut pack_buf) {
                    Ok(_) => hex_to_bytes32(&proto_event.id)?,
                    Err(e) => {
                        tracing::warn!("Notepack encoding error: {}", e);
                        stats.invalid_events += 1;
                        stats.proto_errors += 1;
                        if self.config.continue_on_error {
                            continue;
                        } else {
                            return Err(Error::Notepack(e.to_string()));
                        }
                    }
                }
            } else {
                match validate_proto_event(&proto_event) {
                    Ok(event) => {
                        let id_bytes: [u8; 32] = *event.id.as_bytes();
                        pack_event_binary_into(&event, &mut pack_buf);
                        id_bytes
                    }
                    Err(e) => {
                        tracing::warn!("Validation error: {}", e);
                        stats.invalid_events += 1;
                        stats.validation_errors += 1;
                        if self.config.continue_on_error {
                            continue;
                        } else {
                            return Err(Error::Validation(e.to_string()));
                        }
                    }
                }
            };

            stats.valid_events += 1;

            // Call the handler
            let packed_event = PackedEvent {
                event_id,
                data: pack_buf.clone(),
            };

            match handler(packed_event) {
                Ok(true) => {} // Continue
                Ok(false) => {
                    tracing::info!("Handler signaled stop");
                    return Ok(false);
                }
                Err(e) => {
                    if self.config.continue_on_error {
                        tracing::warn!("Handler error: {}", e);
                    } else {
                        return Err(e);
                    }
                }
            }

            // Progress reporting
            if stats.total_events.is_multiple_of(self.config.progress_interval) {
                tracing::info!(
                    "Progress: {} events, {} valid, {} invalid",
                    stats.total_events, stats.valid_events, stats.invalid_events
                );
            }
        }

        Ok(true)
    }
}

impl EventSource for ProtoSource {
    fn name(&self) -> &'static str {
        "proto"
    }

    fn process<F>(&mut self, mut handler: F) -> Result<SourceStats>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        let mut stats = ProtoStats::default();

        // Collect input files
        let files = self.collect_files()?;
        tracing::info!("Found {} protobuf files to process", files.len());

        for (file_idx, file_path) in files.iter().enumerate() {
            tracing::info!(
                "[{}/{}] Processing: {}",
                file_idx + 1,
                files.len(),
                file_path.display()
            );

            match self.process_file(file_path, &mut handler, &mut stats) {
                Ok(true) => {
                    stats.files_processed += 1;
                }
                Ok(false) => {
                    // Handler signaled stop
                    stats.files_processed += 1;
                    break;
                }
                Err(e) => {
                    tracing::warn!("Error processing {}: {}", file_path.display(), e);
                    if !self.config.continue_on_error {
                        return Err(e);
                    }
                }
            }
        }

        Ok(SourceStats {
            total_events: stats.total_events,
            valid_events: stats.valid_events,
            invalid_events: stats.invalid_events,
            validation_errors: stats.validation_errors,
            parse_errors: stats.proto_errors,
            source_metadata: SourceMetadata {
                files_processed: Some(stats.files_processed),
                bytes_read: Some(stats.decompressed_proto_bytes),
                ..Default::default()
            },
        })
    }
}

/// Internal statistics for protobuf processing.
#[derive(Default)]
struct ProtoStats {
    files_processed: usize,
    total_events: usize,
    valid_events: usize,
    invalid_events: usize,
    proto_errors: usize,
    validation_errors: usize,
    compressed_proto_bytes: usize,
    decompressed_proto_bytes: usize,
}

fn hex_to_bytes32(hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex).map_err(|e| Error::Hex(e.to_string()))?;
    if bytes.len() != 32 {
        return Err(Error::Hex(format!("Expected 32 bytes, got {}", bytes.len())));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

/// Pack a protobuf event to notepack without full validation.
/// This is faster but should only be used when you trust the data source.
fn proto_to_notepack_unvalidated(
    proto: &pensieve_core::ProtoEvent,
    buf: &mut Vec<u8>,
) -> std::result::Result<usize, String> {
    use notepack::{pack_note_into, NoteBuf};

    // Convert proto tags to notepack tags (Vec<Vec<String>>)
    let tags: Vec<Vec<String>> = proto.tags.iter().map(|t| t.values.clone()).collect();

    let note = NoteBuf {
        id: proto.id.clone(),
        pubkey: proto.pubkey.clone(),
        created_at: proto.created_at as u64,
        kind: proto.kind as u64,
        tags,
        content: proto.content.clone(),
        sig: proto.sig.clone(),
    };

    pack_note_into(&note, buf).map_err(|e| e.to_string())
}
