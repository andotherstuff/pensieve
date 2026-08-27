use std::fs::File;
use std::path::{Path, PathBuf};

use nostr::{Event, EventBuilder, Keys, Kind, Timestamp};
use pensieve_analytics::{
    AllBoundedProducts, AnalyticsBuild, BatchLimits, BuildConfig, CatalogDeltaPlan,
    EventFactsConfig, FixedActivityConfig, FlexibleDistinctConfig, FlexibleDistinctWindow,
    ObjectLocation, PlannedRunKind, PubkeyFirstSeenConfig, PublishOutcome,
    advance_bounded_fixed_activity, advance_bounded_pubkey_first_seen, apply_incremental,
    build_bounded_cohort_retention, build_bounded_event_facts, build_bounded_fixed_activity,
    build_bounded_flexible_distinct, build_bounded_pubkey_first_seen,
    estimate_flexible_distinct_window, estimate_flexible_distinct_windows,
    load_bounded_fixed_activity, load_bounded_flexible_distinct, load_bounded_pubkey_first_seen,
    plan_catalog_delta_for_query_version, plan_catalog_delta_from_run, publish,
    publish_with_all_bounded_products, publish_with_identity, publish_with_identity_and_activity,
    resolve_delta_locations, resolve_snapshot,
};
use pensieve_lake::{
    ActiveRawFragment, Inventory, ObjectKind, ObjectRecord, ObjectState, WorkState,
    WorkUnitRegistration, merge_active_raw_fragments, sha256_file, write_catalog_atomically,
};
use pensieve_parquet::write_events;

const AS_OF: u64 = 1_700_000_000;

struct Fixture {
    _directory: tempfile::TempDir,
    catalog_path: PathBuf,
    lake_root: PathBuf,
    build: AnalyticsBuild,
}

fn test_keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("valid test secret key")
}

fn event(created_at: u64, kind: u16, content: &str) -> Event {
    EventBuilder::new(Kind::from_u16(kind), content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&test_keys())
        .expect("test event should sign")
}

fn event_with_keys(keys: &Keys, created_at: u64, kind: u16, content: &str) -> Event {
    EventBuilder::new(Kind::from_u16(kind), content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("test event should sign")
}

fn publish_object(inventory: &mut Inventory, lake_root: &Path, work_id: &str, events: &[Event]) {
    let object_key = format!("nostr/v1/raw/{work_id}/part-00000.parquet");
    let object_path = lake_root.join(&object_key);
    std::fs::create_dir_all(object_path.parent().expect("object parent")).expect("create parent");
    let summary = write_events(
        File::create(&object_path).expect("create Parquet object"),
        events.iter(),
    )
    .expect("write Parquet object");
    let metadata = std::fs::metadata(&object_path).expect("object metadata");
    let source_sha256 = "11".repeat(32);
    inventory
        .ensure_work_unit(&WorkUnitRegistration {
            id: work_id,
            source_path: &PathBuf::from(format!("/source/{work_id}.notepack.gz")),
            source_bytes: 100,
            source_sha256: &source_sha256,
            target_uncompressed_bytes: 1_000,
            max_event_bytes: 2_000,
            object_prefix: "nostr/v1",
            writer_version: "test-writer",
        })
        .expect("register work");
    inventory
        .transition_work(work_id, WorkState::Writing, None)
        .expect("start work");
    inventory
        .record_validated_objects(
            work_id,
            events.len() as u64,
            summary.output_rows as u64,
            0,
            &[ObjectRecord {
                object_key: object_key.clone(),
                work_unit_id: work_id.to_owned(),
                part_number: 0,
                kind: ObjectKind::Parquet,
                state: ObjectState::Validated,
                local_path: object_path.clone(),
                byte_size: metadata.len(),
                sha256: sha256_file(&object_path).expect("object checksum"),
                writer_version: "test-writer".to_owned(),
                row_count: summary.output_rows as u64,
                min_created_at: events.iter().map(|event| event.created_at.as_secs()).min(),
                max_created_at: events.iter().map(|event| event.created_at.as_secs()).max(),
            }],
        )
        .expect("record validated object");
    inventory
        .transition_work(work_id, WorkState::Uploading, None)
        .expect("start upload");
    inventory
        .mark_object_uploaded(&object_key)
        .expect("mark uploaded");
    inventory
        .transition_work(work_id, WorkState::Uploaded, None)
        .expect("finish upload");
    inventory
        .activate_work_unit(work_id)
        .expect("activate object");
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("temporary directory");
    let lake_root = directory.path().join("lake");
    let duplicate = event(AS_OF - 10, 1, "duplicate");
    let lower_boundary = event(AS_OF - 7 * 24 * 60 * 60, 2, "lower");
    let recent = event(AS_OF - 100_000, 2, "recent");
    let pre_genesis = event_with_keys(&Keys::generate(), 100, 3, "old");
    let overflow = event(u64::from(u32::MAX) + 1, 4, "overflow");
    let future = event(AS_OF + 100, 5, "future");

    let mut inventory = Inventory::open_in_memory().expect("inventory");
    publish_object(
        &mut inventory,
        &lake_root,
        "first",
        &[duplicate.clone(), lower_boundary, pre_genesis, overflow],
    );
    publish_object(
        &mut inventory,
        &lake_root,
        "second",
        &[duplicate, recent, future],
    );
    let fragment = ActiveRawFragment::export(
        &mut inventory,
        "test",
        "s3+https://example.test/test-bucket",
    )
    .expect("export catalog");
    let snapshot = merge_active_raw_fragments([fragment]).expect("merge snapshot");
    let catalog_path = directory.path().join("snapshot.json");
    write_catalog_atomically(&catalog_path, &snapshot).expect("write snapshot");

    let resolved =
        resolve_snapshot(&catalog_path, Some(&lake_root)).expect("resolve local snapshot");
    let build = AnalyticsBuild::create(
        directory.path().join("analytics.duckdb"),
        resolved,
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "test".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "1GB".to_owned(),
            threads: 1,
        },
    )
    .expect("build analytics");
    Fixture {
        _directory: directory,
        catalog_path,
        lake_root,
        build,
    }
}

