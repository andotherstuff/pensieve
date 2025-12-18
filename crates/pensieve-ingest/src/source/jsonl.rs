//! JSONL event source adapter.
//!
//! Reads Nostr events from JSONL files (one JSON event per line),
//! validates each event's ID and signature, and emits packed events
//! ready for the ingestion pipeline.

use super::{EventSource, SourceMetadata, SourceStats};
use crate::pipeline::PackedEvent;
use crate::{Error, Result};
use notepack::{pack_note_into, NoteBuf};
use pensieve_core::{pack_event_binary_into, validate_event};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// Configuration for the JSONL source.
#[derive(Debug, Clone)]
pub struct JsonlConfig {
    /// Input file or directory path.
    pub input: PathBuf,

    /// Skip signature/ID validation (faster but unsafe).
    pub skip_validation: bool,

    /// Continue processing on errors (log and skip invalid events).
    pub continue_on_error: bool,

    /// Limit number of files to process (for testing).
    pub limit: Option<usize>,

    /// Progress reporting interval (events).
    pub progress_interval: usize,
}

impl Default for JsonlConfig {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            skip_validation: false,
            continue_on_error: true,
            limit: None,
            progress_interval: 100_000,
        }
    }
}

/// JSONL file event source.
pub struct JsonlSource {
    config: JsonlConfig,
}

impl JsonlSource {
    /// Create a new JSONL source with the given configuration.
    pub fn new(config: JsonlConfig) -> Self {
        Self { config }
    }

    /// Get the configuration.
    pub fn config(&self) -> &JsonlConfig {
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
                            .is_some_and(|ext| ext == "jsonl" || ext == "json" || ext == "ndjson")
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

    /// Process a single JSONL file.
    fn process_file<F>(
        &self,
        file_path: &PathBuf,
        handler: &mut F,
        stats: &mut JsonlStats,
    ) -> Result<bool>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        // Reusable buffer for notepack encoding
        let mut pack_buf: Vec<u8> = Vec::with_capacity(4096);

        for (line_num, line_result) in reader.lines().enumerate() {
            stats.total_lines += 1;

            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("Line {}: I/O error: {}", line_num + 1, e);
                    stats.invalid_events += 1;
                    stats.json_errors += 1;
                    if self.config.continue_on_error {
                        continue;
                    } else {
                        return Err(Error::Io(e));
                    }
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            stats.total_events += 1;
            pack_buf.clear();

            // Parse and validate
            let event_id = if self.config.skip_validation {
                match serde_json::from_str::<NoteBuf>(&line) {
                    Ok(note) => {
                        if let Err(e) = pack_note_into(&note, &mut pack_buf) {
                            tracing::warn!("Line {}: Notepack encoding error: {}", line_num + 1, e);
                            stats.invalid_events += 1;
                            stats.notepack_errors += 1;
                            if self.config.continue_on_error {
                                continue;
                            } else {
                                return Err(Error::Notepack(e.to_string()));
                            }
                        }
                        // Parse event ID from the note
                        hex_to_bytes32(&note.id)?
                    }
                    Err(e) => {
                        tracing::warn!("Line {}: JSON parse error: {}", line_num + 1, e);
                        stats.invalid_events += 1;
                        stats.json_errors += 1;
                        if self.config.continue_on_error {
                            continue;
                        } else {
                            return Err(Error::Json(e.to_string()));
                        }
                    }
                }
            } else {
                match validate_event(&line) {
                    Ok(event) => {
                        let id_bytes: [u8; 32] = *event.id.as_bytes();
                        pack_event_binary_into(&event, &mut pack_buf);
                        id_bytes
                    }
                    Err(e) => {
                        tracing::warn!("Line {}: Validation error: {}", line_num + 1, e);
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

impl EventSource for JsonlSource {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn process<F>(&mut self, mut handler: F) -> Result<SourceStats>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        let mut stats = JsonlStats::default();

        // Collect input files
        let files = self.collect_files()?;
        tracing::info!("Found {} JSONL files to process", files.len());

        for (file_idx, file_path) in files.iter().enumerate() {
            tracing::info!(
                "[{}/{}] Processing: {}",
                file_idx + 1,
                files.len(),
                file_path.display()
            );

            let file_size = fs::metadata(file_path)?.len() as usize;
            stats.total_json_bytes += file_size;

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
            parse_errors: stats.json_errors,
            source_metadata: SourceMetadata {
                files_processed: Some(stats.files_processed),
                bytes_read: Some(stats.total_json_bytes),
                ..Default::default()
            },
        })
    }
}

/// Internal statistics for JSONL processing.
#[derive(Default)]
struct JsonlStats {
    files_processed: usize,
    total_lines: usize,
    total_events: usize,
    valid_events: usize,
    invalid_events: usize,
    json_errors: usize,
    validation_errors: usize,
    notepack_errors: usize,
    total_json_bytes: usize,
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
