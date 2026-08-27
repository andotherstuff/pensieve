//! Transactional Postgres publication for completed Slice A products.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use postgres::{Client, GenericClient};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    AnalyticsBuild, BoundedCohortRetention, BoundedFixedActivity, BoundedFlexibleDistinct,
    BoundedPubkeyFirstSeen, BoundedPublisherRanking, BoundedRelayDistribution,
    BoundedSemanticFacts, BoundedZapDistinct, COHORT_RETENTION_QUERY_VERSION, Error,
    FIXED_ACTIVITY_QUERY_VERSION, IDENTITY_QUERY_VERSION, QUERY_VERSION, Result,
    flexible_distinct_publish::{
        publish_flexible_distinct_leaves_in_transaction, validate_flexible_distinct_publication,
    },
    publisher_publish::{
        PublisherRankingPublishOutcome, ValidatedPublisherRankingPublication,
        publish_publisher_ranking_in_transaction, validate_publisher_ranking_publication,
    },
    relay_publish::{
        RelayDistributionPublishOutcome, ValidatedRelayDistributionPublication,
        publish_relay_distribution_in_transaction, validate_relay_distribution_publication,
    },
    schema::SCHEMA_SQL,
    semantic_publish::{
        SemanticPublishOutcome, ValidatedSemanticPublication,
        publish_semantic_facts_in_transaction, validate_semantic_publication,
    },
    zap_distinct_publish::{
        ValidatedZapDistinctPublication, ZapDistinctPublishOutcome,
        publish_zap_distinct_in_transaction, validate_zap_distinct_publication,
    },
};

const PUBLICATION_LOCK_ID: i64 = 8_056_718_693_194_101_224;

#[derive(Clone, Copy, Default)]
struct PublicationProducts<'a> {
    identity: Option<&'a BoundedPubkeyFirstSeen>,
    activity: Option<&'a BoundedFixedActivity>,
    cohort: Option<&'a BoundedCohortRetention>,
    flexible: Option<FlexibleDistinctPublication<'a>>,
    semantic: Option<SemanticPublication<'a>>,
    relay: Option<&'a BoundedRelayDistribution>,
    publisher: Option<&'a BoundedPublisherRanking>,
}

/// Hold the analytics publication lock for the lifetime of `client`.
///
/// Incremental executors use this before planning so no other publisher can
/// advance Postgres between the catalog diff and the DuckDB commit. PostgreSQL
/// releases the session-scoped lock automatically if the process disconnects.
pub fn acquire_publication_lock(client: &mut Client) -> Result<()> {
    client.query_one("SELECT pg_advisory_lock($1)", &[&PUBLICATION_LOCK_ID])?;
    Ok(())
}

/// Result of attempting to publish a deterministic run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    /// A new run was staged, validated, and made current.
    Published {
        /// Deterministic analytics run identifier.
        run_id: String,
        /// Previously current run, if any.
        previous_run_id: Option<String>,
    },
    /// The same run was already the complete current run.
    AlreadyCurrent {
        /// Deterministic analytics run identifier.
        run_id: String,
    },
}

/// Exact bounded Slice B products that must publish as one generation.
#[derive(Clone, Copy)]
pub struct AllBoundedProducts<'a> {
    /// Eligible-pubkey first-seen state.
    pub identity: &'a BoundedPubkeyFirstSeen,
    /// Fixed-grain pubkey activity state.
    pub activity: &'a BoundedFixedActivity,
    /// Exact weekly and monthly cohort-retention matrix.
    pub cohort: &'a BoundedCohortRetention,
}

/// Validated Slice 6 product and its exact production tolerance gate.
#[derive(Clone, Copy)]
pub struct FlexibleDistinctPublication<'a> {
    /// Complete-hour HLL leaf product.
    pub product: &'a BoundedFlexibleDistinct,
    /// Canonical passed tolerance evidence.
    pub validation_evidence_path: &'a Path,
    /// Explicitly authorized SHA-256 of the tolerance evidence.
    pub validation_evidence_sha256: &'a str,
}

/// Validated Slice 7 exact semantic facts and participant sketches.
#[derive(Clone, Copy)]
pub struct SemanticPublication<'a> {
    /// Canonical additive semantic facts and rollups.
    pub product: &'a BoundedSemanticFacts,
    /// Daily validated zap sender/recipient sketches derived from the facts.
    pub zap_distinct: &'a BoundedZapDistinct,
}

/// Every validated bounded product currently carried by recurring publication.
#[derive(Clone, Copy)]
pub struct AllRecurringProducts<'a> {
    /// Exact B1/B2/B3 products.
    pub bounded: AllBoundedProducts<'a>,
    /// Complete-hour flexible distinct leaves and tolerance gate.
    pub flexible: FlexibleDistinctPublication<'a>,
    /// Additive semantic rollups and zap participant leaves.
    pub semantic: SemanticPublication<'a>,
}

/// Every validated bounded product through Slice 8.
#[derive(Clone, Copy)]
pub struct AllRecurringProductsWithRelay<'a> {
    /// Recurring products through Slice 7.
    pub recurring: AllRecurringProducts<'a>,
    /// Exact current NIP-65 relay distribution.
    pub relay: &'a BoundedRelayDistribution,
}

/// Every validated bounded product through Slice 9.
#[derive(Clone, Copy)]
pub struct AllRecurringProductsWithPublisher<'a> {
    /// Recurring products through Slice 8.
    pub recurring: AllRecurringProductsWithRelay<'a>,
    /// Exact predefined-window publisher rankings.
    pub publisher: &'a BoundedPublisherRanking,
}

