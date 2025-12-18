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

mod clickhouse;
mod dedupe;
mod segment;

pub use clickhouse::{ClickHouseConfig, ClickHouseIndexer, EventRow, IndexerStats};
pub use dedupe::{DedupeIndex, DedupeStats, EventStatus};
pub use segment::{
    PackedEvent, SealedSegment, SegmentConfig, SegmentStats, SegmentWriter, pack_nostr_event,
};
