//! Operational work-unit, inventory, and publication machinery for the Parquet lake.
//!
//! Canonical row semantics and physical Parquet encoding remain owned by
//! `pensieve-parquet`. This crate owns resumability and the external facts that
//! deliberately do not belong in canonical file metadata.

mod error;
mod inventory;
mod publisher;
mod work_unit;

pub use error::{Error, Result};
pub use inventory::{
    Inventory, ObjectKind, ObjectRecord, ObjectState, WorkState, WorkUnitRecord,
    WorkUnitRegistration,
};
pub use publisher::{LocalObjectStore, PublishedObject, Publisher, S3Publisher, S3PublisherConfig};
pub use work_unit::{
    CampaignConfig, CampaignSummary, DEFAULT_TARGET_UNCOMPRESSED_BYTES, run_notepack_work_unit,
    sha256_file,
};
