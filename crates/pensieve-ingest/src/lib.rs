//! Pensieve ingestion pipeline components.
//!
//! This crate provides the core pipeline for ingesting Nostr events from
//! various sources into the Pensieve archive.
//!
//! # Modules
//!
//! - [`pipeline`] - Core pipeline components (dedupe, segment writer, ClickHouse indexer)
//! - [`source`] - Event source adapters (JSONL, Protobuf, live relays)
//! - [`sync`] - Negentropy sync for NIP-77 reconciliation with trusted relays
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐       ┌─────────────────┐
//! │  Event Sources  │       │ Negentropy Sync │
//! │ (JSONL, relays) │       │ (NIP-77, periodic)
//! └────────┬────────┘       └────────┬────────┘
//!          │                         │
//!          └──────────┬──────────────┘
//!                     │
//!                     ▼
//!          ┌─────────────────┐
//!          │   DedupeIndex   │  RocksDB - tracks seen event IDs
//!          └────────┬────────┘
//!                   │
//!                   ▼
//!          ┌─────────────────┐
//!          │  SegmentWriter  │  Writes notepack segments, seals on size threshold
//!          └────────┬────────┘
//!                   │
//!                   ▼
//!          ┌─────────────────┐
//!          │ClickHouseIndexer│  Derived index for analytics queries
//!          └─────────────────┘
//! ```
//!
//! The pipeline is archive-first: the notepack archive is the source of truth,
//! and ClickHouse is a derived index.

pub mod error;
pub mod logging;
pub mod pipeline;
pub mod relay;
pub mod source;
pub mod sync;

// Re-export commonly used types at crate root
pub use error::{Error, Result};

// Re-export pipeline components for convenience
pub use pipeline::{
    ClickHouseConfig, ClickHouseIndexer, CoverageSampler, DedupeIndex, DedupeStats, EventStatus,
    IndexerStats, PackedEvent, SealedSegment, SegmentConfig, SegmentStats, SegmentWriter,
    pack_nostr_event,
};

// Re-export source trait and adapters
pub use source::{
    EventSource, JsonlConfig, JsonlSource, ProtoConfig, ProtoSource, RelayConfig, RelaySource,
    SourceMetadata, SourceStats,
};

// Re-export relay manager types
pub use relay::{
    AggregateRelayStats, ConnectionGuard, ConnectionGuardConfig, KIND_RELAY_DISCOVERY,
    OptimizationSuggestions, RelayCatalogEntry, RelayManager, RelayManagerConfig, RelayStatus,
    RelayTier, parse_relay_discovery,
};

// Re-export sync types
pub use sync::{
    NegentropySyncConfig, NegentropySyncer, SyncStateDb, SyncStats, seed_from_clickhouse,
};