#[test]
fn slice_a_deduplicates_and_reconciles_exact_rollups() {
    let fixture = fixture();
    let build = fixture.build;

    assert_eq!(build.summary.physical_rows, 7);
    assert_eq!(build.summary.logical_events, 6);
    assert_eq!(build.summary.duplicate_rows, 1);
    assert_eq!(build.summary.api_representable_events, 5);
    assert_eq!(build.summary.kind_all_time_rows, 5);

    let overview = build.overview().expect("overview");
    assert_eq!(overview.total_events, 6);
    assert_eq!(overview.api_representable_events, 5);
    assert_eq!(
        overview.earliest_event,
        pensieve_core::NOSTR_GENESIS_TIMESTAMP
    );
    assert_eq!(overview.latest_event, (AS_OF - 10) as u32);
    assert_eq!(overview.events_7d, 3);
    assert_eq!(overview.events_per_hour_7d, 3.0 / 168.0);
    assert_eq!(overview.kinds_30d, 2);

    let mut daily_sum = 0;
    build
        .for_each_event_daily(|row| {
            daily_sum += row.event_count;
            Ok(())
        })
        .expect("daily rows");
    assert_eq!(daily_sum, 5);

    let mut daily_kind_sum = 0;
    build
        .for_each_event_daily_kind(|row| {
            daily_kind_sum += row.event_count;
            Ok(())
        })
        .expect("daily kind rows");
    assert_eq!(daily_kind_sum, 5);

    let mut kind_counts = Vec::new();
    build
        .for_each_kind_all_time(|row| {
            kind_counts.push((row.kind, row.event_count));
            Ok(())
        })
        .expect("kind rows");
    assert_eq!(kind_counts, vec![(1, 1), (2, 2), (3, 1), (4, 1), (5, 1)]);
}

#[test]
fn bounded_event_facts_are_byte_identical_to_slice_a_and_resume_exactly() {
    let fixture = fixture();
    let reference_bytes = fixture
        .build
        .canonical_metric_bytes()
        .expect("reference metric bytes");
    let resolved = resolve_snapshot(&fixture.catalog_path, Some(&fixture.lake_root))
        .expect("resolve bounded snapshot");
    let root = fixture._directory.path().join("bounded-event-facts");
    let database = fixture._directory.path().join("bounded.duckdb");
    let evidence = root.join("evidence.json");
    let config = BuildConfig {
        as_of_epoch: AS_OF,
        code_version: "bounded-test".to_owned(),
        s3_region: "test".to_owned(),
        s3_force_path_style: false,
        memory_limit: "256MB".to_owned(),
        threads: 1,
    };
    let facts_config = EventFactsConfig {
        work_root: root.clone(),
        batch_limits: BatchLimits {
            max_bytes: u64::MAX,
            max_rows: 4,
        },
        merge_fan_in: 2,
        disk_reserve_bytes: 0,
    };
    let bounded = build_bounded_event_facts(
        &database,
        &evidence,
        resolved,
        config.clone(),
        facts_config.clone(),
    )
    .expect("bounded build");
    assert_eq!(
        bounded
            .analytics
            .canonical_metric_bytes()
            .expect("bounded metric bytes"),
        reference_bytes
    );
    assert_eq!(bounded.evidence.status, "completed");
    assert_eq!(bounded.evidence.object_count, 2);
    assert_eq!(bounded.evidence.batch_count, 2);
    assert_eq!(bounded.evidence.merge_count, 1);
    assert_eq!(bounded.evidence.physical_rows, 7);
    assert_eq!(bounded.evidence.logical_events, 6);
    assert_eq!(bounded.evidence.duplicate_rows, 1);
    assert_eq!(bounded.evidence.batch_duplicate_rows, 0);
    assert_eq!(bounded.evidence.merge_duplicate_rows, 1);
    assert_eq!(bounded.evidence.final_artifact.byte_size, 6 * 42);
    assert_eq!(bounded.evidence.memory.max_merge_buffered_bytes, 3 * 42);
    let final_artifact_sha256 = bounded.evidence.final_artifact.sha256.clone();
    let first_evidence_sha256 = bounded.evidence_sha256.clone();
    drop(bounded);

    let single_batch_root = fixture._directory.path().join("bounded-single-batch");
    let resolved = resolve_snapshot(&fixture.catalog_path, Some(&fixture.lake_root))
        .expect("resolve single batch snapshot");
    let single_batch = build_bounded_event_facts(
        fixture._directory.path().join("bounded-single.duckdb"),
        single_batch_root.join("evidence.json"),
        resolved,
        config.clone(),
        EventFactsConfig {
            work_root: single_batch_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: 7,
            },
            merge_fan_in: 4,
            disk_reserve_bytes: 0,
        },
    )
    .expect("single batch bounded build");
    assert_eq!(single_batch.evidence.batch_count, 1);
    assert_eq!(single_batch.evidence.merge_count, 0);
    assert_eq!(single_batch.evidence.batch_duplicate_rows, 1);
    assert_eq!(single_batch.evidence.merge_duplicate_rows, 0);
    assert_eq!(
        single_batch.evidence.final_artifact.sha256,
        final_artifact_sha256
    );
    assert_eq!(
        single_batch
            .analytics
            .canonical_metric_bytes()
            .expect("single batch metric bytes"),
        reference_bytes
    );
    drop(single_batch);

    let resolved = resolve_snapshot(&fixture.catalog_path, Some(&fixture.lake_root))
        .expect("resolve retry snapshot");
    let resumed = build_bounded_event_facts(&database, &evidence, resolved, config, facts_config)
        .expect("resume bounded build");
    assert_eq!(resumed.evidence_sha256, first_evidence_sha256);
    assert_eq!(
        resumed
            .analytics
            .canonical_metric_bytes()
            .expect("resumed metric bytes"),
        reference_bytes
    );
}

