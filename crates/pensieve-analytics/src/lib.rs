//! Rebuildable analytics products derived from canonical Parquet snapshots.
//!
//! DuckDB performs large object scans and exact aggregation. Postgres receives
//! only small serving relations, all keyed by one immutable analytics run.

mod bounded;
mod build;
mod error;
mod event_facts;
mod incremental;
mod input;
mod plan;
mod publish;
mod reconcile;
mod schema;

pub use bounded::{
    ArtifactIdentity, BOUNDED_CHECKPOINT_SCHEMA_VERSION, BOUNDED_RUNNER_VERSION, BatchLimits,
    BoundedExecutionError, CleanupEligibility, CompactionConfig, CompactionStep, DiskBudget,
    DiskPreflight, FixedRecordLayout, InputBatch, InputIdentity, MergeStats, RunCheckpoint,
    RunIdentity, RunReference, cleanup_is_eligible, load_reusable_checkpoint,
    merge_fixed_min_u64_runs, merge_fixed_runs, plan_input_batches, plan_levelled_compaction,
    preflight_disk, publish_canonical_json, publish_run_checkpoint, read_run_checkpoint,
    validate_run_checkpoint,
};
pub use build::{
    AnalyticsBuild, BuildConfig, BuildSummary, EventDaily, EventDailyKind, KindAllTime, Overview,
    QUERY_VERSION,
};
pub use error::{Error, Result};
pub use event_facts::{
    BoundedEventBuild, EVENT_FACT_BYTES, EVENT_FACT_KEY_BYTES, EventFact, EventFactBatchStats,
    EventFactReader, EventFactsConfig, EventFactsEvidence, EventFactsMemoryEvidence,
    build_bounded_event_facts,
};
pub use incremental::{IncrementalSummary, apply_incremental, resolve_delta_locations};
pub use input::{ObjectLocation, ResolvedSnapshot, resolve_snapshot};
pub use plan::{AppliedObject, CatalogDeltaPlan, PlannedRunKind, plan_catalog_delta};
pub use publish::{PublishOutcome, acquire_publication_lock, publish, publish_incremental};
pub use reconcile::{
    Classification, ComparisonGate, DifferenceExample, InputAlignment, MetricComparison,
    ReconciliationSummary, SeriesComparison, compare_metric, compare_series,
};
