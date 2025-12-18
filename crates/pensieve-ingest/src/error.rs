//! Error types for the ingestion pipeline.

use thiserror::Error;

/// Result type alias using the crate's error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during ingestion.
#[derive(Error, Debug)]
pub enum Error {
    /// RocksDB error.
    #[error("RocksDB error: {0}")]
    RocksDb(#[from] rocksdb::Error),

    /// ClickHouse error.
    #[error("ClickHouse error: {0}")]
    ClickHouse(#[from] clickhouse::error::Error),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Nostr SDK error.
    #[error("Nostr SDK error: {0}")]
    NostrSdk(#[from] nostr_sdk::client::Error),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Segment error.
    #[error("Segment error: {0}")]
    Segment(String),

    /// Channel send error.
    #[error("Channel send error: {0}")]
    ChannelSend(String),

    /// Channel receive error.
    #[error("Channel receive error")]
    ChannelRecv,

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// JSON parsing error.
    #[error("JSON error: {0}")]
    Json(String),

    /// Event validation error.
    #[error("Validation error: {0}")]
    Validation(String),

    /// Notepack encoding error.
    #[error("Notepack error: {0}")]
    Notepack(String),

    /// Protobuf decoding error.
    #[error("Protobuf error: {0}")]
    Protobuf(String),

    /// Hex decoding error.
    #[error("Hex decoding error: {0}")]
    Hex(String),
}