#[test]
fn bounded_first_seen_is_exact_eligible_and_resumable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let lake_root = directory.path().join("lake");
    let first = test_keys();
    let second = Keys::generate();
    let pre_genesis = Keys::generate();
    let future = Keys::generate();
    let day_one = AS_OF - 86_400;
    let day_two = AS_OF - 10;
    let mut inventory = Inventory::open_in_memory().expect("inventory");
    publish_object(
        &mut inventory,
        &lake_root,
        "first-seen-a",
        &[
            event_with_keys(&first, day_one, 1, "first"),
            event_with_keys(&second, day_one - 100, 445, "excluded"),
            event_with_keys(&pre_genesis, 100, 1, "too old"),
        ],
    );
    publish_object(
        &mut inventory,
        &lake_root,
        "first-seen-b",
        &[
            event_with_keys(&first, day_two, 1, "later"),
            event_with_keys(&second, day_two, 1, "eligible"),
            event_with_keys(&pre_genesis, day_two, 1, "later but still ineligible"),
            event_with_keys(&future, AS_OF + 10, 1, "future"),
        ],
    );
    let fragment = ActiveRawFragment::export(
        &mut inventory,
        "first-seen-test",
        "s3+https://example.test/test-bucket",
    )
    .expect("export catalog");
    let snapshot = merge_active_raw_fragments([fragment]).expect("merge snapshot");
    let catalog = directory.path().join("snapshot.json");
    write_catalog_atomically(&catalog, &snapshot).expect("write snapshot");
    let resolved = resolve_snapshot(&catalog, Some(&lake_root)).expect("resolve snapshot");
    let root = directory.path().join("first-seen");
    let evidence = root.join("evidence.json");
    let build = BuildConfig {
        as_of_epoch: AS_OF,
        code_version: "first-seen-test".to_owned(),
        s3_region: "test".to_owned(),
        s3_force_path_style: false,
        memory_limit: "256MB".to_owned(),
        threads: 1,
    };
    let config = PubkeyFirstSeenConfig {
        work_root: root.clone(),
        batch_limits: BatchLimits {
            max_bytes: u64::MAX,
            max_rows: 4,
        },
        merge_fan_in: 2,
        disk_reserve_bytes: 0,
    };
    let completed =
        build_bounded_pubkey_first_seen(&evidence, resolved, build.clone(), config.clone())
            .expect("bounded first seen");
    assert_eq!(completed.evidence.batch_count, 2);
    assert_eq!(completed.evidence.merge_count, 1);
    assert_eq!(completed.evidence.first_seen_records, 4);
    assert_eq!(completed.evidence.eligible_pubkeys, 2);
    assert_eq!(completed.evidence.new_users_daily.len(), 2);
    assert_eq!(
        completed
            .evidence
            .new_users_daily
            .iter()
            .map(|row| row.new_pubkeys)
            .sum::<u64>(),
        2
    );
    assert_eq!(completed.evidence.final_artifact.byte_size, 4 * 40);
    let evidence_sha = completed.evidence_sha256.clone();
    let artifact_sha = completed.evidence.final_artifact.sha256.clone();
    drop(completed);
    let resolved = resolve_snapshot(&catalog, Some(&lake_root)).expect("resolve retry");
    let retried = build_bounded_pubkey_first_seen(&evidence, resolved, build, config)
        .expect("resume first seen");
    assert_eq!(retried.evidence_sha256, evidence_sha);
    assert_eq!(retried.evidence.final_artifact.sha256, artifact_sha);
}

#[test]
fn bounded_fixed_activity_is_exact_across_grains_flags_and_exclusions() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let lake_root = directory.path().join("lake");
    let profile = test_keys();
    let follows = Keys::generate();
    let both = Keys::generate();
    let excluded = Keys::generate();
    let trailing = Keys::generate();
    let day_a = 1_699_833_600_u64; // 2023-11-13 UTC
    let day_b = day_a + 86_400;
    let duplicate = event_with_keys(&profile, day_a + 2, 1, "note one");
    let mut inventory = Inventory::open_in_memory().expect("inventory");
    publish_object(
        &mut inventory,
        &lake_root,
        "activity-a",
        &[
            event_with_keys(&profile, day_a + 1, 0, "profile"),
            duplicate.clone(),
            event_with_keys(&follows, day_a + 3, 3, "follows"),
            event_with_keys(&both, day_a + 4, 0, "profile"),
            event_with_keys(&excluded, day_a + 5, 445, "excluded active kind"),
        ],
    );
    publish_object(
        &mut inventory,
        &lake_root,
        "activity-b",
        &[
            duplicate,
            event_with_keys(&profile, day_b + 1, 1, "note two"),
            event_with_keys(&profile, day_b + 2, 1, "note three"),
            event_with_keys(&both, day_b + 3, 3, "follows"),
            event_with_keys(&both, day_b + 4, 1, "note"),
            event_with_keys(&follows, day_b + 5, 1, "note"),
            event_with_keys(&trailing, AS_OF - 1, 1, "incomplete trailing hour"),
        ],
    );
    let snapshot = merge_active_raw_fragments([ActiveRawFragment::export(
        &mut inventory,
        "fixed-activity-test",
        "s3+https://example.test/test-bucket",
    )
    .expect("export fragment")])
    .expect("snapshot");
    let catalog = directory.path().join("snapshot.json");
    write_catalog_atomically(&catalog, &snapshot).expect("write snapshot");
    let root = directory.path().join("fixed-activity");
    let evidence = root.join("evidence.json");
    let completed = build_bounded_fixed_activity(
        &evidence,
        resolve_snapshot(&catalog, Some(&lake_root)).expect("resolve snapshot"),
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "fixed-activity-test".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "256MB".to_owned(),
            threads: 1,
        },
        FixedActivityConfig {
            work_root: root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: 5,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("build fixed activity");
    assert_eq!(completed.evidence.batch_count, 2);
    assert_eq!(completed.evidence.merge_count, 1);
    assert_eq!(completed.evidence.flags_artifact.row_count, 5);
    assert_eq!(completed.evidence.max_merge_buffered_bytes, 3 * 70);

    let daily = completed
        .evidence
        .active_users
        .iter()
        .find(|row| row.grain == "day" && row.period_start == "2023-11-14")
        .expect("second daily row");
    assert_eq!(daily.active_users, 4);
    assert_eq!(daily.has_profile, 2);
    assert_eq!(daily.has_follows_list, 2);
    assert_eq!(daily.has_profile_and_follows_list, 1);
    assert_eq!(daily.total_events, 6);

    let daily_all: Vec<_> = completed
        .evidence
        .distinct_pubkeys
        .iter()
        .filter(|row| row.grain == "day" && row.kind.is_none())
        .collect();
    assert_eq!(daily_all.len(), 2);
    assert_eq!(
        daily_all.iter().map(|row| row.unique_pubkeys).sum::<u64>(),
        8
    );
    let weekly = completed
        .evidence
        .distinct_pubkeys
        .iter()
        .find(|row| row.grain == "week" && row.kind.is_none())
        .expect("weekly all-kind row");
    assert_eq!(weekly.unique_pubkeys, 5);
    assert!(daily_all.iter().map(|row| row.unique_pubkeys).sum::<u64>() > weekly.unique_pubkeys);
    let kind_one_weekly = completed
        .evidence
        .distinct_pubkeys
        .iter()
        .find(|row| row.grain == "week" && row.kind == Some(1))
        .expect("weekly kind-one row");
    assert_eq!(kind_one_weekly.unique_pubkeys, 4);

    let evidence_sha = completed.evidence_sha256.clone();
    let flexible_root = directory.path().join("flexible-distinct");
    let flexible_evidence = flexible_root.join("evidence.json");
    let flexible_config = FlexibleDistinctConfig {
        work_root: flexible_root,
        source_records_per_batch: 3,
        merge_fan_in: 2,
        disk_reserve_bytes: 0,
    };
    let flexible =
        build_bounded_flexible_distinct(&flexible_evidence, &completed, flexible_config.clone())
            .expect("build flexible distinct");
    assert_eq!(flexible.evidence.source_activity_rows, 11);
    assert_eq!(flexible.evidence.batch_count, 4);
    assert!(flexible.evidence.merge_count >= 2);
    assert_eq!(flexible.evidence.max_batch_buffered_bytes, 3 * 38);
    let complete_through = AS_OF - (AS_OF % 3_600);
    assert_eq!(
        estimate_flexible_distinct_window(&flexible, day_a, complete_through, None)
            .expect("estimate all authors"),
        4
    );
    assert_eq!(
        estimate_flexible_distinct_window(&flexible, day_a, complete_through, Some(0))
            .expect("estimate profile authors"),
        2
    );
    assert_eq!(
        estimate_flexible_distinct_windows(
            &flexible,
            &[
                FlexibleDistinctWindow {
                    since_epoch: day_a,
                    until_epoch: complete_through,
                    kind: None,
                },
                FlexibleDistinctWindow {
                    since_epoch: day_a,
                    until_epoch: complete_through,
                    kind: Some(0),
                },
            ],
        )
        .expect("estimate several windows"),
        vec![4, 2]
    );
    assert!(
        estimate_flexible_distinct_window(&flexible, day_a + 1, complete_through, None).is_err()
    );
    let flexible_sha = flexible.evidence_sha256.clone();
    let flexible_leaf_sha = flexible.evidence.leaf_artifact.sha256.clone();
    drop(flexible);
    let retried = build_bounded_flexible_distinct(&flexible_evidence, &completed, flexible_config)
        .expect("resume flexible distinct");
    assert_eq!(retried.evidence_sha256, flexible_sha);
    assert_eq!(retried.evidence.leaf_artifact.sha256, flexible_leaf_sha);
    drop(retried);
    let loaded_flexible =
        load_bounded_flexible_distinct(&flexible_evidence).expect("reload flexible evidence");
    assert_eq!(loaded_flexible.evidence_sha256, flexible_sha);

    drop(completed);
    let loaded = load_bounded_fixed_activity(&evidence).expect("reload evidence");
    assert_eq!(loaded.evidence_sha256, evidence_sha);
}

