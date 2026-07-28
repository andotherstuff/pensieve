//! Transactional Postgres publication for completed Slice A products.

use std::io::Write;

use chrono::{DateTime, Utc};
use postgres::{Client, GenericClient};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{AnalyticsBuild, Error, QUERY_VERSION, Result};

const SCHEMA_SQL: &str = include_str!("../../../docs/postgres/001_analytics_slice_a.sql");
const PUBLICATION_LOCK_ID: i64 = 8_056_718_693_194_101_224;

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
    client.batch_execute(SCHEMA_SQL)?;
    let run_id = run_id(build);
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
            transaction.commit()?;
            return Ok(PublishOutcome::AlreadyCurrent { run_id });
        }
        return Err(Error::StalePublishedRun(run_id));
    }

    let overview = build.overview()?;
    let validation = serde_json::to_value(ValidationRecord {
        event_daily_sum: build.summary.api_representable_events,
        event_daily_kind_sum: build.summary.api_representable_events,
        kind_all_time_sum: build.summary.logical_events,
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
            validation
        )
        VALUES (
            $1, $2, $3, 'full_rebuild', $4, $5, $6, $7, $8, now(),
            $9, $10, $11, $12, $13, $14, $15, $16
        )
        ",
        &[
            &run_id,
            &build.snapshot.catalog.snapshot_id,
            &current_run_id,
            &QUERY_VERSION,
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
            &validation,
        ],
    )?;
    insert_inputs(&mut transaction, &run_id, build)?;
    transaction.execute(
        "
        INSERT INTO pensieve_analytics.overview (
            run_id,
            total_events,
            api_representable_events,
            earliest_event,
            latest_event,
            events_7d,
            events_per_hour_7d,
            kinds_30d
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ",
        &[
            &run_id,
            &to_i64("total_events", overview.total_events)?,
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

fn expect_copied(table: &str, actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        return Err(Error::Validation(format!(
            "Postgres copied {actual} {table} rows, expected {expected}"
        )));
    }
    Ok(())
}

fn run_id(build: &AnalyticsBuild) -> String {
    let mut digest = Sha256::new();
    digest.update(build.snapshot.catalog.snapshot_id.as_bytes());
    digest.update([0]);
    digest.update(build.config.as_of_epoch.to_be_bytes());
    digest.update([0]);
    digest.update(QUERY_VERSION.as_bytes());
    digest.update([0]);
    digest.update(build.config.code_version.as_bytes());
    hex::encode(digest.finalize())
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}
