//! Catalog delta plan types consumed by bounded incremental analytics work.
//!
//! A plan names the immutable objects added since a baseline snapshot and is
//! supplied to the advancing builders as persisted JSON.

use pensieve_lake::CatalogObject;
use serde::{Deserialize, Serialize};

/// Execution mode required to advance from the currently published snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedRunKind {
    /// There is no published analytics baseline.
    FullRebuild,
    /// The selected catalog exactly matches the applied-object ledger.
    NoChange,
    /// The selected catalog only adds immutable objects.
    Incremental,
    /// The selected catalog removes objects and requires partition replacement.
    AffectedPeriodRebuild,
}

/// One object from the durable applied-object ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppliedObject {
    /// Immutable object-store key.
    pub object_key: String,
    /// Owning source work unit.
    pub work_unit_id: String,
    /// Lowercase SHA-256 of the object bytes.
    pub sha256: String,
    /// Exact object byte size.
    pub byte_size: u64,
    /// Physical rows asserted by the catalog.
    pub physical_rows: u64,
    /// Minimum unsigned event timestamp, when recorded.
    pub min_created_at: Option<String>,
    /// Maximum unsigned event timestamp, when recorded.
    pub max_created_at: Option<String>,
}

/// Exact catalog work required after the current successful publication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogDeltaPlan {
    /// Selected target snapshot.
    pub snapshot_id: String,
    /// Current successful run used as the baseline.
    pub previous_run_id: Option<String>,
    /// Snapshot associated with the baseline run.
    pub previous_snapshot_id: Option<String>,
    /// Required execution mode.
    pub run_kind: PlannedRunKind,
    /// New immutable objects that need staging and processing.
    pub added_objects: Vec<CatalogObject>,
    /// Previously applied objects absent from the selected snapshot.
    pub removed_objects: Vec<AppliedObject>,
    /// Objects whose identity is unchanged.
    pub unchanged_objects: u64,
    /// Bytes that need staging for this plan.
    pub added_bytes: u64,
    /// Physical rows in newly added objects.
    pub added_physical_rows: u64,
    /// Earliest timestamp touched by added or removed objects, when known.
    pub affected_min_created_at: Option<String>,
    /// Latest timestamp touched by added or removed objects, when known.
    pub affected_max_created_at: Option<String>,
    /// Whether every changed object supplied timestamp bounds.
    pub affected_range_complete: bool,
}
