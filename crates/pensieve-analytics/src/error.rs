//! Error types for analytics construction.

use std::path::PathBuf;

/// Analytics builder result.
pub type Result<T> = std::result::Result<T, Error>;

/// Failures that prevent a complete analytics run from being built.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bounded run evidence or execution failed validation.
    #[error("bounded analytics execution error: {0}")]
    Bounded(#[from] crate::BoundedExecutionError),
    /// The active-file catalog is invalid or unreadable.
    #[error("active-file catalog error: {0}")]
    Catalog(#[from] pensieve_lake::Error),
    /// DuckDB could not scan or aggregate the selected snapshot.
    #[error("DuckDB error: {0}")]
    DuckDb(#[from] duckdb::Error),
    /// SQLite could not persist bounded current-state facts.
    #[error("SQLite state error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A local object does not exist under the configured lake root.
    #[error("catalog object is missing locally: {0}")]
    MissingLocalObject(PathBuf),
    /// The catalog's store identity cannot be converted to an S3 location.
    #[error("unsupported store_id {0:?}; expected s3+https://<endpoint>/<bucket>")]
    UnsupportedStoreId(String),
    /// A numeric value does not fit the supported signed domain.
    #[error("{field} value {value} exceeds the supported signed 64-bit domain")]
    NumericOverflow {
        /// Name of the overflowing field.
        field: &'static str,
        /// Original unsigned value.
        value: u64,
    },
    /// The materialized rollups do not reconcile.
    #[error("analytics validation failed: {0}")]
    Validation(String),
    /// Summing catalog delta accounting exceeded the unsigned 64-bit domain.
    #[error("catalog delta {0} exceeds the unsigned 64-bit domain")]
    PlanOverflow(&'static str),
    /// A supposedly non-negative counter was negative.
    #[error("{field} is negative: {value}")]
    NegativeLedgerValue {
        /// Counter name.
        field: &'static str,
        /// Invalid signed value.
        value: i64,
    },
    /// The selected delta plan is inconsistent with the target snapshot.
    #[error("invalid incremental analytics plan: {0}")]
    InvalidIncrementalPlan(String),
    /// The durable DuckDB checkpoint is for a different snapshot.
    #[error("DuckDB checkpoint is at snapshot {actual}, expected {expected}")]
    CheckpointSnapshotMismatch {
        /// Snapshot stored in DuckDB.
        actual: String,
        /// Required baseline or target snapshot.
        expected: String,
    },
    /// A delta repeated an event ID with conflicting committed fields.
    #[error("delta contains {0} event IDs whose created_at or kind conflicts with the checkpoint")]
    ConflictingDeltaEvents(u64),
    /// Local artifact or evidence I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
