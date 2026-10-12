//! Rebuildable analytics products derived from canonical Parquet snapshots.
//!
//! DuckDB performs large object scans and exact aggregation; results are kept
//! as local DuckDB databases and immutable, evidence-checked artifacts for
//! research over the Parquet lake.

mod bounded;
mod build;
mod cohort_retention;
mod error;
mod event_facts;
mod fixed_activity;
mod flexible_distinct;
mod flexible_distinct_validation;
mod incremental;
mod input;
mod plan;
mod pubkey_first_seen;
mod publisher_benchmark;
mod publisher_product;
mod relay_build;
mod relay_distribution;
mod semantic_build;
mod semantic_facts;
mod serving_facts;
mod zap_distinct;

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
pub use error::{Error, Result};
pub use event_facts::{
    BoundedEventBuild, EVENT_FACT_BYTES, EVENT_FACT_KEY_BYTES, EventFact, EventFactBatchStats,
    EventFactReader, EventFactsConfig, EventFactsEvidence, EventFactsMemoryEvidence,
    advance_bounded_event_facts, build_bounded_event_facts, load_event_facts_evidence,
};
pub use fixed_activity::{
    ActiveUsersPeriod, BoundedFixedActivity, DistinctPubkeysPeriod, FIXED_ACTIVITY_KEY_BYTES,
    FIXED_ACTIVITY_RECORD_BYTES, FIXED_ACTIVITY_VERSION, FixedActivityConfig,
    FixedActivityEvidence, PUBKEY_FLAGS_RECORD_BYTES, advance_bounded_fixed_activity,
    build_bounded_fixed_activity, exact_kind_unique_pubkeys, load_bounded_fixed_activity,
    upgrade_bounded_fixed_activity_v2,
};
pub use flexible_distinct::{
    BoundedFlexibleDistinct, FLEXIBLE_DISTINCT_IDENTITY_BYTES, FLEXIBLE_DISTINCT_VERSION,
    FlexibleDistinctConfig, FlexibleDistinctEvidence, FlexibleDistinctWindow,
    advance_bounded_flexible_distinct, build_bounded_flexible_distinct,
    estimate_flexible_distinct_window, estimate_flexible_distinct_windows,
    load_and_estimate_flexible_distinct_windows, load_bounded_flexible_distinct,
    visit_flexible_distinct_leaves,
};
pub use flexible_distinct_validation::{
    FLEXIBLE_DISTINCT_TOLERANCE_PPM, FLEXIBLE_DISTINCT_VALIDATION_RUNNER,
    FlexibleDistinctValidationEvidence, FlexibleDistinctValidationSample,
    build_flexible_distinct_validation,
};
pub use incremental::{IncrementalSummary, apply_incremental, resolve_delta_locations};
pub use input::{ObjectLocation, ResolvedSnapshot, resolve_snapshot};
pub use pensieve_core::{
    DATASKETCHES_HLL_SERIALIZATION_VERSION, DISTINCT_SKETCH_FORMAT_VERSION, DISTINCT_SKETCH_LG_K,
    DISTINCT_SKETCH_RELATIVE_TOLERANCE, DistinctSketch, DistinctSketchBuilder, DistinctSketchError,
    DistinctSketchUnion, ZAP_DISTINCT_SKETCH_LG_K, ZAP_HISTOGRAM_UPPER_SATS,
};
pub use plan::{AppliedObject, CatalogDeltaPlan, PlannedRunKind};
pub use pubkey_first_seen::{
    BoundedPubkeyFirstSeen, NewUsersDaily, PUBKEY_FIRST_SEEN_BYTES, PUBKEY_FIRST_SEEN_KEY_BYTES,
    PUBKEY_FIRST_SEEN_VERSION, PubkeyFirstSeenConfig, PubkeyFirstSeenEvidence,
    advance_bounded_pubkey_first_seen, build_bounded_pubkey_first_seen,
    load_bounded_pubkey_first_seen,
};
pub use publisher_benchmark::{
    PUBLISHER_BENCHMARK_VERSION, PUBLISHER_DAILY_FACT_BYTES, PUBLISHER_DAILY_KIND_FACT_BYTES,
    PUBLISHER_MATERIALIZED_TOP_BYTES, PublisherBenchmarkConfig, PublisherBenchmarkEvidence,
    PublisherBenchmarkRow, benchmark_publishers,
};
pub use publisher_product::{
    BoundedPublisherRanking, PUBLISHER_RANKING_RECORD_BYTES, PUBLISHER_RANKING_VERSION,
    PublisherRankingConfig, PublisherRankingEvidence, PublisherRankingRow,
    advance_bounded_publisher_ranking, build_bounded_publisher_ranking,
    load_bounded_publisher_ranking, visit_publisher_ranking_rows,
};
pub use relay_build::{
    BoundedRelayDistribution, RelayDistributionConfig, RelayDistributionEvidence,
    advance_bounded_relay_distribution, build_bounded_relay_distribution,
    load_bounded_relay_distribution, load_bounded_relay_distribution_for_advance,
};
pub use relay_distribution::{
    RELAY_DISTRIBUTION_VERSION, RelayDistributionRow, RelayListIdentity, RelayMembership,
    relay_list_wins, relay_memberships,
};
pub use semantic_build::{
    BoundedSemanticFacts, SEMANTIC_FACTS_RUNNER_VERSION, SEMANTIC_FACTS_VERSION,
    SemanticDomainCounts, SemanticFactsConfig, SemanticFactsEvidence, SemanticMemoryEvidence,
    advance_bounded_semantic_facts, build_bounded_semantic_facts, load_bounded_semantic_facts,
};
pub use semantic_facts::{
    EngagementDay, EngagementFact, LongformDay, LongformFact, MAX_ZAP_AMOUNT_MSATS,
    SEMANTIC_FACT_BYTES, SEMANTIC_FACT_KEY_BYTES, SemanticFactReader, SemanticFactRecord,
    SemanticPayload, SemanticRollups, SemanticScanStats, ZapDay, ZapFact, ZapRejection,
    classify_engagement, classify_longform, classify_zap, scan_semantic_facts,
    write_semantic_facts, zap_histogram_bucket,
};
pub use serving_facts::{
    BoundedServingFacts, CONTENT_FACT_BYTES, HOURLY_COUNT_BYTES, KIND_SUMMARY_BYTES,
    SERVING_FACTS_RUNNER_VERSION, SERVING_FACTS_VERSION, ServingFactsConfig, ServingFactsEvidence,
    ServingFactsMemoryEvidence, ServingHourlyRow, ServingKindRow, advance_bounded_serving_facts,
    build_bounded_serving_facts, load_bounded_serving_facts, visit_serving_hourly_rows,
    visit_serving_kind_rows,
};
pub use zap_distinct::{
    BoundedZapDistinct, ZAP_DISTINCT_VERSION, ZAP_IDENTITY_BYTES, ZapDistinctConfig,
    ZapDistinctEvidence, ZapDistinctLeaf, ZapParticipantRole, build_bounded_zap_distinct,
    load_bounded_zap_distinct,
};
