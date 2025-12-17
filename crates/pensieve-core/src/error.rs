//! Error types for the Pensieve ingestion pipeline.

use thiserror::Error;

/// Result type alias using the crate's error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during event processing.
#[derive(Error, Debug)]
pub enum Error {
    /// Event ID validation failed - computed ID doesn't match claimed ID.
    #[error("invalid event ID: computed {computed}, expected {expected}")]
    InvalidEventId {
        /// The ID we computed by hashing the event.
        computed: String,
        /// The ID claimed in the event.
        expected: String,
    },

    /// Event signature is invalid.
    #[error("invalid event signature: {0}")]
    InvalidSignature(String),

    /// Event has an invalid field format (e.g., wrong hex length).
    #[error("invalid field '{field}': {reason}")]
    InvalidField {
        /// The name of the invalid field.
        field: &'static str,
        /// Description of what's wrong.
        reason: String,
    },

    /// JSON parsing error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Notepack encoding/decoding error.
    #[error("notepack error: {0}")]
    Notepack(#[from] notepack::Error),

    /// Nostr library error (for crypto operations).
    #[error("nostr error: {0}")]
    Nostr(#[from] nostr::event::Error),

    /// Hex decoding error.
    #[error("hex decode error: {0}")]
    HexDecode(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Protobuf decoding error.
    #[error("protobuf decode error: {0}")]
    ProtobufDecode(String),
}

