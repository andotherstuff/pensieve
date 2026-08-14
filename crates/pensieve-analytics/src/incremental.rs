//! Transactional incremental updates to one durable DuckDB checkpoint.

use std::collections::BTreeMap;
use std::path::Path;

use duckdb::{Connection, OptionalExt, params};
use pensieve_lake::{CatalogObject, sha256_file};
use serde::Serialize;

use crate::build::{
    API_TIMESTAMP_MAX, configure_execution, replace_overview, scalar_u64, sql_string,
    validate_rollups_for_physical_rows,
};
use crate::{
    AnalyticsBuild, BuildConfig, CatalogDeltaPlan, Error, ObjectLocation, PlannedRunKind,
    QUERY_VERSION, ResolvedSnapshot, Result,
};

/// Work performed while advancing one DuckDB checkpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IncrementalSummary {
    /// Physical rows asserted by the added catalog objects.
    pub delta_physical_rows: u64,
    /// Distinct event IDs within the delta.
    pub delta_logical_events: u64,
    /// Duplicate physical rows within the delta.
    pub delta_internal_duplicates: u64,
    /// Delta IDs already represented by the checkpoint.
    pub existing_events: u64,
    /// Event IDs newly inserted into the checkpoint.
    pub inserted_events: u64,
    /// Whether the checkpoint already represented the target snapshot.
    pub already_applied: bool,
}

/// Resolve and validate the locally staged objects named by a delta plan.
pub fn resolve_delta_locations(
    plan: &CatalogDeltaPlan,
    local_root: impl AsRef<Path>,
) -> Result<Vec<ObjectLocation>> {
    let root = local_root.as_ref();
    plan.added_objects
        .iter()
        .map(|object| {
            let path = root.join(&object.object_key);
            if !path.is_file() {
                return Err(Error::MissingLocalObject(path));
            }
            let actual = std::fs::metadata(&path)?.len();
            if actual != object.byte_size {
                return Err(Error::InvalidIncrementalPlan(format!(
                    "staged object {} has {actual} bytes, expected {}",
                    object.object_key, object.byte_size
                )));
            }
            let actual_sha256 = sha256_file(&path)?;
            if actual_sha256 != object.sha256 {
                return Err(Error::InvalidIncrementalPlan(format!(
                    "staged object {} has SHA-256 {actual_sha256}, expected {}",
                    object.object_key, object.sha256
                )));
            }
            Ok(ObjectLocation::Local(path))
        })
        .collect()
}