#[test]
fn bounded_fixed_activity_incrementally_unions_identities_and_counts_events() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let lake_root = directory.path().join("lake");
    let existing = test_keys();
    let added = Keys::generate();
    let maturing = Keys::generate();
    let day_a = 1_699_833_600_u64;
    let day_b = day_a + 86_400;
    let mut inventory = Inventory::open_in_memory().expect("inventory");
    publish_object(
        &mut inventory,
        &lake_root,
        "activity-baseline",
        &[
            event_with_keys(&existing, day_b + 1, 1, "baseline"),
            event_with_keys(&maturing, AS_OF + 50, 1, "future at baseline"),
        ],
    );
    let baseline_snapshot = merge_active_raw_fragments([ActiveRawFragment::export(
        &mut inventory,
        "activity-incremental",
        "s3+https://example.test/test-bucket",
    )
    .expect("baseline fragment")])
    .expect("baseline snapshot");
    let baseline_catalog = directory.path().join("activity-baseline.json");
    write_catalog_atomically(&baseline_catalog, &baseline_snapshot).expect("baseline catalog");
    let build = BuildConfig {
        as_of_epoch: AS_OF,
        code_version: "activity-incremental-test".to_owned(),
        s3_region: "test".to_owned(),
        s3_force_path_style: false,
        memory_limit: "256MB".to_owned(),
        threads: 1,
    };
    let baseline_root = directory.path().join("activity-baseline-work");
    let baseline = build_bounded_fixed_activity(
        baseline_root.join("evidence.json"),
        resolve_snapshot(&baseline_catalog, Some(&lake_root)).expect("resolve baseline"),
        build.clone(),
        FixedActivityConfig {
            work_root: baseline_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: u64::MAX,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("build baseline activity");
    assert_eq!(baseline.evidence.activity_artifact.row_count, 2);
    assert_eq!(
        baseline
            .evidence
            .active_users
            .iter()
            .map(|row| row.total_events)
            .sum::<u64>(),
        3
    );

    publish_object(
        &mut inventory,
        &lake_root,
        "activity-delta",
        &[
            event_with_keys(&existing, day_b + 2, 1, "same key increments count"),
            event_with_keys(&existing, day_a + 1, 1, "late earlier day"),
            event_with_keys(&added, day_b + 3, 0, "new profile"),
        ],
    );
    let target_snapshot = merge_active_raw_fragments([ActiveRawFragment::export(
        &mut inventory,
        "activity-incremental",
        "s3+https://example.test/test-bucket",
    )
    .expect("target fragment")])
    .expect("target snapshot");
    let target_catalog = directory.path().join("activity-target.json");
    write_catalog_atomically(&target_catalog, &target_snapshot).expect("target catalog");
    let added_object = target_snapshot
        .objects()
        .iter()
        .find(|object| object.work_unit_id == "activity-delta")
        .expect("delta object")
        .clone();
    let plan = CatalogDeltaPlan {
        snapshot_id: target_snapshot.snapshot_id.clone(),
        previous_run_id: Some("baseline-run".to_owned()),
        previous_snapshot_id: Some(baseline_snapshot.snapshot_id.clone()),
        run_kind: PlannedRunKind::Incremental,
        added_objects: vec![added_object.clone()],
        removed_objects: Vec::new(),
        unchanged_objects: 1,
        added_bytes: added_object.byte_size,
        added_physical_rows: added_object.row_count,
        affected_min_created_at: added_object.min_created_at.clone(),
        affected_max_created_at: added_object.max_created_at.clone(),
        affected_range_complete: true,
    };
    let successor_root = directory.path().join("activity-successor-work");
    let successor = advance_bounded_fixed_activity(
        successor_root.join("evidence.json"),
        &baseline,
        resolve_snapshot(&target_catalog, Some(&lake_root)).expect("resolve target"),
        &plan,
        &[ObjectLocation::Local(
            lake_root.join(&added_object.object_key),
        )],
        BuildConfig {
            as_of_epoch: AS_OF + 100,
            ..build
        },
        FixedActivityConfig {
            work_root: successor_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: u64::MAX,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("advance activity");
    assert_eq!(successor.evidence.delta_object_count, 1);
    assert_eq!(
        successor.evidence.baseline_evidence_sha256.as_deref(),
        Some(baseline.evidence_sha256.as_str())
    );
    let day_b_row = successor
        .evidence
        .active_users
        .iter()
        .find(|row| row.grain == "day" && row.period_start == "2023-11-14")
        .expect("target daily row");
    assert_eq!(day_b_row.active_users, 3);
    assert_eq!(day_b_row.has_profile, 1);
    assert_eq!(day_b_row.total_events, 4);
    let week_kind = successor
        .evidence
        .distinct_pubkeys
        .iter()
        .find(|row| row.grain == "week" && row.kind == Some(1))
        .expect("weekly kind row");
    assert_eq!(week_kind.unique_pubkeys, 2);
}

#[test]
fn bounded_first_seen_incremental_moves_late_pubkey_to_older_day() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let lake_root = directory.path().join("lake");
    let existing = test_keys();
    let added = Keys::generate();
    let mut inventory = Inventory::open_in_memory().expect("inventory");
    publish_object(
        &mut inventory,
        &lake_root,
        "identity-baseline",
        &[event_with_keys(&existing, AS_OF - 10, 1, "baseline")],
    );
    let baseline_snapshot = merge_active_raw_fragments([ActiveRawFragment::export(
        &mut inventory,
        "identity-incremental",
        "s3+https://example.test/test-bucket",
    )
    .expect("baseline fragment")])
    .expect("baseline snapshot");
    let baseline_catalog = directory.path().join("baseline.json");
    write_catalog_atomically(&baseline_catalog, &baseline_snapshot).expect("baseline catalog");
    let baseline_config = BuildConfig {
        as_of_epoch: AS_OF,
        code_version: "identity-incremental-test".to_owned(),
        s3_region: "test".to_owned(),
        s3_force_path_style: false,
        memory_limit: "256MB".to_owned(),
        threads: 1,
    };
    let baseline_root = directory.path().join("identity-baseline-work");
    let baseline_evidence = baseline_root.join("evidence.json");
    let baseline = build_bounded_pubkey_first_seen(
        &baseline_evidence,
        resolve_snapshot(&baseline_catalog, Some(&lake_root)).expect("resolve baseline"),
        baseline_config.clone(),
        PubkeyFirstSeenConfig {
            work_root: baseline_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: u64::MAX,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("build baseline identity");
    assert_eq!(baseline.evidence.eligible_pubkeys, 1);

    publish_object(
        &mut inventory,
        &lake_root,
        "identity-delta",
        &[
            event_with_keys(&existing, AS_OF - 2 * 86_400, 1, "late history"),
            event_with_keys(&added, AS_OF - 20, 1, "new pubkey"),
        ],
    );
    let target_snapshot = merge_active_raw_fragments([ActiveRawFragment::export(
        &mut inventory,
        "identity-incremental",
        "s3+https://example.test/test-bucket",
    )
    .expect("target fragment")])
    .expect("target snapshot");
    let target_catalog = directory.path().join("target.json");
    write_catalog_atomically(&target_catalog, &target_snapshot).expect("target catalog");
    let added_object = target_snapshot
        .objects()
        .iter()
        .find(|object| object.work_unit_id == "identity-delta")
        .expect("delta object")
        .clone();
    let plan = CatalogDeltaPlan {
        snapshot_id: target_snapshot.snapshot_id.clone(),
        previous_run_id: Some("baseline-run".to_owned()),
        previous_snapshot_id: Some(baseline_snapshot.snapshot_id.clone()),
        run_kind: PlannedRunKind::Incremental,
        added_objects: vec![added_object.clone()],
        removed_objects: Vec::new(),
        unchanged_objects: 1,
        added_bytes: added_object.byte_size,
        added_physical_rows: added_object.row_count,
        affected_min_created_at: added_object.min_created_at.clone(),
        affected_max_created_at: added_object.max_created_at.clone(),
        affected_range_complete: true,
    };
    let successor_root = directory.path().join("identity-successor-work");
    let successor_evidence = successor_root.join("evidence.json");
    let successor = advance_bounded_pubkey_first_seen(
        &successor_evidence,
        &baseline,
        resolve_snapshot(&target_catalog, Some(&lake_root)).expect("resolve target"),
        &plan,
        &[ObjectLocation::Local(
            lake_root.join(&added_object.object_key),
        )],
        baseline_config,
        PubkeyFirstSeenConfig {
            work_root: successor_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: u64::MAX,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("advance identity");
    assert_eq!(successor.evidence.eligible_pubkeys, 2);
    assert_eq!(successor.evidence.delta_object_count, 1);
    assert_eq!(
        successor.evidence.baseline_evidence_sha256.as_deref(),
        Some(baseline.evidence_sha256.as_str())
    );
    assert_eq!(successor.evidence.new_users_daily.len(), 2);
    assert_eq!(successor.evidence.new_users_daily[0].new_pubkeys, 1);
    assert_eq!(successor.evidence.new_users_daily[1].new_pubkeys, 1);
    let reloaded = load_bounded_pubkey_first_seen(&successor_evidence)
        .expect("reload immutable successor evidence");
    assert_eq!(reloaded.evidence_sha256, successor.evidence_sha256);
}

#[test]
fn slice_a_handles_a_snapshot_with_no_parquet_objects() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut inventory = Inventory::open_in_memory().expect("inventory");
    let source_sha256 = "22".repeat(32);
    inventory
        .ensure_work_unit(&WorkUnitRegistration {
            id: "empty",
            source_path: Path::new("/source/empty.notepack.gz"),
            source_bytes: 0,
            source_sha256: &source_sha256,
            target_uncompressed_bytes: 1_000,
            max_event_bytes: 2_000,
            object_prefix: "nostr/v1",
            writer_version: "test-writer",
        })
        .expect("register empty work");
    inventory
        .transition_work("empty", WorkState::Writing, None)
        .expect("start empty work");
    inventory
        .record_validated_objects("empty", 0, 0, 0, &[])
        .expect("validate empty work");
    inventory
        .transition_work("empty", WorkState::Uploading, None)
        .expect("start empty upload");
    inventory
        .transition_work("empty", WorkState::Uploaded, None)
        .expect("finish empty upload");
    inventory
        .activate_work_unit("empty")
        .expect("activate empty work");

    let fragment = ActiveRawFragment::export(
        &mut inventory,
        "empty-test",
        "s3+https://example.test/test-bucket",
    )
    .expect("export empty catalog");
    let snapshot = merge_active_raw_fragments([fragment]).expect("merge empty snapshot");
    let catalog_path = directory.path().join("empty-snapshot.json");
    write_catalog_atomically(&catalog_path, &snapshot).expect("write empty snapshot");
    let resolved = resolve_snapshot(&catalog_path, Some(directory.path()))
        .expect("resolve empty local snapshot");
    let database_path = directory.path().join("empty.duckdb");
    let build = AnalyticsBuild::create(
        &database_path,
        resolved,
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "test".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "1GB".to_owned(),
            threads: 1,
        },
    )
    .expect("build empty analytics");

    assert_eq!(build.summary.logical_events, 0);
    assert_eq!(build.summary.event_daily_rows, 0);
    let overview = build.overview().expect("empty overview");
    assert_eq!(overview.total_events, 0);
    assert_eq!(
        overview.earliest_event,
        pensieve_core::NOSTR_GENESIS_TIMESTAMP
    );
    assert_eq!(overview.latest_event, 0);
    assert_eq!(overview.events_7d, 0);
    assert_eq!(overview.kinds_30d, 0);

    let expected_summary = build.summary.clone();
    drop(build);
    let resolved = resolve_snapshot(&catalog_path, Some(directory.path()))
        .expect("resolve empty local snapshot for reopen");
    let reopened = AnalyticsBuild::open_completed(
        &database_path,
        resolved,
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "test".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "1GB".to_owned(),
            threads: 1,
        },
    )
    .expect("reopen completed analytics");
    assert_eq!(reopened.summary, expected_summary);
    let reference_bytes = reopened
        .canonical_metric_bytes()
        .expect("empty reference metrics");
    drop(reopened);
    let resolved = resolve_snapshot(&catalog_path, Some(directory.path()))
        .expect("resolve empty bounded snapshot");
    let bounded_root = directory.path().join("empty-bounded");
    let bounded = build_bounded_event_facts(
        directory.path().join("empty-bounded.duckdb"),
        bounded_root.join("evidence.json"),
        resolved,
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "empty-bounded-test".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "256MB".to_owned(),
            threads: 1,
        },
        EventFactsConfig {
            work_root: bounded_root,
            batch_limits: BatchLimits {
                max_bytes: 1,
                max_rows: 1,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("build empty bounded analytics");
    assert_eq!(bounded.evidence.object_count, 0);
    assert_eq!(bounded.evidence.batch_count, 0);
    assert_eq!(bounded.evidence.merge_count, 0);
    assert_eq!(bounded.evidence.physical_rows, 0);
    assert_eq!(bounded.evidence.logical_events, 0);
    assert_eq!(bounded.evidence.final_artifact.byte_size, 0);
    assert_eq!(
        bounded
            .analytics
            .canonical_metric_bytes()
            .expect("empty bounded metrics"),
        reference_bytes
    );
}

#[test]
fn incremental_build_inserts_only_new_ids_and_is_idempotent() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let lake_root = directory.path().join("lake");
    let duplicate = event(AS_OF - 10, 1, "duplicate");
    let added = event(AS_OF - 20, 2, "added");
    let mut inventory = Inventory::open_in_memory().expect("inventory");
    publish_object(
        &mut inventory,
        &lake_root,
        "first",
        std::slice::from_ref(&duplicate),
    );
    let baseline_fragment = ActiveRawFragment::export(
        &mut inventory,
        "test",
        "s3+https://example.test/test-bucket",
    )
    .expect("baseline fragment");
    let baseline_snapshot =
        merge_active_raw_fragments([baseline_fragment]).expect("baseline snapshot");
    let baseline_catalog = directory.path().join("baseline.json");
    write_catalog_atomically(&baseline_catalog, &baseline_snapshot).expect("baseline catalog");
    let database = directory.path().join("incremental.duckdb");
    let baseline = resolve_snapshot(&baseline_catalog, Some(&lake_root)).expect("baseline resolve");
    AnalyticsBuild::create(
        &database,
        baseline,
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "baseline".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "1GB".to_owned(),
            threads: 1,
        },
    )
    .expect("baseline build");

    publish_object(&mut inventory, &lake_root, "second", &[duplicate, added]);
    let target_fragment = ActiveRawFragment::export(
        &mut inventory,
        "test",
        "s3+https://example.test/test-bucket",
    )
    .expect("target fragment");
    let target_snapshot = merge_active_raw_fragments([target_fragment]).expect("target snapshot");
    let target_catalog = directory.path().join("target.json");
    write_catalog_atomically(&target_catalog, &target_snapshot).expect("target catalog");
    let added_object = target_snapshot
        .objects()
        .iter()
        .find(|object| object.work_unit_id == "second")
        .expect("second object")
        .clone();
    let plan = CatalogDeltaPlan {
        snapshot_id: target_snapshot.snapshot_id.clone(),
        previous_run_id: Some("baseline-run".to_owned()),
        previous_snapshot_id: Some(baseline_snapshot.snapshot_id),
        run_kind: PlannedRunKind::Incremental,
        added_objects: vec![added_object.clone()],
        removed_objects: Vec::new(),
        unchanged_objects: 1,
        added_bytes: added_object.byte_size,
        added_physical_rows: added_object.row_count,
        affected_min_created_at: added_object.min_created_at.clone(),
        affected_max_created_at: added_object.max_created_at.clone(),
        affected_range_complete: true,
    };
    let locations = resolve_delta_locations(&plan, &lake_root).expect("delta locations");
    let config = BuildConfig {
        as_of_epoch: AS_OF + 100,
        code_version: "incremental".to_owned(),
        s3_region: "test".to_owned(),
        s3_force_path_style: false,
        memory_limit: "1GB".to_owned(),
        threads: 1,
    };
    let target = resolve_snapshot(&target_catalog, Some(&lake_root)).expect("dry target resolve");
    let (dry_build, dry_run) = apply_incremental(
        &database,
        target,
        &plan,
        &locations,
        config.clone(),
        AS_OF,
        true,
    )
    .expect("incremental dry run");
    assert_eq!(dry_run.existing_events, 1);
    assert_eq!(dry_run.inserted_events, 1);
    assert_eq!(dry_build.summary.logical_events, 1);
    drop(dry_build);

    let target = resolve_snapshot(&target_catalog, Some(&lake_root)).expect("target resolve");
    let (build, incremental) = apply_incremental(
        &database,
        target,
        &plan,
        &locations,
        config.clone(),
        AS_OF,
        false,
    )
    .expect("incremental build");
    assert_eq!(incremental.delta_physical_rows, 2);
    assert_eq!(incremental.delta_logical_events, 2);
    assert_eq!(incremental.existing_events, 1);
    assert_eq!(incremental.inserted_events, 1);
    assert!(!incremental.already_applied);
    assert_eq!(build.summary.physical_rows, 3);
    assert_eq!(build.summary.logical_events, 2);
    assert_eq!(build.summary.duplicate_rows, 1);
    drop(build);

    let target = resolve_snapshot(&target_catalog, Some(&lake_root)).expect("target re-resolve");
    let (build, retry) =
        apply_incremental(&database, target, &plan, &locations, config, AS_OF, false)
            .expect("idempotent retry");
    assert!(retry.already_applied);
    assert_eq!(build.summary.logical_events, 2);
}

#[test]
#[ignore = "requires a disposable Postgres database in PENSIEVE_TEST_POSTGRES_URL"]
fn slice_a_publication_is_atomic_and_idempotent() {
    let fixture = fixture();
    let mut client = postgres::Client::connect(
        &std::env::var("PENSIEVE_TEST_POSTGRES_URL")
            .expect("PENSIEVE_TEST_POSTGRES_URL must be set"),
        postgres::NoTls,
    )
    .expect("connect to disposable Postgres");
    let started_at = chrono::Utc::now();
    let completed_at = chrono::Utc::now();
    let first = publish(&mut client, &fixture.build, started_at, completed_at)
        .expect("publish Slice A baseline");
    let baseline_run_id = match first {
        PublishOutcome::Published { run_id, .. } => run_id,
        PublishOutcome::AlreadyCurrent { .. } => panic!("database must begin empty"),
    };
    let identity_root = fixture._directory.path().join("publication-identity");
    let identity = build_bounded_pubkey_first_seen(
        identity_root.join("evidence.json"),
        fixture.build.snapshot.clone(),
        fixture.build.config.clone(),
        PubkeyFirstSeenConfig {
            work_root: identity_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: 4,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("build identity publication");
    client
        .batch_execute(
            "
            CREATE OR REPLACE FUNCTION pensieve_analytics.reject_identity_test()
            RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                RAISE EXCEPTION 'injected identity publication failure';
            END;
            $$;
            CREATE TRIGGER reject_identity_test
            BEFORE INSERT ON pensieve_analytics.new_users_daily
            FOR EACH ROW EXECUTE FUNCTION pensieve_analytics.reject_identity_test();
            ",
        )
        .expect("install failure injection");
    publish_with_identity(
        &mut client,
        &fixture.build,
        &identity,
        started_at,
        completed_at,
    )
    .expect_err("injected COPY failure must abort identity publication");
    assert_eq!(
        client
            .query_one(
                "SELECT run_id FROM pensieve_analytics.current_run_metadata",
                &[],
            )
            .expect("read baseline after failed identity publication")
            .get::<_, String>(0),
        baseline_run_id
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pensieve_analytics.runs WHERE query_version = 'slice-b1-v1'",
                &[],
            )
            .expect("count rolled-back identity runs")
            .get::<_, i64>(0),
        0
    );
    client
        .batch_execute(
            "
            DROP TRIGGER reject_identity_test ON pensieve_analytics.new_users_daily;
            DROP FUNCTION pensieve_analytics.reject_identity_test();
            ",
        )
        .expect("remove failure injection");
    let identity_publication = publish_with_identity(
        &mut client,
        &fixture.build,
        &identity,
        started_at,
        completed_at,
    )
    .expect("publish identity run");
    let run_id = match identity_publication {
        PublishOutcome::Published {
            run_id,
            previous_run_id,
        } => {
            assert_eq!(previous_run_id.as_deref(), Some(baseline_run_id.as_str()));
            run_id
        }
        PublishOutcome::AlreadyCurrent { .. } => panic!("identity run must be new"),
    };
    assert_eq!(
        client
            .query_one(
                "SELECT run_id FROM pensieve_analytics.current_run_metadata",
                &[],
            )
            .expect("read current run")
            .get::<_, String>(0),
        run_id
    );
    assert_eq!(
        client
            .query_one(
                "SELECT total_events FROM pensieve_analytics.current_overview",
                &[],
            )
            .expect("read current overview")
            .get::<_, i64>(0),
        6
    );
    let metadata = client
        .query_one(
            "SELECT query_version, eligible_pubkeys, new_users_daily_rows FROM pensieve_analytics.current_run_metadata",
            &[],
        )
        .expect("read current identity metadata");
    assert_eq!(metadata.get::<_, String>(0), "slice-b1-v1");
    assert_eq!(
        metadata.get::<_, i64>(1),
        identity.evidence.eligible_pubkeys as i64
    );
    assert_eq!(
        metadata.get::<_, i64>(2),
        identity.evidence.new_users_daily.len() as i64
    );
    assert_eq!(
        client
            .query_one(
                "SELECT total_pubkeys FROM pensieve_analytics.current_overview",
                &[],
            )
            .expect("read current pubkeys")
            .get::<_, i64>(0),
        identity.evidence.eligible_pubkeys as i64
    );
    assert_eq!(
        client
            .query_one(
                "SELECT coalesce(sum(new_pubkeys), 0)::BIGINT FROM pensieve_analytics.current_new_users_daily",
                &[],
            )
            .expect("read daily new users")
            .get::<_, i64>(0),
        identity.evidence.eligible_pubkeys as i64
    );
    assert_eq!(
        publish_with_identity(
            &mut client,
            &fixture.build,
            &identity,
            started_at,
            completed_at,
        )
        .expect("retry current identity run"),
        PublishOutcome::AlreadyCurrent {
            run_id: run_id.clone()
        }
    );
    let activity_root = fixture._directory.path().join("publication-activity");
    let activity = build_bounded_fixed_activity(
        activity_root.join("evidence.json"),
        fixture.build.snapshot.clone(),
        fixture.build.config.clone(),
        FixedActivityConfig {
            work_root: activity_root,
            batch_limits: BatchLimits {
                max_bytes: u64::MAX,
                max_rows: 4,
            },
            merge_fan_in: 2,
            disk_reserve_bytes: 0,
        },
    )
    .expect("build activity publication");
    client
        .batch_execute(
            "
            CREATE OR REPLACE FUNCTION pensieve_analytics.reject_activity_test()
            RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                RAISE EXCEPTION 'injected activity publication failure';
            END;
            $$;
            CREATE TRIGGER reject_activity_test
            BEFORE INSERT ON pensieve_analytics.active_users_period
            FOR EACH ROW EXECUTE FUNCTION pensieve_analytics.reject_activity_test();
            ",
        )
        .expect("install activity failure injection");
    publish_with_identity_and_activity(
        &mut client,
        &fixture.build,
        &identity,
        &activity,
        started_at,
        completed_at,
    )
    .expect_err("injected activity COPY failure must abort publication");
    assert_eq!(
        client
            .query_one(
                "SELECT run_id FROM pensieve_analytics.current_run_metadata",
                &[],
            )
            .expect("read identity run after failed activity publication")
            .get::<_, String>(0),
        run_id
    );
    client
        .batch_execute(
            "
            DROP TRIGGER reject_activity_test ON pensieve_analytics.active_users_period;
            DROP FUNCTION pensieve_analytics.reject_activity_test();
            ",
        )
        .expect("remove activity failure injection");
    let activity_run_id = match publish_with_identity_and_activity(
        &mut client,
        &fixture.build,
        &identity,
        &activity,
        started_at,
        completed_at,
    )
    .expect("publish activity run")
    {
        PublishOutcome::Published {
            run_id: activity_run_id,
            previous_run_id,
        } => {
            assert_eq!(previous_run_id.as_deref(), Some(run_id.as_str()));
            activity_run_id
        }
        PublishOutcome::AlreadyCurrent { .. } => panic!("activity run must be new"),
    };
    let activity_metadata = client
        .query_one(
            "SELECT query_version, distinct_pubkeys_period_rows, active_users_period_rows FROM pensieve_analytics.current_run_metadata",
            &[],
        )
        .expect("read current activity metadata");
    assert_eq!(activity_metadata.get::<_, String>(0), "slice-b2-v2");
    assert_eq!(
        activity_metadata.get::<_, i64>(1),
        activity.evidence.distinct_period_rows as i64
    );
    assert_eq!(
        activity_metadata.get::<_, i64>(2),
        activity.evidence.active_period_rows as i64
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pensieve_analytics.current_distinct_pubkeys_period",
                &[],
            )
            .expect("count current distinct rows")
            .get::<_, i64>(0),
        activity.evidence.distinct_period_rows as i64
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pensieve_analytics.current_active_users_period",
                &[],
            )
            .expect("count current active rows")
            .get::<_, i64>(0),
        activity.evidence.active_period_rows as i64
    );
    assert_eq!(
        publish_with_identity_and_activity(
            &mut client,
            &fixture.build,
            &identity,
            &activity,
            started_at,
            completed_at,
        )
        .expect("retry current activity run"),
        PublishOutcome::AlreadyCurrent {
            run_id: activity_run_id.clone()
        }
    );
    let cohort_root = fixture._directory.path().join("publication-cohort");
    let cohort = build_bounded_cohort_retention(
        cohort_root.join("evidence.json"),
        &identity,
        &activity,
        128,
    )
    .expect("build cohort publication");
    client
        .batch_execute(
            "
            CREATE OR REPLACE FUNCTION pensieve_analytics.reject_cohort_test()
            RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                RAISE EXCEPTION 'injected cohort publication failure';
            END;
            $$;
            CREATE TRIGGER reject_cohort_test
            BEFORE INSERT ON pensieve_analytics.cohort_retention_period
            FOR EACH ROW EXECUTE FUNCTION pensieve_analytics.reject_cohort_test();
            ",
        )
        .expect("install cohort failure injection");
    publish_with_all_bounded_products(
        &mut client,
        &fixture.build,
        AllBoundedProducts {
            identity: &identity,
            activity: &activity,
            cohort: &cohort,
        },
        started_at,
        completed_at,
    )
    .expect_err("injected cohort COPY failure must abort publication");
    assert_eq!(
        client
            .query_one(
                "SELECT run_id FROM pensieve_analytics.current_run_metadata",
                &[],
            )
            .expect("read activity run after failed cohort publication")
            .get::<_, String>(0),
        activity_run_id
    );
    client
        .batch_execute(
            "
            DROP TRIGGER reject_cohort_test ON pensieve_analytics.cohort_retention_period;
            DROP FUNCTION pensieve_analytics.reject_cohort_test();
            ",
        )
        .expect("remove cohort failure injection");
    let cohort_run_id = match publish_with_all_bounded_products(
        &mut client,
        &fixture.build,
        AllBoundedProducts {
            identity: &identity,
            activity: &activity,
            cohort: &cohort,
        },
        started_at,
        completed_at,
    )
    .expect("publish cohort run")
    {
        PublishOutcome::Published {
            run_id: cohort_run_id,
            previous_run_id,
        } => {
            assert_eq!(previous_run_id.as_deref(), Some(activity_run_id.as_str()));
            cohort_run_id
        }
        PublishOutcome::AlreadyCurrent { .. } => panic!("cohort run must be new"),
    };
    let cohort_metadata = client
        .query_one(
            "SELECT query_version, cohort_retention_rows FROM pensieve_analytics.current_run_metadata",
            &[],
        )
        .expect("read current cohort metadata");
    assert_eq!(cohort_metadata.get::<_, String>(0), "slice-b3-v1");
    assert_eq!(
        cohort_metadata.get::<_, i64>(1),
        cohort.evidence.period_rows as i64
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pensieve_analytics.current_cohort_retention_period",
                &[],
            )
            .expect("count current cohort rows")
            .get::<_, i64>(0),
        cohort.evidence.period_rows as i64
    );
    assert_eq!(
        publish_with_all_bounded_products(
            &mut client,
            &fixture.build,
            AllBoundedProducts {
                identity: &identity,
                activity: &activity,
                cohort: &cohort,
            },
            started_at,
            completed_at,
        )
        .expect("retry current cohort run"),
        PublishOutcome::AlreadyCurrent {
            run_id: cohort_run_id
        }
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pensieve_analytics.applied_objects WHERE active = true",
                &[],
            )
            .expect("count active applied objects")
            .get::<_, i64>(0),
        2
    );
    let plan = plan_catalog_delta_for_query_version(
        &mut client,
        &fixture.build.snapshot.catalog,
        pensieve_analytics::COHORT_RETENTION_QUERY_VERSION,
    )
    .expect("plan current cohort snapshot");
    assert_eq!(plan.run_kind, PlannedRunKind::NoChange);
    let historical_plan = plan_catalog_delta_from_run(
        &mut client,
        &fixture.build.snapshot.catalog,
        &run_id,
        pensieve_analytics::IDENTITY_QUERY_VERSION,
    )
    .expect("plan from historical identity run");
    assert_eq!(historical_plan.run_kind, PlannedRunKind::NoChange);
    assert_eq!(
        historical_plan.previous_run_id.as_deref(),
        Some(run_id.as_str())
    );
    assert_eq!(
        historical_plan.previous_snapshot_id.as_deref(),
        Some(fixture.build.snapshot.catalog.snapshot_id.as_str())
    );
    assert_eq!(plan.unchanged_objects, 2);
    assert!(plan.added_objects.is_empty());
    assert!(plan.removed_objects.is_empty());
}
