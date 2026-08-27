//! Rebuildable analytics products derived from canonical Parquet snapshots.
//!
//! DuckDB performs large object scans and exact aggregation. Postgres receives
//! only small serving relations, all keyed by one immutable analytics run.

mod bounded;
mod build;
mod cohort_retention;
mod distinct_sketch;
mod error;
mod event_facts;
mod fixed_activity;
mod flexible_distinct;
mod flexible_distinct_publish;
mod incremental;
mod input;
mod plan;
mod pubkey_first_seen;
mod publish;
mod reconcile;
mod schema;
mod semantic_build;
mod semantic_facts;

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
    AnalyticsBuild, BuildConfig, BuildSummary, COHORT_RETENTION_QUERY_VERSION, EventDaily,
    EventDailyKind, FIXED_ACTIVITY_QUERY_VERSION, IDENTITY_QUERY_VERSION, KindAllTime, Overview,
    QUERY_VERSION,
};
pub use cohort_retention::{
    BoundedCohortRetention, CohortRetentionEvidence, CohortRetentionPeriod,
    build_bounded_cohort_retention, load_bounded_cohort_retention,
};
pub use distinct_sketch::{
    DATASKETCHES_HLL_SERIALIZATION_VERSION, DISTINCT_SKETCH_FORMAT_VERSION, DISTINCT_SKETCH_LG_K,
    DISTINCT_SKETCH_RELATIVE_TOLERANCE, DistinctSketch, DistinctSketchBuilder, DistinctSketchError,
    DistinctSketchUnion,
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
    build_bounded_fixed_activity, load_bounded_fixed_activity, upgrade_bounded_fixed_activity_v2,
};
pub use flexible_distinct::{
    BoundedFlexibleDistinct, FLEXIBLE_DISTINCT_IDENTITY_BYTES, FLEXIBLE_DISTINCT_VERSION,
    FlexibleDistinctConfig, FlexibleDistinctEvidence, FlexibleDistinctWindow,
    build_bounded_flexible_distinct, estimate_flexible_distinct_window,
    estimate_flexible_distinct_windows, load_and_estimate_flexible_distinct_windows,
    load_bounded_flexible_distinct, visit_flexible_distinct_leaves,
};
pub use flexible_distinct_publish::{
    FlexibleDistinctPublishOutcome, estimate_published_flexible_distinct,
    publish_flexible_distinct_leaves,
};
pub use incremental::{IncrementalSummary, apply_incremental, resolve_delta_locations};
pub use input::{ObjectLocation, ResolvedSnapshot, resolve_snapshot};
pub use plan::{
    AppliedObject, CatalogDeltaPlan, PlannedRunKind, plan_catalog_delta,
    plan_catalog_delta_for_query_version, plan_catalog_delta_from_run,
};
pub use pubkey_first_seen::{
    BoundedPubkeyFirstSeen, NewUsersDaily, PUBKEY_FIRST_SEEN_BYTES, PUBKEY_FIRST_SEEN_KEY_BYTES,
    PUBKEY_FIRST_SEEN_VERSION, PubkeyFirstSeenConfig, PubkeyFirstSeenEvidence,
    advance_bounded_pubkey_first_seen, build_bounded_pubkey_first_seen,
    load_bounded_pubkey_first_seen,
};
pub use publish::{
    AllBoundedProducts, PublishOutcome, acquire_publication_lock, publish, publish_incremental,
    publish_incremental_with_all_bounded_products, publish_incremental_with_identity,
    publish_incremental_with_identity_and_activity, publish_with_all_bounded_products,
    publish_with_identity, publish_with_identity_and_activity,
};
pub use reconcile::{
    Classification, ComparisonGate, DifferenceExample, InputAlignment, MetricComparison,
    ReconciliationSummary, SeriesComparison, compare_metric, compare_series,
};
pub use semantic_build::{
    BoundedSemanticFacts, SEMANTIC_FACTS_VERSION, SemanticDomainCounts, SemanticFactsConfig,
    SemanticFactsEvidence, SemanticMemoryEvidence, build_bounded_semantic_facts,
};
pub use semantic_facts::{
    EngagementDay, EngagementFact, LongformDay, LongformFact, MAX_ZAP_AMOUNT_MSATS,
    SEMANTIC_FACT_BYTES, SEMANTIC_FACT_KEY_BYTES, SemanticFactReader, SemanticFactRecord,
    SemanticPayload, SemanticRollups, SemanticScanStats, ZAP_HISTOGRAM_UPPER_SATS, ZapDay, ZapFact,
    ZapRejection, classify_engagement, classify_longform, classify_zap, scan_semantic_facts,
    write_semantic_facts, zap_histogram_bucket,
};
