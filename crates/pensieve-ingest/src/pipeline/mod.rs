//! Core pipeline components for event ingestion.
//!
//! This module provides the shared pipeline that all event sources feed into:
//!
//! - [`DedupeIndex`] - RocksDB-backed deduplication by event ID
//! - [`SegmentWriter`] - Writes events to length-prefixed notepack segments
//! - [`ClickHouseIndexer`] - Indexes sealed segments into ClickHouse
//!
//! # Architecture
//!
//! ```text
//! [EventSource] → [DedupeIndex] → [SegmentWriter] → [ClickHouseIndexer]
//!                      ↓                ↓
//!                  RocksDB          S3 Upload
//! ```
//!
//! The pipeline is archive-first: the notepack archive is the source of truth,
//! and ClickHouse is a derived index.

mod admission;
mod clickhouse;
mod coverage;
mod dedupe;
mod parquet_shadow;
mod seal_timer;
mod segment;

pub use admission::validate_archive_event;
pub use clickhouse::{ClickHouseConfig, ClickHouseIndexer, EventRow, IndexerStats};
pub use coverage::CoverageSampler;
pub use dedupe::{DedupeIndex, DedupeStats, EventStatus, PendingAdmission};
pub use parquet_shadow::{ParquetShadowConfig, ParquetShadowPublisher, start_parquet_shadow};
pub use seal_timer::ArchiveSealTimer;
pub use segment::{
    LatestEventWatermark, PackedEvent, SealedSegment, SegmentConfig, SegmentStats, SegmentWriter,
    pack_nostr_event, read_latest_event_watermark,
};
