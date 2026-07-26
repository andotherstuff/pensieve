use std::fs::{self, File};
use std::path::Path;

use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use notepack::NoteBinary;
use pensieve_parquet::{CanonicalEvent, RawEvent, validate_file, write_events};

fn test_keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("valid test secret key")
}

fn signed_event() -> Event {
    EventBuilder::new(Kind::TextNote, "")
        .tags([
            Tag::parse(["alt"]).expect("one-element tag"),
            Tag::parse([
                "e",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])
            .expect("compact hex tag"),
            Tag::parse(["client", "", "終"]).expect("variable tag"),
        ])
        .custom_created_at(Timestamp::from(i64::MAX as u64 + 1))
        .sign_with_keys(&test_keys())
        .expect("event should sign")
}

fn raw_event(event: &Event) -> RawEvent {
    RawEvent {
        id: *event.id.as_bytes(),
        pubkey: *event.pubkey.as_bytes(),
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16(),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
            .collect(),
        content: event.content.clone(),
        sig: *event.sig.as_ref(),
    }
}

#[test]
fn validates_raw_and_notepack_without_json() {
    let event = signed_event();
    let raw = raw_event(&event);
    assert_eq!(
        CanonicalEvent::from_raw(raw.clone()).expect("raw row should validate"),
        CanonicalEvent::from_event(&event).expect("typed event should validate")
    );

    let tags = raw.tags.clone();
    let payload = NoteBinary {
        id: &raw.id,
        pubkey: &raw.pubkey,
        sig: &raw.sig,
        content: &raw.content,
        created_at: raw.created_at,
        kind: u64::from(raw.kind),
        tags: &tags,
    }
    .pack();
    let decoded = CanonicalEvent::from_notepack(&payload).expect("notepack should validate");

    assert_eq!(decoded.id(), &raw.id);
    assert_eq!(decoded.tags(), raw.tags);
    assert_eq!(decoded.content(), "");
    assert_eq!(decoded.created_at(), i64::MAX as u64 + 1);
}

#[test]
fn strict_validator_accepts_writer_output() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("valid.parquet");
    let event = signed_event();
    write_events(
        File::create(&path).expect("create fixture"),
        std::iter::once(&event),
    )
    .expect("write fixture");

    let report = validate_file(path).expect("writer output should strictly validate");
    assert_eq!(report.rows, 1);
    assert_eq!(report.row_groups, 1);
    assert_eq!(report.min_created_at, Some(i64::MAX as u64 + 1));
    assert_eq!(report.max_created_at, report.min_created_at);
}

#[test]
fn raw_validation_rejects_mutated_id() {
    let event = signed_event();
    let mut raw = raw_event(&event);
    raw.id[0] ^= 1;

    assert!(CanonicalEvent::from_raw(raw).is_err());
}

#[test]
fn checked_in_fixture_corpus_has_expected_conformance() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let valid = validate_file(fixtures.join("valid-v1.parquet"))
        .expect("canonical interoperability fixture should validate");
    assert_eq!(valid.rows, 3);
    assert_eq!(valid.min_created_at, Some(0));
    assert_eq!(valid.max_created_at, Some(i64::MAX as u64 + 1));

    let mut invalid_count = 0;
    let expected_errors = [
        ("invalid-bad-id.parquet", "invalid ID"),
        ("invalid-bad-signature.parquet", "invalid signature"),
        ("invalid-data-page-v2.parquet", "Data Page V2"),
        ("invalid-duplicate-id.parquet", "not strictly ordered"),
        (
            "invalid-duplicate-version-metadata.parquet",
            "duplicate reserved footer metadata key",
        ),
        ("invalid-empty-inner-tag.parquet", "empty tag"),
        (
            "invalid-missing-created-at-statistics.parquet",
            "created_at statistics are missing",
        ),
        (
            "invalid-missing-version.parquet",
            "required footer metadata",
        ),
        (
            "invalid-null-content.parquet",
            "schema does not exactly match",
        ),
        ("invalid-truncated-footer.parquet", "Parquet error"),
        ("invalid-unsorted.parquet", "not strictly ordered"),
        (
            "invalid-wrong-id-length.parquet",
            "schema does not exactly match",
        ),
        (
            "invalid-wrong-root-schema.parquet",
            "schema does not exactly match",
        ),
    ];
    for entry in fs::read_dir(fixtures).expect("fixture directory") {
        let path = entry.expect("fixture entry").path();
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let is_invalid_parquet = filename.starts_with("invalid-") && filename.ends_with(".parquet");
        if is_invalid_parquet {
            invalid_count += 1;
            let error = validate_file(&path)
                .expect_err("invalid fixture unexpectedly passed strict validation")
                .to_string();
            let expected = expected_errors
                .iter()
                .find_map(|(name, expected)| (*name == filename).then_some(*expected))
                .unwrap_or_else(|| panic!("missing expected error for {filename}"));
            assert!(error.contains(expected), "{filename}: {error:?}");
        }
    }
    assert_eq!(invalid_count, 13);
}
