//! Rebuildable analytics products derived from canonical Parquet snapshots.
//!
//! DuckDB performs large object scans and exact aggregation. Postgres receives
//! only small serving relations, all keyed by one immutable analytics run.

mod bounded;
mod build;
mod error;
mod incremental;
mod input;
mod plan;
mod publish;
mod reconcile;
mod schema;

pub use bounded::{
    ArtifactIdentity, BOUNDED_CHECKPOINT_SCHEMA_VERSION, BOUNDED_RUNNER_VERSION,
    BoundedExecutionError, InputIdentity, RunCheckpoint, RunIdentity, publish_run_checkpoint,
    read_run_checkpoint, validate_run_checkpoint,
};
pub use build::{
    AnalyticsBuild, BuildConfig, BuildSummary, EventDaily, EventDailyKind, KindAllTime, Overview,
    QUERY_VERSION,
};
pub use error::{Error, Result};
pub use incremental::{IncrementalSummary, apply_incremental, resolve_delta_locations};
pub use input::{ObjectLocation, ResolvedSnapshot, resolve_snapshot};
pub use plan::{AppliedObject, CatalogDeltaPlan, PlannedRunKind, plan_catalog_delta};
pub use publish::{PublishOutcome, acquire_publication_lock, publish, publish_incremental};
pub use reconcile::{
    Classification, ComparisonGate, DifferenceExample, InputAlignment, MetricComparison,
    ReconciliationSummary, SeriesComparison, compare_metric, compare_series,
};
