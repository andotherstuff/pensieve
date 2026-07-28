//! Errors returned by canonical archive preparation, writing, and validation.

use thiserror::Error;

/// Result type for canonical Parquet archive operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned while validating, preparing, or writing archive rows.
#[derive(Debug, Error)]
pub enum Error {
    /// A public key is not a valid x-only secp256k1 public key.
    #[error("event {id} has an invalid public key")]
    InvalidPublicKey {
        /// Claimed event ID.
        id: String,
    },

    /// An event ID does not match its ID-committed fields.
    #[error("event {id} has an invalid ID")]
    InvalidEventId {
        /// Claimed event ID.
        id: String,
    },

    /// An event signature does not verify.
    #[error("event {id} has an invalid signature")]
    InvalidSignature {
        /// Claimed event ID.
        id: String,
    },

    /// An event contained a zero-element tag, which V1 does not permit.
    #[error("event {id} contains an empty tag at index {tag_index}")]
    EmptyTag {
        /// Event ID.
        id: String,
        /// Zero-based tag position.
        tag_index: usize,
    },

    /// A notepack event kind cannot be represented by the V1 schema.
    #[error("notepack event kind {kind} exceeds the V1 unsigned 16-bit range")]
    KindOutOfRange {
        /// Parsed notepack kind.
        kind: u64,
    },

    /// A framed notepack payload exceeds the configured safety limit.
    #[error("notepack frame length {length} exceeds the configured limit {limit}")]
    FrameTooLarge {
        /// Claimed payload length.
        length: usize,
        /// Configured maximum payload length.
        limit: usize,
    },

    /// A length-prefixed segment ended partway through a frame.
    #[error("notepack segment is truncated in frame {frame_index}")]
    TruncatedFrame {
        /// Zero-based frame index.
        frame_index: usize,
    },

    /// A salvage operation was requested for a structurally complete segment.
    #[error("notepack segment is complete; refusing to create a salvage artifact")]
    SegmentNotTruncated,

    /// A salvage bundle or its requested destination is invalid.
    #[error("invalid notepack salvage: {0}")]
    InvalidSalvage(String),

    /// A framed notepack payload failed decoding or canonical validation.
    #[error("notepack frame {frame_index} failed validation: {source}")]
    FrameValidation {
        /// Zero-based frame index.
        frame_index: usize,
        /// Underlying decoding or canonical validation error.
        #[source]
        source: Box<Error>,
    },

    /// Atomic publication does not overwrite an existing canonical file.
    #[error("output file already exists: {path}")]
    OutputExists {
        /// Existing output path.
        path: std::path::PathBuf,
    },

    /// The caller attempted to seal a file without rows.
    #[error("cannot write an empty canonical archive file")]
    EmptyBatch,

    /// A partition target must be large enough to contain at least one byte.
    #[error("target uncompressed file size must be greater than zero")]
    InvalidTargetSize,

    /// Prepared rows were not unique and strictly ordered by the V1 sort key.
    #[error("prepared rows must be unique and sorted by unsigned (created_at, id)")]
    UnsortedOrDuplicateRows,

    /// A file does not conform to the canonical V1 profile.
    #[error("invalid canonical V1 Parquet file: {0}")]
    InvalidFile(String),

    /// Reading a local file failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Decoding a notepack payload failed.
    #[error("notepack error: {0}")]
    Notepack(#[from] notepack::Error),

    /// Arrow array or record-batch construction failed.
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),

    /// Parquet encoding or file finalization failed.
    #[error("Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
}
