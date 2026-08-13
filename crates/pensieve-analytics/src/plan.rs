//! Catalog difference planning for bounded incremental analytics work.

use std::collections::BTreeMap;

use pensieve_lake::{ActiveRawSnapshot, CatalogObject};
use postgres::{Client, GenericClient};
use serde::{Deserialize, Serialize};

use crate::{Error, QUERY_VERSION, Result, schema::SCHEMA_SQL};

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

/// Load the current applied-object ledger and compare it with `snapshot`.
///
/// This is read-only apart from idempotent schema installation. Publication is
/// solely responsible for advancing the ledger.
pub fn plan_catalog_delta(
    client: &mut Client,
    snapshot: &ActiveRawSnapshot,
) -> Result<CatalogDeltaPlan> {
    client.batch_execute(SCHEMA_SQL)?;
    let baseline = load_baseline(client)?;
    plan_from_baseline(snapshot, baseline)
}

#[derive(Debug)]
struct Baseline {
    run_id: String,
    snapshot_id: String,
    query_version: String,
    objects: Vec<AppliedObject>,
}

fn load_baseline(client: &mut impl GenericClient) -> Result<Option<Baseline>> {
    let Some(run) = client.query_opt(
        "SELECT run_id, snapshot_id, query_version FROM pensieve_analytics.current_run_metadata",
        &[],
    )?
    else {
        return Ok(None);
    };
    let run_id = run.get::<_, String>(0);
    let snapshot_id = run.get::<_, String>(1);
    let query_version = run.get::<_, String>(2);
    let objects = client
        .query(
            "
        SELECT inputs.object_key, inputs.work_unit_id, inputs.sha256,
               inputs.byte_size, inputs.physical_rows,
               applied.min_created_at, applied.max_created_at
        FROM pensieve_analytics.run_inputs inputs
        LEFT JOIN pensieve_analytics.applied_objects applied
          ON applied.object_key = inputs.object_key
         AND applied.active = true
         AND applied.last_applied_run_id = inputs.run_id
         AND applied.work_unit_id = inputs.work_unit_id
         AND applied.sha256 = inputs.sha256
         AND applied.byte_size = inputs.byte_size
         AND applied.physical_rows = inputs.physical_rows
        WHERE inputs.run_id = $1
        ORDER BY inputs.object_key
        ",
            &[&run_id],
        )?
        .iter()
        .map(applied_object_from_row)
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(Baseline {
        run_id,
        snapshot_id,
        query_version,
        objects,
    }))
}

fn applied_object_from_row(row: &postgres::Row) -> Result<AppliedObject> {
    Ok(AppliedObject {
        object_key: row.get(0),
        work_unit_id: row.get(1),
        sha256: row.get(2),
        byte_size: from_i64("object byte_size", row.get(3))?,
        physical_rows: from_i64("object physical_rows", row.get(4))?,
        min_created_at: row.get(5),
        max_created_at: row.get(6),
    })
}

fn plan_from_baseline(
    snapshot: &ActiveRawSnapshot,
    baseline: Option<Baseline>,
) -> Result<CatalogDeltaPlan> {
    let Some(baseline) = baseline else {
        return finish_plan(
            snapshot,
            None,
            None,
            snapshot.objects().to_vec(),
            Vec::new(),
            0,
        );
    };
    if baseline.query_version != QUERY_VERSION {
        return finish_plan(
            snapshot,
            None,
            None,
            snapshot.objects().to_vec(),
            Vec::new(),
            0,
        );
    }
    let mut previous: BTreeMap<_, _> = baseline
        .objects
        .into_iter()
        .map(|object| (object.object_key.clone(), object))
        .collect();
    let mut added = Vec::new();
    let mut unchanged = 0;
    for object in snapshot.objects() {
        match previous.remove(&object.object_key) {
            None => added.push(object.clone()),
            Some(applied) if same_identity(object, &applied) => unchanged += 1,
            Some(applied) => {
                return Err(Error::ImmutableObjectChanged {
                    object_key: object.object_key.clone(),
                    previous_sha256: applied.sha256,
                    selected_sha256: object.sha256.clone(),
                });
            }
        }
    }
    finish_plan(
        snapshot,
        Some(baseline.run_id),
        Some(baseline.snapshot_id),
        added,
        previous.into_values().collect(),
        unchanged,
    )
}