#[derive(Serialize)]
struct ValidationRecord {
    event_daily_sum: u64,
    event_daily_kind_sum: u64,
    kind_all_time_sum: u64,
    eligible_pubkeys: u64,
    new_users_daily_sum: u64,
    identity_evidence_sha256: Option<String>,
    identity_metric_sha256: Option<String>,
    fixed_activity_evidence_sha256: Option<String>,
    fixed_activity_metric_sha256: Option<String>,
    distinct_pubkeys_period_rows: u64,
    active_users_period_rows: u64,
    cohort_retention_evidence_sha256: Option<String>,
    cohort_retention_metric_sha256: Option<String>,
    cohort_retention_rows: u64,
    flexible_distinct_evidence_sha256: Option<String>,
    flexible_distinct_validation_sha256: Option<String>,
    semantic_evidence_sha256: Option<String>,
    zap_distinct_evidence_sha256: Option<String>,
    relay_distribution_evidence_sha256: Option<String>,
    publisher_ranking_evidence_sha256: Option<String>,
    result: &'static str,
}

/// Publish one completed DuckDB build behind the Postgres current-run pointer.
///
/// Schema creation is idempotent. All run metadata, inputs, products, and the
/// pointer change are committed in one transaction while holding a transaction
/// advisory lock, so readers cannot observe a partial or mixed run.
pub fn publish(
    client: &mut Client,
    build: &AnalyticsBuild,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "full_rebuild",
        None,
        PublicationProducts::default(),
    )
}

