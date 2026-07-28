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
    /// Writing streamed COPY data failed.
    #[error("failed to stream Postgres COPY data: {0}")]
    CopyIo(#[from] std::io::Error),
}
