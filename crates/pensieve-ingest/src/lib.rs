//! Pensieve ingestion pipeline components.
//!
//! This crate provides the core pipeline for ingesting Nostr events from
//! various sources into the Pensieve archive.
//!
//! # Modules
//!
//! - [`pipeline`] - Core pipeline components (dedupe, segment writer, ClickHouse indexer)
//! - [`source`] - Event source adapters (JSONL, Protobuf, live relays)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐
//! │  Event Sources  │  (JSONL files, Protobuf archives, live relays)
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │   DedupeIndex   │  RocksDB - tracks seen event IDs
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │  SegmentWriter  │  Writes notepack segments, seals on size threshold
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │ClickHouseIndexer│  Derived index for analytics queries
//! └─────────────────┘
//! ```
//!
//! The pipeline is archive-first: the notepack archive is the source of truth,
//! and ClickHouse is a derived index.

pub mod error;
pub mod pipeline;
pub mod relay;
pub mod source;

// Re-export commonly used types at crate root
pub use error::{Error, Result};

// Re-export pipeline components for convenience
pub use pipeline::{
    ClickHouseConfig, ClickHouseIndexer, DedupeIndex, DedupeStats, EventStatus, IndexerStats,
    PackedEvent, SealedSegment, SegmentConfig, SegmentStats, SegmentWriter,
};

// Re-export source trait and adapters
pub use source::{
    EventSource, JsonlConfig, JsonlSource, ProtoConfig, ProtoSource, RelayConfig, RelaySource,
    SourceMetadata, SourceStats,
};

// Re-export relay manager types
pub use relay::{
    AggregateRelayStats, OptimizationSuggestions, RelayManager, RelayManagerConfig, RelayStatus,
    RelayTier,
};
