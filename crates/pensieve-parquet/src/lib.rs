//! Canonical Parquet archive writing for validated Nostr events.
//!
//! This crate owns Pensieve's V1 Parquet schema and writer configuration. It
//! deliberately does not own live buffering, object publication, migration
//! checkpoints, or compaction policy.

mod error;
mod row;
mod salvage;
mod schema;
mod segment;
mod validator;
mod writer;

pub use error::{Error, Result};
pub use row::{CanonicalEvent, RawEvent};
pub use salvage::{
    NOTEPACK_SALVAGE_FORMAT, SALVAGE_REPORT_NAME, SALVAGED_SEGMENT_NAME, SalvageReport,
    TRUNCATED_TAIL_NAME, read_salvage_report, salvage_truncated_segment,
};
pub use schema::{
    ARCHIVE_VERSION, ARCHIVE_VERSION_KEY, ROW_GROUP_TARGET_BYTES, canonical_arrow_schema,
};
pub use segment::{
    ConversionSummary, DEFAULT_MAX_EVENT_BYTES, RejectedFrame, SegmentScan, convert_segment,
    convert_segment_quarantining_invalid, read_framed_notepack, scan_framed_notepack, scan_segment,
    write_rejected_segment,
};
pub use validator::{ValidationReport, validate_file};
pub use writer::{
    WriteSummary, partition_prepared_rows, prepare_canonical_events, prepare_events,
    write_canonical_events, write_events, write_prepared,
};

/// Operational implementation identity for external inventories.
///
/// This is deliberately not canonical file metadata.
pub const IMPLEMENTATION_VERSION: &str = concat!("pensieve-parquet/", env!("CARGO_PKG_VERSION"));
