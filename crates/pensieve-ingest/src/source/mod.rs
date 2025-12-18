//! Event source adapters.
//!
//! This module provides adapters for different event sources that feed into
//! the ingestion pipeline. Each source produces validated, packed events
//! ready for deduplication and archival.
//!
//! # Available Sources
//!
//! - [`JsonlSource`] - Reads JSONL files (one JSON event per line)
//! - [`ProtoSource`] - Reads length-delimited protobuf files
//! - [`RelaySource`] - Connects to Nostr relays for live event streaming
//!
//! # Architecture
//!
//! All sources implement the [`EventSource`] trait, which provides a uniform
//! interface for the pipeline to consume events regardless of their origin.

mod jsonl;
mod proto;
mod relay;

pub use jsonl::{JsonlConfig, JsonlSource};
pub use proto::{ProtoConfig, ProtoSource};
pub use relay::{RelayConfig, RelaySource};

use crate::pipeline::PackedEvent;
use crate::Result;

/// A source of Nostr events.
///
/// Event sources are responsible for:
/// 1. Reading/receiving raw events from their underlying source
/// 2. Validating event signatures and IDs (per NIP-01)
/// 3. Packing events into notepack format
///
/// The pipeline then handles deduplication, archival, and indexing.
pub trait EventSource {
    /// Human-readable name for this source (used in logs and metrics).
    fn name(&self) -> &'static str;

    /// Process events from this source, calling the handler for each valid event.
    ///
    /// The handler receives packed events ready for the pipeline. It returns
    /// `Ok(true)` to continue processing, `Ok(false)` to stop gracefully,
    /// or `Err` to abort with an error.
    ///
    /// # Arguments
    ///
    /// * `handler` - Callback invoked for each valid, packed event
    ///
    /// # Returns
    ///
    /// Statistics about the processing run.
    fn process<F>(&mut self, handler: F) -> Result<SourceStats>
    where
        F: FnMut(PackedEvent) -> Result<bool>;
}

/// Statistics from processing an event source.
#[derive(Debug, Clone, Default)]
pub struct SourceStats {
    /// Total events encountered (before validation/dedup).
    pub total_events: usize,

    /// Events that passed validation.
    pub valid_events: usize,

    /// Events that failed validation.
    pub invalid_events: usize,

    /// Breakdown of invalid events by error type.
    pub validation_errors: usize,
    pub parse_errors: usize,

    /// Source-specific metadata (e.g., files processed, relays connected).
    pub source_metadata: SourceMetadata,
}

/// Source-specific metadata.
#[derive(Debug, Clone, Default)]
pub struct SourceMetadata {
    /// For file-based sources: number of files processed.
    pub files_processed: Option<usize>,

    /// For file-based sources: total bytes read.
    pub bytes_read: Option<usize>,

    /// For relay sources: number of relays connected.
    pub relays_connected: Option<usize>,

    /// For relay sources: number of relays discovered.
    pub relays_discovered: Option<usize>,
}

