//! Core types, validation, and shared utilities for the Pensieve ingestion pipeline.
//!
//! This crate provides:
//! - Event validation (ID and signature verification per NIP-01) via the nostr crate
//! - Serialization between JSON and notepack formats
//! - Protobuf types and conversion utilities
//! - Prometheus metrics helpers
//! - Shared error types

mod error;
mod event;
pub mod metrics;
pub mod proto;

// ═══════════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════════

/// Nostr genesis date: November 7, 2020.
/// This is the date of the first Nostr commit. Events with `created_at` before
/// this date are considered invalid/bogus.
pub const NOSTR_GENESIS_TIMESTAMP: u32 = 1604707200; // 2020-11-07 00:00:00 UTC

/// Nostr genesis date as a string for SQL queries.
pub const NOSTR_GENESIS_DATE_SQL: &str = "2020-11-07 00:00:00";

pub use error::{Error, Result};
pub use event::{
    event_to_notebuf, pack_event_binary, pack_event_binary_into, validate_event, validate_event_id,
    validate_event_signature, validate_notebuf,
};
pub use proto::{
    EventBatch, ProtoEvent, Tag, decode_length_delimited, decode_length_delimited_with_size,
    decode_proto_event, proto_to_json, validate_proto_event,
};
