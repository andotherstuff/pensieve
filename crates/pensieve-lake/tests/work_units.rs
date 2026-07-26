use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use notepack::NoteBinary;
use pensieve_lake::{
    CampaignConfig, Error, Inventory, LocalObjectStore, ObjectKind, ObjectState, PublishedObject,
    Publisher, WorkState, cleanup_published_local_artifacts, run_notepack_work_unit, sha256_file,
};
use pensieve_parquet::validate_file;

fn keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("valid test key")
}

fn event(created_at: u64, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .tag(Tag::parse(["alt"]).expect("tag"))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&keys())
        .expect("signed event")
}

fn pack(event: &Event, mutate_id: bool) -> Vec<u8> {
    let tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
        .collect();
    let mut id = *event.id.as_bytes();
    if mutate_id {
        id[0] ^= 1;
    }
    NoteBinary {
        id: &id,
        pubkey: event.pubkey.as_bytes(),
        sig: event.sig.as_ref(),
        content: &event.content,
        created_at: event.created_at.as_secs(),
        kind: u64::from(event.kind.as_u16()),
        tags: &tags,
    }
    .pack()
}

fn segment(path: &Path) {
    let events: Vec<_> = (0..5)
        .map(|index| event(index + 1, &format!("event-{index}-{}", "x".repeat(80))))
        .collect();
    let file = File::create(path).expect("segment file");
    let mut gzip = GzEncoder::new(file, Compression::default());
    for payload in events
        .iter()
        .map(|event| pack(event, false))
        .chain(std::iter::once(pack(&events[0], false)))
        .chain(std::iter::once(pack(&events[1], true)))
    {
        gzip.write_all(&(payload.len() as u32).to_le_bytes())
            .expect("frame length");
        gzip.write_all(&payload).expect("frame payload");
    }
    gzip.finish().expect("gzip finish");
}

fn empty_gzip_segment(path: &Path) {
    GzEncoder::new(
        File::create(path).expect("empty gzip segment"),
        Compression::default(),
    )
    .finish()
    .expect("finish empty gzip segment");
}

fn config(staging_dir: &Path) -> CampaignConfig {
    CampaignConfig {
        staging_dir: staging_dir.to_owned(),
        object_prefix: "test/v1".to_owned(),
        target_uncompressed_bytes: 500,
        max_event_bytes: 16 * 1024 * 1024,
    }
}

#[test]
fn records_empty_plain_and_gzip_segments_as_published_no_ops() {
    let directory = tempfile::tempdir().expect("temp directory");
    let plain = directory.path().join("empty.notepack");
    File::create(&plain).expect("empty plain segment");
    let plain_alias = directory.path().join("empty-alias.notepack");
    File::create(&plain_alias).expect("empty plain alias");
    let gzip = directory.path().join("empty.notepack.gz");
    empty_gzip_segment(&gzip);

    let mut inventory =
        Inventory::open(directory.path().join("inventory.sqlite")).expect("inventory");
    let store = LocalObjectStore::new(directory.path().join("lake")).expect("object store");
    let staging = directory.path().join("staging");
    let config = config(&staging);

    let plain_summary = run_notepack_work_unit(&mut inventory, &store, &plain, &config)
        .expect("publish empty plain segment");
    assert_eq!(plain_summary.state, WorkState::Published);
    assert_eq!(plain_summary.input_events, 0);
    assert_eq!(plain_summary.output_rows, 0);
    assert_eq!(plain_summary.rejected_events, 0);
    assert_eq!(plain_summary.parquet_objects, 0);
    assert!(!plain_summary.resumed);
    assert!(
        inventory
            .objects_for_work(&plain_summary.work_unit_id)
            .expect("plain objects")
            .is_empty()
    );

    let alias_summary = run_notepack_work_unit(&mut inventory, &store, &plain_alias, &config)
        .expect("resume identical empty source");
    assert_eq!(alias_summary.work_unit_id, plain_summary.work_unit_id);
    assert!(alias_summary.resumed);

    let gzip_summary = run_notepack_work_unit(&mut inventory, &store, &gzip, &config)
        .expect("publish empty gzip segment");
    assert_eq!(gzip_summary.state, WorkState::Published);
    assert_eq!(gzip_summary.input_events, 0);
    assert_eq!(gzip_summary.output_rows, 0);
    assert_eq!(gzip_summary.rejected_events, 0);
    assert_eq!(gzip_summary.parquet_objects, 0);
    assert!(!gzip_summary.resumed);
    assert_ne!(gzip_summary.work_unit_id, plain_summary.work_unit_id);
    assert!(
        inventory
            .objects_for_work(&gzip_summary.work_unit_id)
            .expect("gzip objects")
            .is_empty()
    );
    assert!(
        inventory
            .active_raw_objects()
            .expect("active objects")
            .is_empty()
    );

    assert_eq!(
        cleanup_published_local_artifacts(&inventory, &plain_summary.work_unit_id, &staging)
            .expect("clean plain no-op"),
        Default::default()
    );
    assert_eq!(
        cleanup_published_local_artifacts(&inventory, &gzip_summary.work_unit_id, &staging)
            .expect("clean gzip no-op"),
        Default::default()
    );
    assert!(plain.is_file());
    assert!(plain_alias.is_file());
    assert!(gzip.is_file());
}

