//! Errors returned by lake work-unit and publication operations.

use std::path::PathBuf;

use thiserror::Error;

/// Result type for lake operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by operational lake machinery.
#[derive(Debug, Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite inventory access failed.
    #[error("inventory database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Canonical Parquet processing failed.
    #[error("Parquet archive error: {0}")]
    Parquet(#[from] pensieve_parquet::Error),

    /// Existing work-unit identity conflicts with the requested source/configuration.
    #[error("work unit {work_unit_id} conflicts with existing inventory: {reason}")]
    WorkUnitConflict {
        /// Stable work-unit identifier.
        work_unit_id: String,
        /// Conflicting property.
        reason: String,
    },

    /// A state transition is not valid for the current work-unit state.
    #[error("invalid work-unit transition for {work_unit_id}: {from} -> {to}")]
    InvalidTransition {
        /// Stable work-unit identifier.
        work_unit_id: String,
        /// Current state.
        from: String,
        /// Requested state.
        to: String,
    },

    /// A previously generated local artifact no longer matches its inventory record.
    #[error("local artifact differs from inventory: {path}")]
    ArtifactMismatch {
        /// Conflicting artifact path.
        path: PathBuf,
    },

    /// An immutable object key already exists with different content.
    #[error("immutable object collision at {key}")]
    ObjectCollision {
        /// Conflicting object key.
        key: String,
    },

    /// An object-store request failed.
    #[error("object-store error: {0}")]
    ObjectStore(String),

    /// A requested database enum value is not recognized.
    #[error("invalid inventory value for {field}: {value}")]
    InvalidInventoryValue {
        /// Column or logical field.
        field: &'static str,
        /// Stored value.
        value: String,
    },

    /// A numeric value cannot be represented by the inventory schema.
    #[error("numeric inventory value is out of range: {field}")]
    NumericOutOfRange {
        /// Logical field name.
        field: &'static str,
    },
}
