//! Operational work-unit, inventory, and publication machinery for the Parquet lake.
//!
//! Canonical row semantics and physical Parquet encoding remain owned by
//! `pensieve-parquet`. This crate owns resumability and the external facts that
//! deliberately do not belong in canonical file metadata.

mod catalog;
mod error;
mod inventory;
mod publisher;
mod source_manifest;
mod work_unit;

pub use catalog::{
    ACTIVE_RAW_CATALOG_FORMAT, ActiveRawFragment, ActiveRawSnapshot, CatalogObject, CatalogTotals,
    CatalogWorkUnit, merge_active_raw_fragments, read_catalog_fragment, read_catalog_snapshot,
    write_catalog_atomically,
};
pub use error::{Error, Result};
pub use inventory::{
    Inventory, ObjectKind, ObjectRecord, ObjectState, WorkState, WorkUnitRecord,
    WorkUnitRegistration,
};
pub use publisher::{LocalObjectStore, PublishedObject, Publisher, S3Publisher, S3PublisherConfig};
pub use source_manifest::{
    CompletionProblem, CompletionTotals, HISTORICAL_SOURCE_EXCEPTIONS_FORMAT,
    HISTORICAL_SOURCE_MANIFEST_FORMAT, HistoricalCompletionAudit, HistoricalSourceEntry,
    HistoricalSourceException, HistoricalSourceExceptions, HistoricalSourceManifest,
    HistoricalSourceTotals, audit_historical_completion, historical_source_exception_from_salvage,
    historical_source_exceptions_from_salvage, read_historical_source_exceptions,
    read_historical_source_manifest, write_historical_source_exceptions_noclobber,
    write_historical_source_manifest_noclobber,
};
pub use work_unit::{
    CampaignConfig, CampaignSummary, CleanupSummary, DEFAULT_TARGET_UNCOMPRESSED_BYTES,
    cleanup_published_local_artifacts, run_notepack_work_unit, sha256_file,
};