#[test]
fn publishes_multiple_parts_quarantine_and_resumes_idempotently() {
    let directory = tempfile::tempdir().expect("temp directory");
    let input = directory.path().join("segment.notepack.gz");
    segment(&input);
    let mut inventory =
        Inventory::open(directory.path().join("inventory.sqlite")).expect("inventory");
    let store = LocalObjectStore::new(directory.path().join("lake")).expect("object store");
    let config = config(&directory.path().join("staging"));

    let first = run_notepack_work_unit(&mut inventory, &store, &input, &config)
        .expect("first campaign run");
    assert_eq!(first.state, WorkState::Published);
    assert_eq!(first.input_events, 7);
    assert_eq!(first.output_rows, 5);
    assert_eq!(first.rejected_events, 1);
    assert!(first.parquet_objects >= 2);
    assert!(!first.resumed);

    let objects = inventory
        .objects_for_work(&first.work_unit_id)
        .expect("objects");
    assert_eq!(
        objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Parquet)
            .count(),
        first.parquet_objects
    );
    assert!(objects.iter().any(|object| {
        object.kind == ObjectKind::Reject && object.state == ObjectState::Quarantined
    }));
    for object in &objects {
        assert_eq!(
            object.writer_version,
            pensieve_parquet::IMPLEMENTATION_VERSION
        );
        let published = store
            .path_for_key(&object.object_key)
            .expect("published path");
        assert_eq!(
            published.metadata().expect("metadata").len(),
            object.byte_size
        );
        assert_eq!(sha256_file(&published).expect("checksum"), object.sha256);
        if object.kind == ObjectKind::Parquet {
            validate_file(published).expect("published canonical object");
        }
    }
    assert_eq!(
        inventory
            .active_raw_objects()
            .expect("active objects")
            .len(),
        first.parquet_objects
    );

    let second =
        run_notepack_work_unit(&mut inventory, &store, &input, &config).expect("idempotent resume");
    assert_eq!(second.state, WorkState::Published);
    assert_eq!(second.work_unit_id, first.work_unit_id);
    assert!(second.resumed);
    assert_eq!(second.parquet_objects, first.parquet_objects);

    let mut incompatible = config.clone();
    incompatible.object_prefix = "different/v1".to_string();
    assert!(matches!(
        run_notepack_work_unit(&mut inventory, &store, &input, &incompatible),
        Err(Error::WorkUnitConflict { .. })
    ));
}

#[test]
fn publication_failure_never_activates_partial_set_and_retry_converges() {
    let directory = tempfile::tempdir().expect("temp directory");
    let input = directory.path().join("segment.notepack.gz");
    segment(&input);
    let mut inventory =
        Inventory::open(directory.path().join("inventory.sqlite")).expect("inventory");
    let store = LocalObjectStore::new(directory.path().join("lake")).expect("object store");
    let config = config(&directory.path().join("staging"));
    let failing = FailAfterOne {
        inner: store.clone(),
        calls: AtomicUsize::new(0),
    };

    assert!(run_notepack_work_unit(&mut inventory, &failing, &input, &config).is_err());
    assert!(
        inventory
            .active_raw_objects()
            .expect("active objects")
            .is_empty()
    );
    let work_unit_id = format!(
        "notepack-sha256-{}",
        sha256_file(&input).expect("source checksum")
    );
    assert_eq!(
        inventory
            .work_unit(&work_unit_id)
            .expect("work query")
            .expect("work exists")
            .state,
        WorkState::Failed
    );

    let resumed = run_notepack_work_unit(&mut inventory, &store, &input, &config)
        .expect("retry should converge");
    assert_eq!(resumed.state, WorkState::Published);
    assert!(resumed.resumed);
    assert_eq!(
        inventory
            .active_raw_objects()
            .expect("active objects")
            .len(),
        resumed.parquet_objects
    );
}