fn same_identity(object: &CatalogObject, applied: &AppliedObject) -> bool {
    object.work_unit_id == applied.work_unit_id
        && object.sha256 == applied.sha256
        && object.byte_size == applied.byte_size
        && object.row_count == applied.physical_rows
}

fn finish_plan(
    snapshot: &ActiveRawSnapshot,
    previous_run_id: Option<String>,
    previous_snapshot_id: Option<String>,
    added_objects: Vec<CatalogObject>,
    removed_objects: Vec<AppliedObject>,
    unchanged_objects: u64,
) -> Result<CatalogDeltaPlan> {
    let run_kind = if previous_run_id.is_none() {
        PlannedRunKind::FullRebuild
    } else if !removed_objects.is_empty() {
        PlannedRunKind::AffectedPeriodRebuild
    } else if !added_objects.is_empty() {
        PlannedRunKind::Incremental
    } else {
        PlannedRunKind::NoChange
    };
    let added_bytes = added_objects
        .iter()
        .try_fold(0_u64, |total, object| total.checked_add(object.byte_size))
        .ok_or(Error::PlanOverflow("added object bytes"))?;
    let added_physical_rows = added_objects
        .iter()
        .try_fold(0_u64, |total, object| total.checked_add(object.row_count))
        .ok_or(Error::PlanOverflow("added physical rows"))?;
    let ranges = added_objects
        .iter()
        .map(|object| (&object.min_created_at, &object.max_created_at))
        .chain(
            removed_objects
                .iter()
                .map(|object| (&object.min_created_at, &object.max_created_at)),
        )
        .collect::<Vec<_>>();
    let affected_range_complete = ranges
        .iter()
        .all(|(minimum, maximum)| minimum.is_some() && maximum.is_some());
    let affected_min_created_at = ranges
        .iter()
        .filter_map(|(minimum, _)| minimum.as_deref())
        .map(parse_timestamp)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min()
        .map(|value| value.to_string());
    let affected_max_created_at = ranges
        .iter()
        .filter_map(|(_, maximum)| maximum.as_deref())
        .map(parse_timestamp)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .map(|value| value.to_string());
    Ok(CatalogDeltaPlan {
        snapshot_id: snapshot.snapshot_id.clone(),
        previous_run_id,
        previous_snapshot_id,
        run_kind,
        added_objects,
        removed_objects,
        unchanged_objects,
        added_bytes,
        added_physical_rows,
        affected_min_created_at,
        affected_max_created_at,
        affected_range_complete,
    })
}