/// Advance a completed DuckDB checkpoint using only verified delta objects.
///
/// All persistent DuckDB changes occur in one transaction. `dry_run` performs
/// the exact scans and joins through new-event selection, then rolls back
/// without changing the checkpoint.
pub fn apply_incremental(
    work_database: impl AsRef<Path>,
    target: ResolvedSnapshot,
    plan: &CatalogDeltaPlan,
    delta_locations: &[ObjectLocation],
    config: BuildConfig,
    baseline_as_of_epoch: u64,
    dry_run: bool,
) -> Result<(AnalyticsBuild, IncrementalSummary)> {
    if config.as_of_epoch > API_TIMESTAMP_MAX {
        return Err(Error::Validation(format!(
            "as_of {} exceeds the V1 API timestamp maximum {}",
            config.as_of_epoch, API_TIMESTAMP_MAX
        )));
    }
    validate_plan(&target, plan, delta_locations)?;
    let connection = Connection::open(work_database)?;
    configure_execution(&connection, &config)?;
    connection.execute_batch("SET TimeZone = 'UTC'; SET preserve_insertion_order = false")?;

    let state = checkpoint_state(&connection)?;
    if let Some(state) = &state {
        if state.snapshot_id == target.catalog.snapshot_id {
            if state.as_of_epoch != config.as_of_epoch || state.query_version != QUERY_VERSION {
                return Err(Error::InvalidIncrementalPlan(format!(
                    "checkpoint target was materialized with as_of {} and query version {}, requested {} and {}",
                    state.as_of_epoch, state.query_version, config.as_of_epoch, QUERY_VERSION
                )));
            }
            let summary = validate_rollups_for_physical_rows(
                &connection,
                target.catalog.totals().physical_rows,
            )?;
            return Ok((
                AnalyticsBuild {
                    connection,
                    snapshot: target,
                    config,
                    summary,
                },
                IncrementalSummary {
                    delta_physical_rows: plan.added_physical_rows,
                    delta_logical_events: 0,
                    delta_internal_duplicates: 0,
                    existing_events: 0,
                    inserted_events: 0,
                    already_applied: true,
                },
            ));
        }
        let expected = plan
            .previous_snapshot_id
            .as_deref()
            .expect("validated incremental plans have a previous snapshot");
        if state.snapshot_id != expected {
            return Err(Error::CheckpointSnapshotMismatch {
                actual: state.snapshot_id.clone(),
                expected: expected.to_owned(),
            });
        }
    }

    connection.execute_batch("BEGIN TRANSACTION")?;
    let operation = (|| {
        initialize_checkpoint_state(
            &connection,
            state.as_ref(),
            plan,
            &target,
            baseline_as_of_epoch,
        )?;
        materialize_delta(&connection, delta_locations)?;
        let conflicting_delta_ids = scalar_u64(
            &connection,
            "
            SELECT count(*) FROM (
                SELECT id FROM delta_events GROUP BY id HAVING count(*) != 1
            )
            ",
        )?;
        if conflicting_delta_ids != 0 {
            return Err(Error::ConflictingDeltaEvents(conflicting_delta_ids));
        }
        let delta_logical_events = scalar_u64(&connection, "SELECT count(*) FROM delta_events")?;
        let delta_internal_duplicates = plan
            .added_physical_rows
            .checked_sub(delta_logical_events)
            .ok_or_else(|| {
                Error::InvalidIncrementalPlan(format!(
                    "delta catalog claims {} physical rows but contains {delta_logical_events} distinct IDs",
                    plan.added_physical_rows
                ))
            })?;

        connection.execute_batch(
            "
            CREATE TEMP TABLE matched_delta_events AS
            SELECT
                delta.id,
                delta.created_at,
                delta.kind,
                existing.created_at AS existing_created_at,
                existing.kind AS existing_kind
            FROM canonical_events existing
            INNER JOIN delta_events delta ON existing.id = delta.id;
            ",
        )?;
        let conflicts = scalar_u64(
            &connection,
            "
            SELECT count(*) FROM matched_delta_events
            WHERE created_at != existing_created_at OR kind != existing_kind
            ",
        )?;
        if conflicts != 0 {
            return Err(Error::ConflictingDeltaEvents(conflicts));
        }
        let existing_events = scalar_u64(&connection, "SELECT count(*) FROM matched_delta_events")?;
        connection.execute_batch(
            "
            CREATE TEMP TABLE new_events AS
            SELECT delta.*
            FROM delta_events delta
            ANTI JOIN matched_delta_events existing USING (id);
            ",
        )?;
        let inserted_events = scalar_u64(&connection, "SELECT count(*) FROM new_events")?;
        if inserted_events
            .checked_add(existing_events)
            .ok_or(Error::PlanOverflow("incremental logical events"))?
            != delta_logical_events
        {
            return Err(Error::Validation(
                "new and existing delta IDs do not reconcile".to_owned(),
            ));
        }
        let incremental = IncrementalSummary {
            delta_physical_rows: plan.added_physical_rows,
            delta_logical_events,
            delta_internal_duplicates,
            existing_events,
            inserted_events,
            already_applied: false,
        };
        if dry_run {
            return Ok((None, incremental));
        }

        connection.execute_batch(
            "INSERT INTO canonical_events SELECT id, created_at, kind FROM new_events",
        )?;
        replace_additive_rollups(&connection)?;
        replace_overview(&connection, config.as_of_epoch)?;
        connection.execute(
            "
            UPDATE analytics_state SET
                snapshot_id = ?, as_of_epoch = ?, query_version = ?
            WHERE singleton = true
            ",
            params![
                target.catalog.snapshot_id,
                config.as_of_epoch,
                QUERY_VERSION
            ],
        )?;
        let summary =
            validate_rollups_for_physical_rows(&connection, target.catalog.totals().physical_rows)?;
        Ok((Some(summary), incremental))
    })();

    match operation {
        Ok((summary, incremental)) if dry_run => {
            connection.execute_batch("ROLLBACK")?;
            debug_assert!(summary.is_none());
            let baseline_physical_rows = target
                .catalog
                .totals()
                .physical_rows
                .checked_sub(plan.added_physical_rows)
                .ok_or(Error::PlanOverflow("baseline physical rows"))?;
            let summary = validate_rollups_for_physical_rows(&connection, baseline_physical_rows)?;
            Ok((
                AnalyticsBuild {
                    connection,
                    snapshot: target,
                    config,
                    summary,
                },
                incremental,
            ))
        }
        Ok((Some(summary), incremental)) => {
            connection.execute_batch("COMMIT")?;
            Ok((
                AnalyticsBuild {
                    connection,
                    snapshot: target,
                    config,
                    summary,
                },
                incremental,
            ))
        }
        Ok((None, _)) => unreachable!("non-dry runs always produce a summary"),
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

#[derive(Debug)]
struct CheckpointState {
    snapshot_id: String,
    as_of_epoch: u64,
    query_version: String,
}

fn checkpoint_state(connection: &Connection) -> Result<Option<CheckpointState>> {
    let exists: bool = connection.query_row(
        "SELECT count(*) != 0 FROM information_schema.tables WHERE table_name = 'analytics_state'",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT snapshot_id, as_of_epoch, query_version FROM analytics_state WHERE singleton = true",
            [],
            |row| {
                Ok(CheckpointState {
                    snapshot_id: row.get(0)?,
                    as_of_epoch: row.get(1)?,
                    query_version: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Error::from)
}

fn initialize_checkpoint_state(
    connection: &Connection,
    state: Option<&CheckpointState>,
    plan: &CatalogDeltaPlan,
    target: &ResolvedSnapshot,
    baseline_as_of_epoch: u64,
) -> Result<()> {
    if state.is_some() {
        return Ok(());
    }
    let baseline_physical_rows = target
        .catalog
        .totals()
        .physical_rows
        .checked_sub(plan.added_physical_rows)
        .ok_or(Error::PlanOverflow("baseline physical rows"))?;
    validate_rollups_for_physical_rows(connection, baseline_physical_rows)?;
    connection.execute_batch(
        "
        CREATE TABLE analytics_state (
            singleton BOOLEAN PRIMARY KEY,
            snapshot_id VARCHAR NOT NULL,
            as_of_epoch UBIGINT NOT NULL,
            query_version VARCHAR NOT NULL,
            CHECK (singleton)
        );
        ",
    )?;
    connection.execute(
        "INSERT INTO analytics_state VALUES (true, ?, ?, ?)",
        params![
            plan.previous_snapshot_id
                .as_deref()
                .expect("validated incremental plans have a previous snapshot"),
            baseline_as_of_epoch,
            QUERY_VERSION
        ],
    )?;
    Ok(())
}

fn materialize_delta(connection: &Connection, locations: &[ObjectLocation]) -> Result<()> {
    let paths = locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch(&format!(
        "
        CREATE TEMP TABLE delta_events AS
        SELECT DISTINCT id, created_at, kind
        FROM read_parquet([{paths}], union_by_name = false);
        "
    ))?;
    Ok(())
}

fn replace_additive_rollups(connection: &Connection) -> Result<()> {
    connection.execute_batch(&format!(
        "
        CREATE TABLE rollup_event_daily_next AS
        SELECT day, sum(event_count)::UBIGINT AS event_count
        FROM (
            SELECT day, event_count FROM rollup_event_daily
            UNION ALL
            SELECT DATE '1970-01-01' + CAST(created_at // 86400 AS INTEGER),
                   count(*)::UBIGINT
            FROM new_events
            WHERE created_at <= {API_TIMESTAMP_MAX}
            GROUP BY 1
        ) GROUP BY day ORDER BY day;

        CREATE TABLE rollup_event_daily_kind_next AS
        SELECT day, kind, sum(event_count)::UBIGINT AS event_count
        FROM (
            SELECT day, kind, event_count FROM rollup_event_daily_kind
            UNION ALL
            SELECT DATE '1970-01-01' + CAST(created_at // 86400 AS INTEGER),
                   kind, count(*)::UBIGINT
            FROM new_events
            WHERE created_at <= {API_TIMESTAMP_MAX}
            GROUP BY 1, 2
        ) GROUP BY day, kind ORDER BY day, kind;

        CREATE TABLE rollup_kind_all_time_next AS
        SELECT kind, sum(event_count)::UBIGINT AS event_count
        FROM (
            SELECT kind, event_count FROM rollup_kind_all_time
            UNION ALL
            SELECT kind, count(*)::UBIGINT FROM new_events GROUP BY kind
        ) GROUP BY kind ORDER BY kind;

        DROP TABLE rollup_event_daily;
        ALTER TABLE rollup_event_daily_next RENAME TO rollup_event_daily;
        DROP TABLE rollup_event_daily_kind;
        ALTER TABLE rollup_event_daily_kind_next RENAME TO rollup_event_daily_kind;
        DROP TABLE rollup_kind_all_time;
        ALTER TABLE rollup_kind_all_time_next RENAME TO rollup_kind_all_time;
        "
    ))?;
    Ok(())
}

fn validate_plan(
    target: &ResolvedSnapshot,
    plan: &CatalogDeltaPlan,
    locations: &[ObjectLocation],
) -> Result<()> {
    if plan.run_kind != PlannedRunKind::Incremental
        || plan.previous_run_id.is_none()
        || plan.previous_snapshot_id.is_none()
        || !plan.removed_objects.is_empty()
    {
        return Err(Error::InvalidIncrementalPlan(
            "executor requires an incremental plan with a published baseline and no removals"
                .to_owned(),
        ));
    }
    if plan.snapshot_id != target.catalog.snapshot_id {
        return Err(Error::InvalidIncrementalPlan(format!(
            "plan targets {}, selected catalog is {}",
            plan.snapshot_id, target.catalog.snapshot_id
        )));
    }
    if locations.len() != plan.added_objects.len() {
        return Err(Error::InvalidIncrementalPlan(format!(
            "resolved {} delta locations for {} objects",
            locations.len(),
            plan.added_objects.len()
        )));
    }
    let target_objects: BTreeMap<_, _> = target
        .catalog
        .objects()
        .iter()
        .map(|object| (&object.object_key, object))
        .collect();
    for object in &plan.added_objects {
        if target_objects.get(&object.object_key) != Some(&object) {
            return Err(Error::InvalidIncrementalPlan(format!(
                "added object {} does not exactly match the target catalog",
                object.object_key
            )));
        }
    }
    let object_count = u64::try_from(target.catalog.objects().len())
        .map_err(|_| Error::PlanOverflow("target object count"))?;
    let added_count = u64::try_from(plan.added_objects.len())
        .map_err(|_| Error::PlanOverflow("added object count"))?;
    if plan
        .unchanged_objects
        .checked_add(added_count)
        .ok_or(Error::PlanOverflow("target object count"))?
        != object_count
    {
        return Err(Error::InvalidIncrementalPlan(
            "unchanged and added object counts do not cover the target catalog".to_owned(),
        ));
    }
    validate_added_totals(&plan.added_objects, plan)?;
    Ok(())
}

fn validate_added_totals(objects: &[CatalogObject], plan: &CatalogDeltaPlan) -> Result<()> {
    let (bytes, rows) = objects
        .iter()
        .try_fold((0_u64, 0_u64), |(bytes, rows), object| {
            Ok::<_, Error>((
                bytes
                    .checked_add(object.byte_size)
                    .ok_or(Error::PlanOverflow("added bytes"))?,
                rows.checked_add(object.row_count)
                    .ok_or(Error::PlanOverflow("added rows"))?,
            ))
        })?;
    if bytes != plan.added_bytes || rows != plan.added_physical_rows {
        return Err(Error::InvalidIncrementalPlan(
            "added object totals do not match plan accounting".to_owned(),
        ));
    }
    Ok(())
}
