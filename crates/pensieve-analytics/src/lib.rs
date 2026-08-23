//! Rebuildable analytics products derived from canonical Parquet snapshots.
//!
//! DuckDB performs large object scans and exact aggregation. Postgres receives
//! only small serving relations, all keyed by one immutable analytics run.

mod bounded;
mod build;
mod error;
mod event_facts;
mod fixed_activity;
mod incremental;
mod input;
mod plan;
mod pubkey_first_seen;
mod publish;
mod reconcile;
mod schema;

pub use bounded::{
    ArtifactIdentity, BOUNDED_CHECKPOINT_SCHEMA_VERSION, BOUNDED_RUNNER_VERSION, BatchLimits,
    BoundedExecutionError, CleanupEligibility, CompactionConfig, CompactionStep, DiskBudget,
    DiskPreflight, FixedRecordLayout, InputBatch, InputIdentity, MergeStats, RunCheckpoint,
    RunIdentity, RunReference, cleanup_is_eligible, load_reusable_checkpoint,
    merge_fixed_min_u64_runs, merge_fixed_runs, merge_fixed_sum_u64_runs, plan_input_batches,
    plan_levelled_compaction, preflight_disk, publish_canonical_json, publish_run_checkpoint,
    read_run_checkpoint, validate_run_checkpoint,
};
pub use build::{
    AnalyticsBuild, BuildConfig, BuildSummary, EventDaily, EventDailyKind, IDENTITY_QUERY_VERSION,
    KindAllTime, Overview, QUERY_VERSION,
};
pub use error::{Error, Result};
pub use event_facts::{
    BoundedEventBuild, EVENT_FACT_BYTES, EVENT_FACT_KEY_BYTES, EventFact, EventFactBatchStats,
    EventFactReader, EventFactsConfig, EventFactsEvidence, EventFactsMemoryEvidence,
    build_bounded_event_facts,
};
pub use fixed_activity::{
    ActiveUsersPeriod, BoundedFixedActivity, DistinctPubkeysPeriod, FIXED_ACTIVITY_KEY_BYTES,
    FIXED_ACTIVITY_RECORD_BYTES, FIXED_ACTIVITY_VERSION, FixedActivityConfig,
    FixedActivityEvidence, PUBKEY_FLAGS_RECORD_BYTES, advance_bounded_fixed_activity,
    build_bounded_fixed_activity, load_bounded_fixed_activity,
};
pub use incremental::{IncrementalSummary, apply_incremental, resolve_delta_locations};
pub use input::{ObjectLocation, ResolvedSnapshot, resolve_snapshot};
pub use plan::{
    AppliedObject, CatalogDeltaPlan, PlannedRunKind, plan_catalog_delta,
    plan_catalog_delta_for_query_version,
};
pub use pubkey_first_seen::{
    BoundedPubkeyFirstSeen, NewUsersDaily, PUBKEY_FIRST_SEEN_BYTES, PUBKEY_FIRST_SEEN_KEY_BYTES,
    PUBKEY_FIRST_SEEN_VERSION, PubkeyFirstSeenConfig, PubkeyFirstSeenEvidence,
    advance_bounded_pubkey_first_seen, build_bounded_pubkey_first_seen,
    load_bounded_pubkey_first_seen,
};
pub use publish::{
    PublishOutcome, acquire_publication_lock, publish, publish_incremental,
    publish_incremental_with_identity, publish_with_identity,
};
pub use reconcile::{
    Classification, ComparisonGate, DifferenceExample, InputAlignment, MetricComparison,
    ReconciliationSummary, SeriesComparison, compare_metric, compare_series,
};
