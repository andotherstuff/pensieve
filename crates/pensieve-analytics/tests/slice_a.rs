use std::fs::File;
use std::path::{Path, PathBuf};

use nostr::{Event, EventBuilder, Keys, Kind, Timestamp};
use pensieve_analytics::{AnalyticsBuild, BuildConfig, PublishOutcome, publish, resolve_snapshot};
use pensieve_lake::{
    ActiveRawFragment, Inventory, ObjectKind, ObjectRecord, ObjectState, WorkState,
    WorkUnitRegistration, merge_active_raw_fragments, sha256_file, write_catalog_atomically,
};
use pensieve_parquet::write_events;

const AS_OF: u64 = 1_700_000_000;

struct Fixture {
    _directory: tempfile::TempDir,
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
    let pre_genesis = event(100, 3, "old");
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
        },
    )
    .expect("build analytics");
    Fixture {
        _directory: directory,
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
    let build = AnalyticsBuild::create(
        directory.path().join("empty.duckdb"),
        resolved,
        BuildConfig {
            as_of_epoch: AS_OF,
            code_version: "test".to_owned(),
            s3_region: "test".to_owned(),
            s3_force_path_style: false,
            memory_limit: "1GB".to_owned(),
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
    let first =
        publish(&mut client, &fixture.build, started_at, completed_at).expect("publish first run");
    let run_id = match first {
        PublishOutcome::Published { run_id, .. } => run_id,
        PublishOutcome::AlreadyCurrent { .. } => panic!("database must begin empty"),
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
    assert_eq!(
        publish(&mut client, &fixture.build, started_at, completed_at,).expect("retry current run"),
        PublishOutcome::AlreadyCurrent { run_id }
    );
}