/// Publish Slice A and one completed bounded identity product atomically.
pub fn publish_with_identity(
    client: &mut Client,
    build: &AnalyticsBuild,
    identity: &BoundedPubkeyFirstSeen,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "full_rebuild",
        None,
        PublicationProducts {
            identity: Some(identity),
            activity: None,
            cohort: None,
            flexible: None,
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish Slice A, first-seen, and fixed-grain activity products atomically.
pub fn publish_with_identity_and_activity(
    client: &mut Client,
    build: &AnalyticsBuild,
    identity: &BoundedPubkeyFirstSeen,
    activity: &BoundedFixedActivity,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "full_rebuild",
        None,
        PublicationProducts {
            identity: Some(identity),
            activity: Some(activity),
            cohort: None,
            flexible: None,
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish Slice A and every completed bounded Slice B product atomically.
pub fn publish_with_all_bounded_products(
    client: &mut Client,
    build: &AnalyticsBuild,
    products: AllBoundedProducts<'_>,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "full_rebuild",
        None,
        PublicationProducts {
            identity: Some(products.identity),
            activity: Some(products.activity),
            cohort: Some(products.cohort),
            flexible: None,
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish an incrementally advanced build if its planned baseline is current.
pub fn publish_incremental(
    client: &mut Client,
    build: &AnalyticsBuild,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts::default(),
    )
}

/// Publish an incremental Slice A build and bounded identity successor atomically.
pub fn publish_incremental_with_identity(
    client: &mut Client,
    build: &AnalyticsBuild,
    identity: &BoundedPubkeyFirstSeen,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(identity),
            activity: None,
            cohort: None,
            flexible: None,
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish incremental Slice A, first-seen, and activity successors atomically.
pub fn publish_incremental_with_identity_and_activity(
    client: &mut Client,
    build: &AnalyticsBuild,
    identity: &BoundedPubkeyFirstSeen,
    activity: &BoundedFixedActivity,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(identity),
            activity: Some(activity),
            cohort: None,
            flexible: None,
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish an incremental run and every bounded Slice B successor atomically.
pub fn publish_incremental_with_all_bounded_products(
    client: &mut Client,
    build: &AnalyticsBuild,
    products: AllBoundedProducts<'_>,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(products.identity),
            activity: Some(products.activity),
            cohort: Some(products.cohort),
            flexible: None,
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish an incremental B3 successor and its Slice 6 leaves atomically.
pub fn publish_incremental_with_all_bounded_products_and_flexible(
    client: &mut Client,
    build: &AnalyticsBuild,
    products: AllBoundedProducts<'_>,
    flexible: FlexibleDistinctPublication<'_>,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(products.identity),
            activity: Some(products.activity),
            cohort: Some(products.cohort),
            flexible: Some(flexible),
            semantic: None,
            relay: None,
            publisher: None,
        },
    )
}

/// Publish one incremental B3, Slice 6, and Slice 7 generation atomically.
pub fn publish_incremental_with_all_bounded_products_flexible_and_semantic(
    client: &mut Client,
    build: &AnalyticsBuild,
    products: AllRecurringProducts<'_>,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(products.bounded.identity),
            activity: Some(products.bounded.activity),
            cohort: Some(products.bounded.cohort),
            flexible: Some(products.flexible),
            semantic: Some(products.semantic),
            relay: None,
            publisher: None,
        },
    )
}

/// Publish one incremental generation containing every product through Slice 8.
pub fn publish_incremental_with_all_recurring_products_and_relay(
    client: &mut Client,
    build: &AnalyticsBuild,
    products: AllRecurringProductsWithRelay<'_>,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(products.recurring.bounded.identity),
            activity: Some(products.recurring.bounded.activity),
            cohort: Some(products.recurring.bounded.cohort),
            flexible: Some(products.recurring.flexible),
            semantic: Some(products.recurring.semantic),
            relay: Some(products.relay),
            publisher: None,
        },
    )
}

/// Publish one incremental generation containing every product through Slice 9.
pub fn publish_incremental_with_all_recurring_products_and_publisher(
    client: &mut Client,
    build: &AnalyticsBuild,
    products: AllRecurringProductsWithPublisher<'_>,
    expected_previous_run_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Result<PublishOutcome> {
    publish_kind(
        client,
        build,
        started_at,
        completed_at,
        "incremental",
        Some(expected_previous_run_id),
        PublicationProducts {
            identity: Some(products.recurring.recurring.bounded.identity),
            activity: Some(products.recurring.recurring.bounded.activity),
            cohort: Some(products.recurring.recurring.bounded.cohort),
            flexible: Some(products.recurring.recurring.flexible),
            semantic: Some(products.recurring.recurring.semantic),
            relay: Some(products.recurring.relay),
            publisher: Some(products.publisher),
        },
    )
}

fn publish_kind(
    client: &mut Client,
    build: &AnalyticsBuild,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    run_kind: &'static str,
    expected_previous_run_id: Option<&str>,
    products: PublicationProducts<'_>,
) -> Result<PublishOutcome> {
    let identity = products.identity;
    let activity = products.activity;
    let cohort = products.cohort;
    let flexible = products.flexible;
    let semantic = products.semantic;
    let relay = products.relay;
    let publisher = products.publisher;
    if let Some(identity) = identity {
        identity.validate_for_publication(
            &build.snapshot.catalog.snapshot_id,
            build.config.as_of_epoch,
        )?;
    }
    if let Some(activity) = activity {
        if identity.is_none() {
            return Err(Error::Validation(
                "fixed activity publication requires first-seen identity".to_owned(),
            ));
        }
        activity.validate_for_publication(
            &build.snapshot.catalog.snapshot_id,
            build.config.as_of_epoch,
        )?;
    }
    if let Some(cohort) = cohort {
        let (Some(identity), Some(activity)) = (identity, activity) else {
            return Err(Error::Validation(
                "cohort-retention publication requires identity and fixed activity".to_owned(),
            ));
        };
        if cohort.evidence.snapshot_id != build.snapshot.catalog.snapshot_id
            || cohort.evidence.as_of_epoch != build.config.as_of_epoch
            || cohort.evidence.identity_evidence_sha256 != identity.evidence_sha256
            || cohort.evidence.activity_evidence_sha256 != activity.evidence_sha256
        {
            return Err(Error::Validation(
                "cohort-retention evidence does not match its publication inputs".to_owned(),
            ));
        }
        cohort.validate_for_publication()?;
    }
    let validated_flexible = if let Some(flexible) = flexible {
        let Some(activity) = activity else {
            return Err(Error::Validation(
                "flexible-distinct publication requires fixed activity".to_owned(),
            ));
        };
        if flexible.product.evidence.snapshot_id != build.snapshot.catalog.snapshot_id
            || flexible.product.evidence.as_of_epoch != build.config.as_of_epoch
            || flexible.product.evidence.activity_evidence_sha256 != activity.evidence_sha256
            || flexible.product.evidence.activity_artifact != activity.evidence.activity_artifact
        {
            return Err(Error::Validation(
                "flexible-distinct evidence does not match its publication inputs".to_owned(),
            ));
        }
        Some(validate_flexible_distinct_publication(
            flexible.product,
            flexible.validation_evidence_path,
            flexible.validation_evidence_sha256,
        )?)
    } else {
        None
    };
    let validated_semantic = if let Some(semantic) = semantic {
        if semantic.product.evidence.snapshot_id != build.snapshot.catalog.snapshot_id
            || semantic.product.evidence.as_of_epoch != build.config.as_of_epoch
        {
            return Err(Error::Validation(
                "semantic evidence does not match its publication generation".to_owned(),
            ));
        }
        Some((
            validate_semantic_publication(semantic.product)?,
            validate_zap_distinct_publication(semantic.product, semantic.zap_distinct)?,
        ))
    } else {
        None
    };
    let validated_relay = if let Some(relay) = relay {
        if semantic.is_none() {
            return Err(Error::Validation(
                "relay publication requires the complete Slice 7 lane".to_owned(),
            ));
        }
        if relay.evidence.snapshot_id != build.snapshot.catalog.snapshot_id
            || relay.evidence.as_of_epoch != build.config.as_of_epoch
        {
            return Err(Error::Validation(
                "relay evidence does not match its publication generation".to_owned(),
            ));
        }
        Some(validate_relay_distribution_publication(relay)?)
    } else {
        None
    };
    let validated_publisher = if let Some(publisher) = publisher {
        let Some(activity) = activity else {
            return Err(Error::Validation(
                "publisher ranking publication requires fixed activity".to_owned(),
            ));
        };
        if relay.is_none()
            || publisher.evidence.snapshot_id != build.snapshot.catalog.snapshot_id
            || publisher.evidence.as_of_epoch != build.config.as_of_epoch
            || publisher.evidence.activity_evidence_sha256 != activity.evidence_sha256
            || publisher.evidence.activity_artifact_sha256
                != activity.evidence.activity_artifact.sha256
        {
            return Err(Error::Validation(
                "publisher ranking evidence does not match its publication generation".to_owned(),
            ));
        }
        Some(validate_publisher_ranking_publication(publisher)?)
    } else {
        None
    };
    client.batch_execute(SCHEMA_SQL)?;
    let run_id = run_id(build, identity, activity, cohort);
    let mut transaction = client.transaction()?;
    transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&PUBLICATION_LOCK_ID])?;

    let current_run_id = transaction
        .query_opt(
            "SELECT run_id FROM pensieve_analytics.current_run WHERE singleton = true FOR UPDATE",
            &[],
        )?
        .map(|row| row.get::<_, String>(0));
    if transaction
        .query_opt(
            "SELECT run_id FROM pensieve_analytics.runs WHERE run_id = $1",
            &[&run_id],
        )?
        .is_some()
    {
        if current_run_id.as_deref() == Some(run_id.as_str()) {
            reconcile_applied_objects(&mut transaction, &run_id, build)?;
            reconcile_published_identity(&mut transaction, &run_id, identity)?;
            reconcile_published_activity(&mut transaction, &run_id, activity)?;
            reconcile_published_cohort(&mut transaction, &run_id, cohort)?;
            if let (Some(flexible), Some(validated)) = (flexible, &validated_flexible) {
                publish_flexible_distinct_leaves_in_transaction(
                    &mut transaction,
                    &run_id,
                    flexible.product,
                    validated,
                )?;
            }
            publish_semantic_products(
                &mut transaction,
                &run_id,
                semantic,
                validated_semantic.as_ref(),
            )?;
            publish_relay_product(&mut transaction, &run_id, relay, validated_relay.as_ref())?;
            publish_publisher_product(
                &mut transaction,
                &run_id,
                publisher,
                validated_publisher.as_ref(),
            )?;
            transaction.commit()?;
            return Ok(PublishOutcome::AlreadyCurrent { run_id });
        }
        return Err(Error::StalePublishedRun(run_id));
    }
    if let Some(expected) = expected_previous_run_id
        && current_run_id.as_deref() != Some(expected)
    {
        return Err(Error::PublicationBaselineChanged {
            expected: expected.to_owned(),
            actual: current_run_id,
        });
    }

    let overview = build.overview()?;
    let eligible_pubkeys = identity
        .map(|product| product.evidence.eligible_pubkeys)
        .unwrap_or(0);
    let new_users_daily_rows = identity
        .map(|product| product.evidence.new_users_daily.len() as u64)
        .unwrap_or(0);
    let distinct_pubkeys_period_rows = activity
        .map(|product| product.evidence.distinct_period_rows)
        .unwrap_or(0);
    let active_users_period_rows = activity
        .map(|product| product.evidence.active_period_rows)
        .unwrap_or(0);
    let cohort_retention_rows = cohort
        .map(|product| product.evidence.period_rows)
        .unwrap_or(0);
    let validation = serde_json::to_value(ValidationRecord {
        event_daily_sum: build.summary.api_representable_events,
        event_daily_kind_sum: build.summary.api_representable_events,
        kind_all_time_sum: build.summary.logical_events,
        eligible_pubkeys,
        new_users_daily_sum: eligible_pubkeys,
        identity_evidence_sha256: identity.map(|product| product.evidence_sha256.clone()),
        identity_metric_sha256: identity.map(|product| product.evidence.metric_sha256.clone()),
        fixed_activity_evidence_sha256: activity.map(|product| product.evidence_sha256.clone()),
        fixed_activity_metric_sha256: activity
            .map(|product| product.evidence.metric_sha256.clone()),
        distinct_pubkeys_period_rows,
        active_users_period_rows,
        cohort_retention_evidence_sha256: cohort.map(|product| product.evidence_sha256.clone()),
        cohort_retention_metric_sha256: cohort
            .map(|product| product.evidence.metric_sha256.clone()),
        cohort_retention_rows,
        flexible_distinct_evidence_sha256: flexible
            .map(|publication| publication.product.evidence_sha256.clone()),
        flexible_distinct_validation_sha256: flexible
            .map(|publication| publication.validation_evidence_sha256.to_owned()),
        semantic_evidence_sha256: semantic
            .map(|publication| publication.product.evidence_sha256.clone()),
        zap_distinct_evidence_sha256: semantic
            .map(|publication| publication.zap_distinct.evidence_sha256.clone()),
        relay_distribution_evidence_sha256: relay.map(|product| product.evidence_sha256.clone()),
        publisher_ranking_evidence_sha256: publisher.map(|product| product.evidence_sha256.clone()),
        result: "passed",
    })
    .expect("serializing a fixed validation record cannot fail");
    transaction.execute(
        "
        INSERT INTO pensieve_analytics.runs (
            run_id,
            snapshot_id,
            previous_run_id,
            run_kind,
            query_version,
            code_version,
            as_of_epoch,
            started_at,
            completed_at,
            published_at,
            physical_rows,
            logical_events,
            duplicate_rows,
            api_representable_events,
            event_daily_rows,
            event_daily_kind_rows,
            kind_all_time_rows,
            eligible_pubkeys,
            new_users_daily_rows,
            distinct_pubkeys_period_rows,
            active_users_period_rows,
            cohort_retention_rows,
            validation
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, now(),
            $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22
        )
        ",
        &[
            &run_id,
            &build.snapshot.catalog.snapshot_id,
            &current_run_id,
            &run_kind,
            &query_version(identity, activity, cohort),
            &build.config.code_version,
            &to_i64("as_of_epoch", build.config.as_of_epoch)?,
            &started_at,
            &completed_at,
            &to_i64("physical_rows", build.summary.physical_rows)?,
            &to_i64("logical_events", build.summary.logical_events)?,
            &to_i64("duplicate_rows", build.summary.duplicate_rows)?,
            &to_i64(
                "api_representable_events",
                build.summary.api_representable_events,
            )?,
            &to_i64("event_daily_rows", build.summary.event_daily_rows)?,
            &to_i64("event_daily_kind_rows", build.summary.event_daily_kind_rows)?,
            &to_i64("kind_all_time_rows", build.summary.kind_all_time_rows)?,
            &to_i64("eligible_pubkeys", eligible_pubkeys)?,
            &to_i64("new_users_daily_rows", new_users_daily_rows)?,
            &to_i64("distinct_pubkeys_period_rows", distinct_pubkeys_period_rows)?,
            &to_i64("active_users_period_rows", active_users_period_rows)?,
            &to_i64("cohort_retention_rows", cohort_retention_rows)?,
            &validation,
        ],
    )?;
    insert_inputs(&mut transaction, &run_id, build)?;
    reconcile_applied_objects(&mut transaction, &run_id, build)?;
    transaction.execute(
        "
        INSERT INTO pensieve_analytics.overview (
            run_id,
            total_events,
            total_pubkeys,
            api_representable_events,
            earliest_event,
            latest_event,
            events_7d,
            events_per_hour_7d,
            kinds_30d
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ",
        &[
            &run_id,
            &to_i64("total_events", overview.total_events)?,
            &to_i64("total_pubkeys", eligible_pubkeys)?,
            &to_i64(
                "api_representable_events",
                overview.api_representable_events,
            )?,
            &i64::from(overview.earliest_event),
            &i64::from(overview.latest_event),
            &to_i64("events_7d", overview.events_7d)?,
            &overview.events_per_hour_7d,
            &to_i64("kinds_30d", overview.kinds_30d)?,
        ],
    )?;

    copy_event_daily(&mut transaction, &run_id, build)?;
    copy_event_daily_kind(&mut transaction, &run_id, build)?;
    copy_kind_all_time(&mut transaction, &run_id, build)?;
    if let Some(identity) = identity {
        copy_new_users_daily(&mut transaction, &run_id, identity)?;
    }
    if let Some(activity) = activity {
        copy_distinct_pubkeys_period(&mut transaction, &run_id, activity)?;
        copy_active_users_period(&mut transaction, &run_id, activity)?;
    }
    if let Some(cohort) = cohort {
        copy_cohort_retention(&mut transaction, &run_id, cohort)?;
    }
    reconcile_published_identity(&mut transaction, &run_id, identity)?;
    reconcile_published_activity(&mut transaction, &run_id, activity)?;
    reconcile_published_cohort(&mut transaction, &run_id, cohort)?;
    if let (Some(flexible), Some(validated)) = (flexible, &validated_flexible) {
        publish_flexible_distinct_leaves_in_transaction(
            &mut transaction,
            &run_id,
            flexible.product,
            validated,
        )?;
    }
    publish_semantic_products(
        &mut transaction,
        &run_id,
        semantic,
        validated_semantic.as_ref(),
    )?;
    publish_relay_product(&mut transaction, &run_id, relay, validated_relay.as_ref())?;
    publish_publisher_product(
        &mut transaction,
        &run_id,
        publisher,
        validated_publisher.as_ref(),
    )?;

    transaction.execute(
        "
        INSERT INTO pensieve_analytics.current_run (singleton, run_id)
        VALUES (true, $1)
        ON CONFLICT (singleton) DO UPDATE SET run_id = EXCLUDED.run_id
        ",
        &[&run_id],
    )?;
    transaction.commit()?;
    Ok(PublishOutcome::Published {
        run_id,
        previous_run_id: current_run_id,
    })
}

fn publish_relay_product(
    transaction: &mut impl GenericClient,
    run_id: &str,
    relay: Option<&BoundedRelayDistribution>,
    validated: Option<&ValidatedRelayDistributionPublication>,
) -> Result<()> {
    let Some(relay) = relay else {
        return Ok(());
    };
    let Some(validated) = validated else {
        return Err(Error::Validation(
            "relay publication is missing pre-transaction validation".to_owned(),
        ));
    };
    match publish_relay_distribution_in_transaction(transaction, run_id, relay, validated)? {
        RelayDistributionPublishOutcome::Published { .. }
        | RelayDistributionPublishOutcome::AlreadyPublished { .. } => Ok(()),
    }
}

fn publish_publisher_product(
    transaction: &mut impl GenericClient,
    run_id: &str,
    publisher: Option<&BoundedPublisherRanking>,
    validated: Option<&ValidatedPublisherRankingPublication>,
) -> Result<()> {
    let Some(publisher) = publisher else {
        return Ok(());
    };
    let Some(validated) = validated else {
        return Err(Error::Validation(
            "publisher publication is missing pre-transaction validation".to_owned(),
        ));
    };
    match publish_publisher_ranking_in_transaction(transaction, run_id, publisher, validated)? {
        PublisherRankingPublishOutcome::Published { .. }
        | PublisherRankingPublishOutcome::AlreadyPublished { .. } => Ok(()),
    }
}

fn publish_semantic_products(
    transaction: &mut impl GenericClient,
    run_id: &str,
    semantic: Option<SemanticPublication<'_>>,
    validated: Option<&(
        ValidatedSemanticPublication,
        ValidatedZapDistinctPublication,
    )>,
) -> Result<()> {
    let Some(semantic) = semantic else {
        return Ok(());
    };
    let Some((validated_semantic, validated_zap)) = validated else {
        return Err(Error::Validation(
            "semantic publication is missing pre-transaction validation".to_owned(),
        ));
    };
    let semantic_product_id = match publish_semantic_facts_in_transaction(
        transaction,
        run_id,
        semantic.product,
        validated_semantic,
    )? {
        SemanticPublishOutcome::Published { product_id }
        | SemanticPublishOutcome::AlreadyPublished { product_id } => product_id,
    };
    match publish_zap_distinct_in_transaction(
        transaction,
        &semantic_product_id,
        semantic.product,
        semantic.zap_distinct,
        validated_zap,
    )? {
        ZapDistinctPublishOutcome::Published { .. }
        | ZapDistinctPublishOutcome::AlreadyPublished { .. } => Ok(()),
    }
}

fn reconcile_applied_objects(
    transaction: &mut impl GenericClient,
    run_id: &str,
    build: &AnalyticsBuild,
) -> Result<()> {
    transaction.execute(
        "UPDATE pensieve_analytics.applied_objects SET active = false, updated_at = now() WHERE active = true",
        &[],
    )?;
    let statement = transaction.prepare(
        "
        INSERT INTO pensieve_analytics.applied_objects (
            object_key, work_unit_id, sha256, byte_size, physical_rows,
            min_created_at, max_created_at, first_applied_run_id,
            last_applied_run_id, active, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, true, now())
        ON CONFLICT (object_key) DO UPDATE SET
            last_applied_run_id = EXCLUDED.last_applied_run_id,
            active = true,
            updated_at = now()
        WHERE pensieve_analytics.applied_objects.work_unit_id = EXCLUDED.work_unit_id
          AND pensieve_analytics.applied_objects.sha256 = EXCLUDED.sha256
          AND pensieve_analytics.applied_objects.byte_size = EXCLUDED.byte_size
          AND pensieve_analytics.applied_objects.physical_rows = EXCLUDED.physical_rows
        ",
    )?;
    for object in build.snapshot.catalog.objects() {
        let changed = transaction.execute(
            &statement,
            &[
                &object.object_key,
                &object.work_unit_id,
                &object.sha256,
                &to_i64("object byte_size", object.byte_size)?,
                &to_i64("object row_count", object.row_count)?,
                &object.min_created_at,
                &object.max_created_at,
                &run_id,
            ],
        )?;
        if changed != 1 {
            return Err(Error::Validation(format!(
                "immutable applied object {} conflicts with its existing ledger identity",
                object.object_key
            )));
        }
    }
    Ok(())
}

fn insert_inputs(
    transaction: &mut impl GenericClient,
    run_id: &str,
    build: &AnalyticsBuild,
) -> Result<()> {
    let statement = transaction.prepare(
        "
        INSERT INTO pensieve_analytics.run_inputs (
            run_id,
            object_key,
            work_unit_id,
            sha256,
            byte_size,
            physical_rows
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ",
    )?;
    for object in build.snapshot.catalog.objects() {
        transaction.execute(
            &statement,
            &[
                &run_id,
                &object.object_key,
                &object.work_unit_id,
                &object.sha256,
                &to_i64("object byte_size", object.byte_size)?,
                &to_i64("object row_count", object.row_count)?,
            ],
        )?;
    }
    Ok(())
}

fn copy_event_daily(
    transaction: &mut impl GenericClient,
    run_id: &str,
    build: &AnalyticsBuild,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.event_daily (run_id, day, event_count)
        FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    build.for_each_event_daily(|row| {
        writeln!(writer, "{run_id},{},{}", row.day, row.event_count)?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    expect_copied("event_daily", inserted, build.summary.event_daily_rows)
}

fn copy_event_daily_kind(
    transaction: &mut impl GenericClient,
    run_id: &str,
    build: &AnalyticsBuild,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.event_daily_kind (run_id, day, kind, event_count)
        FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    build.for_each_event_daily_kind(|row| {
        writeln!(
            writer,
            "{run_id},{},{},{}",
            row.day, row.kind, row.event_count
        )?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    expect_copied(
        "event_daily_kind",
        inserted,
        build.summary.event_daily_kind_rows,
    )
}

fn copy_kind_all_time(
    transaction: &mut impl GenericClient,
    run_id: &str,
    build: &AnalyticsBuild,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.kind_all_time (run_id, kind, event_count)
        FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    build.for_each_kind_all_time(|row| {
        writeln!(writer, "{run_id},{},{}", row.kind, row.event_count)?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    expect_copied("kind_all_time", inserted, build.summary.kind_all_time_rows)
}

fn copy_new_users_daily(
    transaction: &mut impl GenericClient,
    run_id: &str,
    identity: &BoundedPubkeyFirstSeen,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.new_users_daily (run_id, day, new_pubkeys)
        FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    for row in &identity.evidence.new_users_daily {
        writeln!(writer, "{run_id},{},{}", row.day, row.new_pubkeys)?;
    }
    let inserted = writer.finish()?;
    expect_copied(
        "new_users_daily",
        inserted,
        identity.evidence.new_users_daily.len() as u64,
    )
}

fn copy_distinct_pubkeys_period(
    transaction: &mut impl GenericClient,
    run_id: &str,
    activity: &BoundedFixedActivity,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.distinct_pubkeys_period (
            run_id, grain, period_start, kind_key, unique_pubkeys
        ) FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    for row in &activity.evidence.distinct_pubkeys {
        writeln!(
            writer,
            "{run_id},{},{},{},{}",
            row.grain,
            row.period_start,
            row.kind.map_or(-1_i32, i32::from),
            row.unique_pubkeys
        )?;
    }
    let inserted = writer.finish()?;
    expect_copied(
        "distinct_pubkeys_period",
        inserted,
        activity.evidence.distinct_period_rows,
    )
}

fn copy_active_users_period(
    transaction: &mut impl GenericClient,
    run_id: &str,
    activity: &BoundedFixedActivity,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.active_users_period (
            run_id, grain, period_start, active_users, has_profile,
            has_follows_list, has_profile_and_follows_list, total_events
        ) FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    for row in &activity.evidence.active_users {
        writeln!(
            writer,
            "{run_id},{},{},{},{},{},{},{}",
            row.grain,
            row.period_start,
            row.active_users,
            row.has_profile,
            row.has_follows_list,
            row.has_profile_and_follows_list,
            row.total_events
        )?;
    }
    let inserted = writer.finish()?;
    expect_copied(
        "active_users_period",
        inserted,
        activity.evidence.active_period_rows,
    )
}

fn copy_cohort_retention(
    transaction: &mut impl GenericClient,
    run_id: &str,
    cohort: &BoundedCohortRetention,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "
        COPY pensieve_analytics.cohort_retention_period (
            run_id, grain, cohort_start, activity_period, active_pubkeys
        ) FROM STDIN WITH (FORMAT csv)
        ",
    )?;
    for row in &cohort.evidence.periods {
        writeln!(
            writer,
            "{run_id},{},{},{},{}",
            row.grain, row.cohort_start, row.activity_period, row.active_pubkeys
        )?;
    }
    let inserted = writer.finish()?;
    expect_copied(
        "cohort_retention_period",
        inserted,
        cohort.evidence.period_rows,
    )
}

fn reconcile_published_identity(
    transaction: &mut impl GenericClient,
    run_id: &str,
    identity: Option<&BoundedPubkeyFirstSeen>,
) -> Result<()> {
    let expected_pubkeys = identity
        .map(|product| product.evidence.eligible_pubkeys)
        .unwrap_or(0);
    let expected_rows = identity
        .map(|product| product.evidence.new_users_daily.len() as u64)
        .unwrap_or(0);
    let row = transaction.query_one(
        "
        SELECT runs.eligible_pubkeys, runs.new_users_daily_rows,
               overview.total_pubkeys,
               count(daily.day)::BIGINT,
               coalesce(sum(daily.new_pubkeys), 0)::BIGINT
        FROM pensieve_analytics.runs runs
        JOIN pensieve_analytics.overview overview USING (run_id)
        LEFT JOIN pensieve_analytics.new_users_daily daily USING (run_id)
        WHERE runs.run_id = $1
        GROUP BY runs.eligible_pubkeys, runs.new_users_daily_rows,
                 overview.total_pubkeys
        ",
        &[&run_id],
    )?;
    let actual = [
        from_i64("published eligible_pubkeys", row.get(0))?,
        from_i64("published new_users_daily_rows", row.get(1))?,
        from_i64("published total_pubkeys", row.get(2))?,
        from_i64("published new users row count", row.get(3))?,
        from_i64("published new users sum", row.get(4))?,
    ];
    if actual
        != [
            expected_pubkeys,
            expected_rows,
            expected_pubkeys,
            expected_rows,
            expected_pubkeys,
        ]
    {
        return Err(Error::Validation(format!(
            "published identity accounting {actual:?} does not match expected pubkeys {expected_pubkeys} and rows {expected_rows}"
        )));
    }
    Ok(())
}

fn reconcile_published_activity(
    transaction: &mut impl GenericClient,
    run_id: &str,
    activity: Option<&BoundedFixedActivity>,
) -> Result<()> {
    let expected_distinct_rows = activity
        .map(|product| product.evidence.distinct_period_rows)
        .unwrap_or(0);
    let expected_active_rows = activity
        .map(|product| product.evidence.active_period_rows)
        .unwrap_or(0);
    let expected_distinct_sum = activity.map_or(Ok(0), |product| {
        product
            .evidence
            .distinct_pubkeys
            .iter()
            .try_fold(0_u64, |sum, row| {
                sum.checked_add(row.unique_pubkeys)
                    .ok_or(Error::NumericOverflow {
                        field: "distinct pubkey sum",
                        value: row.unique_pubkeys,
                    })
            })
    })?;
    let expected_active_sum = activity.map_or(Ok(0), |product| {
        product
            .evidence
            .active_users
            .iter()
            .try_fold(0_u64, |sum, row| {
                sum.checked_add(row.active_users)
                    .ok_or(Error::NumericOverflow {
                        field: "active user sum",
                        value: row.active_users,
                    })
            })
    })?;
    let row = transaction.query_one(
        "
        SELECT runs.distinct_pubkeys_period_rows,
               runs.active_users_period_rows,
               (SELECT count(*)::BIGINT
                  FROM pensieve_analytics.distinct_pubkeys_period WHERE run_id = $1),
               (SELECT coalesce(sum(unique_pubkeys), 0)::BIGINT
                  FROM pensieve_analytics.distinct_pubkeys_period WHERE run_id = $1),
               (SELECT count(*)::BIGINT
                  FROM pensieve_analytics.active_users_period WHERE run_id = $1),
               (SELECT coalesce(sum(active_users), 0)::BIGINT
                  FROM pensieve_analytics.active_users_period WHERE run_id = $1)
        FROM pensieve_analytics.runs runs
        WHERE runs.run_id = $1
        ",
        &[&run_id],
    )?;
    let actual = [
        from_i64("published distinct metadata rows", row.get(0))?,
        from_i64("published active metadata rows", row.get(1))?,
        from_i64("published distinct row count", row.get(2))?,
        from_i64("published distinct sum", row.get(3))?,
        from_i64("published active row count", row.get(4))?,
        from_i64("published active sum", row.get(5))?,
    ];
    if actual
        != [
            expected_distinct_rows,
            expected_active_rows,
            expected_distinct_rows,
            expected_distinct_sum,
            expected_active_rows,
            expected_active_sum,
        ]
    {
        return Err(Error::Validation(format!(
            "published fixed-activity accounting {actual:?} does not match expected rows/sums"
        )));
    }
    Ok(())
}

fn reconcile_published_cohort(
    transaction: &mut impl GenericClient,
    run_id: &str,
    cohort: Option<&BoundedCohortRetention>,
) -> Result<()> {
    let expected_rows = cohort
        .map(|product| product.evidence.period_rows)
        .unwrap_or(0);
    let expected_sum = cohort
        .map(|product| product.evidence.active_pubkeys_sum)
        .unwrap_or(0);
    let row = transaction.query_one(
        "
        SELECT runs.cohort_retention_rows,
               (SELECT count(*)::BIGINT
                  FROM pensieve_analytics.cohort_retention_period WHERE run_id = $1),
               (SELECT coalesce(sum(active_pubkeys), 0)::BIGINT
                  FROM pensieve_analytics.cohort_retention_period WHERE run_id = $1),
               (SELECT count(*)::BIGINT
                  FROM pensieve_analytics.cohort_retention_period
                  WHERE run_id = $1 AND activity_period = cohort_start)
        FROM pensieve_analytics.runs runs
        WHERE runs.run_id = $1
        ",
        &[&run_id],
    )?;
    let actual_rows = from_i64("published cohort metadata rows", row.get(0))?;
    let actual_table_rows = from_i64("published cohort row count", row.get(1))?;
    let actual_sum = from_i64("published cohort active sum", row.get(2))?;
    let period_zero_rows = from_i64("published cohort period-zero rows", row.get(3))?;
    let expected_period_zero_rows = cohort.map_or(0, |product| {
        product
            .evidence
            .periods
            .iter()
            .filter(|period| period.activity_period == period.cohort_start)
            .count() as u64
    });
    if [actual_rows, actual_table_rows, actual_sum, period_zero_rows]
        != [
            expected_rows,
            expected_rows,
            expected_sum,
            expected_period_zero_rows,
        ]
    {
        return Err(Error::Validation(format!(
            "published cohort accounting [{actual_rows}, {actual_table_rows}, {actual_sum}, {period_zero_rows}] does not match expected rows/sum/period-zero"
        )));
    }
    Ok(())
}

fn expect_copied(table: &str, actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        return Err(Error::Validation(format!(
            "Postgres copied {actual} {table} rows, expected {expected}"
        )));
    }
    Ok(())
}

fn run_id(
    build: &AnalyticsBuild,
    identity: Option<&BoundedPubkeyFirstSeen>,
    activity: Option<&BoundedFixedActivity>,
    cohort: Option<&BoundedCohortRetention>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(build.snapshot.catalog.snapshot_id.as_bytes());
    digest.update([0]);
    digest.update(build.config.as_of_epoch.to_be_bytes());
    digest.update([0]);
    digest.update(query_version(identity, activity, cohort).as_bytes());
    digest.update([0]);
    digest.update(build.config.code_version.as_bytes());
    if let Some(identity) = identity {
        digest.update([0]);
        digest.update(identity.evidence_sha256.as_bytes());
        digest.update([0]);
        digest.update(identity.evidence.metric_sha256.as_bytes());
        digest.update([0]);
        digest.update(identity.evidence.final_artifact.sha256.as_bytes());
    }
    if let Some(activity) = activity {
        digest.update([0]);
        digest.update(activity.evidence_sha256.as_bytes());
        digest.update([0]);
        digest.update(activity.evidence.metric_sha256.as_bytes());
        digest.update([0]);
        digest.update(activity.evidence.activity_artifact.sha256.as_bytes());
        digest.update([0]);
        digest.update(activity.evidence.flags_artifact.sha256.as_bytes());
    }
    if let Some(cohort) = cohort {
        digest.update([0]);
        digest.update(cohort.evidence_sha256.as_bytes());
        digest.update([0]);
        digest.update(cohort.evidence.metric_sha256.as_bytes());
        digest.update([0]);
        digest.update(cohort.evidence.identity_evidence_sha256.as_bytes());
        digest.update([0]);
        digest.update(cohort.evidence.activity_evidence_sha256.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn query_version(
    identity: Option<&BoundedPubkeyFirstSeen>,
    activity: Option<&BoundedFixedActivity>,
    cohort: Option<&BoundedCohortRetention>,
) -> &'static str {
    if cohort.is_some() {
        COHORT_RETENTION_QUERY_VERSION
    } else if activity.is_some() {
        FIXED_ACTIVITY_QUERY_VERSION
    } else if identity.is_some() {
        IDENTITY_QUERY_VERSION
    } else {
        QUERY_VERSION
    }
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Validation(format!("{field} is negative: {value}")))
}
