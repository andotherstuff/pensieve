//! Transactional Postgres publication for completed Slice A products.

use std::io::Write;

use chrono::{DateTime, Utc};
use postgres::{Client, GenericClient};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    AnalyticsBuild, BoundedPubkeyFirstSeen, Error, IDENTITY_QUERY_VERSION, QUERY_VERSION, Result,
    schema::SCHEMA_SQL,
};

const PUBLICATION_LOCK_ID: i64 = 8_056_718_693_194_101_224;

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

#[derive(Serialize)]
struct ValidationRecord {
    event_daily_sum: u64,
    event_daily_kind_sum: u64,
    kind_all_time_sum: u64,
    eligible_pubkeys: u64,
    new_users_daily_sum: u64,
    identity_evidence_sha256: Option<String>,
    identity_metric_sha256: Option<String>,
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
        None,
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
        Some(identity),
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
        None,
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
        Some(identity),
    )
}

fn publish_kind(
    client: &mut Client,
    build: &AnalyticsBuild,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    run_kind: &'static str,
    expected_previous_run_id: Option<&str>,
    identity: Option<&BoundedPubkeyFirstSeen>,
) -> Result<PublishOutcome> {
    if let Some(identity) = identity {
        identity.validate_for_publication(
            &build.snapshot.catalog.snapshot_id,
            build.config.as_of_epoch,
        )?;
    }
    client.batch_execute(SCHEMA_SQL)?;
    let run_id = run_id(build, identity);
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
    let validation = serde_json::to_value(ValidationRecord {
        event_daily_sum: build.summary.api_representable_events,
        event_daily_kind_sum: build.summary.api_representable_events,
        kind_all_time_sum: build.summary.logical_events,
        eligible_pubkeys,
        new_users_daily_sum: eligible_pubkeys,
        identity_evidence_sha256: identity.map(|product| product.evidence_sha256.clone()),
        identity_metric_sha256: identity.map(|product| product.evidence.metric_sha256.clone()),
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
            validation
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, now(),
            $10, $11, $12, $13, $14, $15, $16, $17, $18, $19
        )
        ",
        &[
            &run_id,
            &build.snapshot.catalog.snapshot_id,
            &current_run_id,
            &run_kind,
            &query_version(identity),
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
    reconcile_published_identity(&mut transaction, &run_id, identity)?;

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

fn expect_copied(table: &str, actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        return Err(Error::Validation(format!(
            "Postgres copied {actual} {table} rows, expected {expected}"
        )));
    }
    Ok(())
}

fn run_id(build: &AnalyticsBuild, identity: Option<&BoundedPubkeyFirstSeen>) -> String {
    let mut digest = Sha256::new();
    digest.update(build.snapshot.catalog.snapshot_id.as_bytes());
    digest.update([0]);
    digest.update(build.config.as_of_epoch.to_be_bytes());
    digest.update([0]);
    digest.update(query_version(identity).as_bytes());
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
    hex::encode(digest.finalize())
}

fn query_version(identity: Option<&BoundedPubkeyFirstSeen>) -> &'static str {
    if identity.is_some() {
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
