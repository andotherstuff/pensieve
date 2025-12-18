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

pub use error::{Error, Result};
pub use event::{
    event_to_notebuf, pack_event_binary, pack_event_binary_into, validate_event, validate_event_id,
    validate_event_signature, validate_notebuf,
};
pub use proto::{
    EventBatch, ProtoEvent, Tag, decode_length_delimited, decode_length_delimited_with_size,
    decode_proto_event, proto_to_json, validate_proto_event,
};