fn parse_timestamp(value: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|_| Error::InvalidLedgerTimestamp(value.to_owned()))
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::NegativeLedgerValue { field, value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pensieve_lake::{
        ActiveRawFragment, Inventory, ObjectKind, ObjectRecord, ObjectState, WorkState,
        WorkUnitRegistration, merge_active_raw_fragments,
    };
    use std::path::{Path, PathBuf};

    fn snapshot(objects: &[(&str, &str, u64, u64, u64, u64)]) -> ActiveRawSnapshot {
        let mut inventory = Inventory::open_in_memory().expect("inventory");
        for (index, (key, sha256, bytes, rows, minimum, maximum)) in objects.iter().enumerate() {
            let work_id = format!("work-{index}");
            inventory
                .ensure_work_unit(&WorkUnitRegistration {
                    id: &work_id,
                    source_path: &PathBuf::from(format!("/source/{index}.gz")),
                    source_bytes: *bytes,
                    source_sha256: &"11".repeat(32),
                    target_uncompressed_bytes: 1,
                    max_event_bytes: 1,
                    object_prefix: "nostr/v1",
                    writer_version: "test",
                })
                .expect("register");
            inventory
                .transition_work(&work_id, WorkState::Writing, None)
                .expect("writing");
            inventory
                .record_validated_objects(
                    &work_id,
                    *rows,
                    *rows,
                    0,
                    &[ObjectRecord {
                        object_key: (*key).to_owned(),
                        work_unit_id: work_id.clone(),
                        part_number: 0,
                        kind: ObjectKind::Parquet,
                        state: ObjectState::Validated,
                        local_path: Path::new("/unused").to_owned(),
                        byte_size: *bytes,
                        sha256: (*sha256).to_owned(),
                        writer_version: "test".to_owned(),
                        row_count: *rows,
                        min_created_at: Some(*minimum),
                        max_created_at: Some(*maximum),
                    }],
                )
                .expect("objects");
            inventory
                .transition_work(&work_id, WorkState::Uploading, None)
                .expect("uploading");
            inventory
                .mark_object_uploaded(key)
                .expect("uploaded object");
            inventory
                .transition_work(&work_id, WorkState::Uploaded, None)
                .expect("uploaded");
            inventory.activate_work_unit(&work_id).expect("activate");
        }
        let fragment =
            ActiveRawFragment::export(&mut inventory, "test", "s3+https://example.test/bucket")
                .expect("fragment");
        merge_active_raw_fragments([fragment]).expect("snapshot")
    }

    fn applied(object: &CatalogObject) -> AppliedObject {
        AppliedObject {
            object_key: object.object_key.clone(),
            work_unit_id: object.work_unit_id.clone(),
            sha256: object.sha256.clone(),
            byte_size: object.byte_size,
            physical_rows: object.row_count,
            min_created_at: object.min_created_at.clone(),
            max_created_at: object.max_created_at.clone(),
        }
    }

    #[test]
    fn append_only_catalog_plans_only_new_objects() {
        let selected = snapshot(&[
            ("nostr/v1/a.parquet", &"aa".repeat(32), 10, 2, 100, 200),
            ("nostr/v1/b.parquet", &"bb".repeat(32), 20, 3, 50, 300),
        ]);
        let baseline = Baseline {
            run_id: "run-1".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            query_version: QUERY_VERSION.to_owned(),
            objects: vec![applied(&selected.objects()[0])],
        };
        let plan = plan_from_baseline(&selected, Some(baseline)).expect("plan");
        assert_eq!(plan.run_kind, PlannedRunKind::Incremental);
        assert_eq!(plan.unchanged_objects, 1);
        assert_eq!(plan.added_objects.len(), 1);
        assert_eq!(plan.added_bytes, 20);
        assert_eq!(plan.added_physical_rows, 3);
        assert_eq!(plan.affected_min_created_at.as_deref(), Some("50"));
        assert_eq!(plan.affected_max_created_at.as_deref(), Some("300"));
        assert!(plan.affected_range_complete);
    }

    #[test]
    fn removal_requires_affected_period_rebuild() {
        let selected = snapshot(&[("nostr/v1/b.parquet", &"bb".repeat(32), 20, 3, 50, 300)]);
        let removed_catalog =
            snapshot(&[("nostr/v1/a.parquet", &"aa".repeat(32), 10, 2, 100, 200)]);
        let baseline = Baseline {
            run_id: "run-1".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            query_version: QUERY_VERSION.to_owned(),
            objects: vec![applied(&removed_catalog.objects()[0])],
        };
        let plan = plan_from_baseline(&selected, Some(baseline)).expect("plan");
        assert_eq!(plan.run_kind, PlannedRunKind::AffectedPeriodRebuild);
        assert_eq!(plan.added_objects.len(), 1);
        assert_eq!(plan.removed_objects.len(), 1);
        assert_eq!(plan.affected_min_created_at.as_deref(), Some("50"));
        assert_eq!(plan.affected_max_created_at.as_deref(), Some("300"));
    }

    #[test]
    fn changed_immutable_key_is_rejected() {
        let selected = snapshot(&[("nostr/v1/a.parquet", &"aa".repeat(32), 10, 2, 100, 200)]);
        let mut old = applied(&selected.objects()[0]);
        old.sha256 = "bb".repeat(32);
        let error = plan_from_baseline(
            &selected,
            Some(Baseline {
                run_id: "run-1".to_owned(),
                snapshot_id: "snapshot-1".to_owned(),
                query_version: QUERY_VERSION.to_owned(),
                objects: vec![old],
            }),
        )
        .expect_err("changed key must fail");
        assert!(matches!(error, Error::ImmutableObjectChanged { .. }));
    }

    #[test]
    fn query_version_change_requires_full_rebuild() {
        let selected = snapshot(&[("nostr/v1/a.parquet", &"aa".repeat(32), 10, 2, 100, 200)]);
        let plan = plan_from_baseline(
            &selected,
            Some(Baseline {
                run_id: "run-1".to_owned(),
                snapshot_id: "snapshot-1".to_owned(),
                query_version: "slice-a-v1".to_owned(),
                objects: vec![applied(&selected.objects()[0])],
            }),
        )
        .expect("version upgrade plan");
        assert_eq!(plan.run_kind, PlannedRunKind::FullRebuild);
        assert_eq!(plan.added_objects, selected.objects());
        assert_eq!(plan.unchanged_objects, 0);
    }
}
