//! Pensieve ingestion pipeline components.
//!
//! This crate provides the core pipeline for ingesting Nostr events:
//!
//! - [`dedupe::DedupeIndex`] - RocksDB-backed deduplication index
//! - [`segment::SegmentWriter`] - Writes events to notepack segments
//! - [`clickhouse::ClickHouseIndexer`] - Indexes events into ClickHouse
//!
//! # Architecture
//!
//! ```text
//! [Source Adapters] → [DedupeIndex] → [SegmentWriter] → [ClickHouseIndexer]
//!                          ↓                ↓
//!                      RocksDB           S3 Upload
//! ```
//!
//! The pipeline is archive-first: the notepack archive is the source of truth,
//! and ClickHouse is a derived index.

pub mod clickhouse;
pub mod dedupe;
pub mod error;
pub mod segment;

pub use clickhouse::ClickHouseIndexer;
pub use dedupe::DedupeIndex;
pub use error::{Error, Result};
pub use segment::SegmentWriter;