#[test]
fn cleans_only_published_staging_and_remains_resumable() {
    let directory = tempfile::tempdir().expect("temp directory");
    let input = directory.path().join("segment.notepack.gz");
    segment(&input);
    let mut inventory =
        Inventory::open(directory.path().join("inventory.sqlite")).expect("inventory");
    let store = LocalObjectStore::new(directory.path().join("lake")).expect("object store");
    let staging = directory.path().join("staging");
    let config = config(&staging);

    let published = run_notepack_work_unit(&mut inventory, &store, &input, &config)
        .expect("initial publication");
    let objects = inventory
        .objects_for_work(&published.work_unit_id)
        .expect("objects");
    assert!(objects.iter().all(|object| object.local_path.is_file()));
    let work_dir = staging.join(&published.work_unit_id);
    fs::write(work_dir.join(".tmp-crash-orphan"), b"partial").expect("orphaned temporary file");
    let reject_candidate = work_dir.join(".reject-candidate-crash");
    fs::create_dir(&reject_candidate).expect("orphaned temporary directory");
    fs::write(reject_candidate.join("rejects.notepack.gz"), b"partial")
        .expect("nested orphaned artifact");

    let cleanup = cleanup_published_local_artifacts(&inventory, &published.work_unit_id, &staging)
        .expect("published cleanup");
    assert_eq!(cleanup.files, objects.len());
    assert_eq!(
        cleanup.bytes,
        objects.iter().map(|object| object.byte_size).sum::<u64>()
    );
    assert!(input.is_file());
    assert!(objects.iter().all(|object| !object.local_path.exists()));
    assert!(!work_dir.exists());

    assert_eq!(
        cleanup_published_local_artifacts(&inventory, &published.work_unit_id, &staging)
            .expect("idempotent cleanup"),
        Default::default()
    );
    let resumed = run_notepack_work_unit(&mut inventory, &store, &input, &config).expect("resume");
    assert_eq!(resumed.state, WorkState::Published);
    assert!(resumed.resumed);
}

#[test]
fn refuses_cleanup_before_publication() {
    let directory = tempfile::tempdir().expect("temp directory");
    let input = directory.path().join("segment.notepack.gz");
    segment(&input);
    let mut inventory =
        Inventory::open(directory.path().join("inventory.sqlite")).expect("inventory");
    let store = LocalObjectStore::new(directory.path().join("lake")).expect("object store");
    let staging = directory.path().join("staging");
    let config = config(&staging);
    let failing = FailAfterOne {
        inner: store,
        calls: AtomicUsize::new(0),
    };

    assert!(run_notepack_work_unit(&mut inventory, &failing, &input, &config).is_err());
    let work_unit_id = format!(
        "notepack-sha256-{}",
        sha256_file(&input).expect("source checksum")
    );
    assert!(matches!(
        cleanup_published_local_artifacts(&inventory, &work_unit_id, &staging),
        Err(Error::InvalidTransition { .. })
    ));
    assert!(
        inventory
            .objects_for_work(&work_unit_id)
            .expect("objects")
            .iter()
            .all(|object| object.local_path.is_file())
    );
}

#[test]
fn resumes_from_every_post_validation_journal_state() {
    for resume_state in ["validated", "uploading", "uploaded"] {
        let directory = tempfile::tempdir().expect("temp directory");
        let input = directory.path().join("segment.notepack.gz");
        let inventory_path = directory.path().join("inventory.sqlite");
        segment(&input);
        let store = LocalObjectStore::new(directory.path().join("lake")).expect("object store");
        let config = config(&directory.path().join("staging"));
        let mut inventory = Inventory::open(&inventory_path).expect("inventory");
        let initial = run_notepack_work_unit(&mut inventory, &store, &input, &config)
            .expect("initial publication");
        drop(inventory);

        let connection = rusqlite::Connection::open(&inventory_path).expect("raw inventory");
        connection
            .execute(
                "UPDATE work_units SET state = ?1 WHERE id = ?2",
                [resume_state, initial.work_unit_id.as_str()],
            )
            .expect("rewind work state");
        let object_state = if resume_state == "uploaded" {
            "uploaded"
        } else {
            "validated"
        };
        connection
            .execute(
                "UPDATE objects SET state = ?1 WHERE work_unit_id = ?2",
                [object_state, initial.work_unit_id.as_str()],
            )
            .expect("rewind object states");
        if resume_state == "uploading" {
            connection
                .execute(
                    r#"
                    UPDATE objects SET state = 'uploaded'
                    WHERE object_key = (
                        SELECT min(object_key) FROM objects WHERE work_unit_id = ?1
                    )
                    "#,
                    [&initial.work_unit_id],
                )
                .expect("simulate partial upload");
        }
        drop(connection);

        let mut inventory = Inventory::open(&inventory_path).expect("reopen inventory");
        let resumed = run_notepack_work_unit(&mut inventory, &store, &input, &config)
            .expect("resume publication");
        assert_eq!(resumed.state, WorkState::Published, "{resume_state}");
        assert!(resumed.resumed, "{resume_state}");
        assert_eq!(
            inventory
                .active_raw_objects()
                .expect("active objects")
                .len(),
            resumed.parquet_objects,
            "{resume_state}"
        );
    }
}

struct FailAfterOne {
    inner: LocalObjectStore,
    calls: AtomicUsize,
}

impl Publisher for FailAfterOne {
    fn publish(
        &self,
        key: &str,
        source: &Path,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> pensieve_lake::Result<PublishedObject> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= 1 {
            return Err(Error::Io(std::io::Error::other(
                "injected publication failure",
            )));
        }
        self.inner
            .publish(key, source, expected_bytes, expected_sha256)
    }
}
