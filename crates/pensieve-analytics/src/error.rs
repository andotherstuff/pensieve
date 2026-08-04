//! Error types for analytics construction and publication.

use std::path::PathBuf;

/// Analytics builder result.
pub type Result<T> = std::result::Result<T, Error>;

/// Failures that prevent a complete analytics run from being published.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The active-file catalog is invalid or unreadable.
    #[error("active-file catalog error: {0}")]
    Catalog(#[from] pensieve_lake::Error),
    /// DuckDB could not scan or aggregate the selected snapshot.
    #[error("DuckDB error: {0}")]
    DuckDb(#[from] duckdb::Error),
    /// Postgres could not stage or atomically publish the run.
    #[error("Postgres error: {0}")]
    Postgres(#[from] postgres::Error),
    /// A local object does not exist under the configured lake root.
    #[error("catalog object is missing locally: {0}")]
    MissingLocalObject(PathBuf),
    /// The catalog's store identity cannot be converted to an S3 location.
    #[error("unsupported store_id {0:?}; expected s3+https://<endpoint>/<bucket>")]
    UnsupportedStoreId(String),
    /// A numeric value does not fit the serving database's signed domain.
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
    /// A deterministic run already exists but is not the current run.
    #[error("analytics run {0} was already published and cannot replace a newer current run")]
    StalePublishedRun(String),
    /// An immutable object key was reused with different content or accounting.
    #[error(
        "immutable catalog object {object_key} changed identity: previous SHA-256 \
         {previous_sha256}, selected SHA-256 {selected_sha256}"
    )]
    ImmutableObjectChanged {
        /// Reused immutable key.
        object_key: String,
        /// Previously applied content digest.
        previous_sha256: String,
        /// Digest in the selected catalog.
        selected_sha256: String,
    },
    /// Summing catalog delta accounting exceeded the unsigned 64-bit domain.
    #[error("catalog delta {0} exceeds the unsigned 64-bit domain")]
    PlanOverflow(&'static str),
    /// A timestamp loaded from the durable object ledger is not unsigned decimal.
    #[error("applied-object ledger contains invalid timestamp {0:?}")]
    InvalidLedgerTimestamp(String),
    /// A supposedly non-negative ledger counter was negative.
    #[error("applied-object ledger {field} is negative: {value}")]
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
    /// Postgres advanced while an incremental build was running.
    #[error("Postgres current run changed: expected {expected}, found {actual:?}")]
    PublicationBaselineChanged {
        /// Run against which the delta was planned.
        expected: String,
        /// Current run observed under the publication lock.
        actual: Option<String>,
    },
    /// Writing streamed COPY data failed.
    #[error("failed to stream Postgres COPY data: {0}")]
    CopyIo(#[from] std::io::Error),
}
