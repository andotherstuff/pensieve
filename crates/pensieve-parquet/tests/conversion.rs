use std::fs::{self, File};
use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use notepack::NoteBinary;
use pensieve_parquet::{
    DEFAULT_MAX_EVENT_BYTES, Error, SALVAGE_REPORT_NAME, SALVAGED_SEGMENT_NAME, SalvageReport,
    TRUNCATED_TAIL_NAME, convert_segment, convert_segment_quarantining_invalid,
    read_salvage_report, salvage_truncated_segment, scan_framed_notepack, validate_file,
};

fn test_keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("valid test secret key")
}

fn event(created_at: u64, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .tag(Tag::parse(["alt"]).expect("one-element tag"))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&test_keys())
        .expect("event should sign")
}

fn pack(event: &Event) -> Vec<u8> {
    let tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
        .collect();
    NoteBinary {
        id: event.id.as_bytes(),
        pubkey: event.pubkey.as_bytes(),
        sig: event.sig.as_ref(),
        content: &event.content,
        created_at: event.created_at.as_secs(),
        kind: u64::from(event.kind.as_u16()),
        tags: &tags,
    }
    .pack()
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) {
    writer
        .write_all(&(payload.len() as u32).to_le_bytes())
        .expect("frame length");
    writer.write_all(payload).expect("frame payload");
}

#[test]
fn converts_gzipped_segment_atomically_with_sorting_and_deduplication() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("segment.notepack.gz");
    let output = directory.path().join("events.parquet");
    let early = pack(&event(1, ""));
    let late = pack(&event(2, "later"));
    let file = File::create(&input).expect("input file");
    let mut gzip = GzEncoder::new(file, Compression::default());
    for payload in [&late, &early, &early] {
        write_frame(&mut gzip, payload);
    }
    gzip.finish().expect("finish gzip");

    let summary =
        convert_segment(&input, &output, DEFAULT_MAX_EVENT_BYTES).expect("convert segment");
    assert_eq!(summary.input_events, 3);
    assert_eq!(summary.output_rows, 2);
    assert_eq!(summary.duplicate_events, 1);
    assert_eq!(summary.row_groups, 1);
    assert_eq!(summary.input_file_bytes, input.metadata().unwrap().len());
    assert_eq!(summary.output_file_bytes, output.metadata().unwrap().len());

    let validation = validate_file(&output).expect("converted file should validate");
    assert_eq!(validation.rows, 2);
    assert_eq!(validation.min_created_at, Some(1));
    assert_eq!(validation.max_created_at, Some(2));

    assert!(matches!(
        convert_segment(&input, &output, DEFAULT_MAX_EVENT_BYTES),
        Err(Error::OutputExists { .. })
    ));
}

#[test]
fn quarantine_mode_preserves_invalid_frames_and_writes_only_valid_rows() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("segment.notepack");
    let output = directory.path().join("events.parquet");
    let rejects = directory.path().join("rejects.notepack");
    let valid_event = event(1, "valid");
    let valid = pack(&valid_event);

    let tags: Vec<Vec<String>> = valid_event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
        .collect();
    let mut invalid_id = *valid_event.id.as_bytes();
    invalid_id[0] ^= 1;
    let invalid = NoteBinary {
        id: &invalid_id,
        pubkey: valid_event.pubkey.as_bytes(),
        sig: valid_event.sig.as_ref(),
        content: &valid_event.content,
        created_at: valid_event.created_at.as_secs(),
        kind: u64::from(valid_event.kind.as_u16()),
        tags: &tags,
    }
    .pack();
    let mut segment = File::create(&input).expect("input segment");
    write_frame(&mut segment, &invalid);
    write_frame(&mut segment, &valid);
    segment.sync_all().expect("sync input");

    let summary =
        convert_segment_quarantining_invalid(&input, &output, &rejects, DEFAULT_MAX_EVENT_BYTES)
            .expect("quarantine conversion");
    assert_eq!(summary.input_events, 2);
    assert_eq!(summary.output_rows, 1);
    assert_eq!(summary.rejected_events, 1);
    assert_eq!(validate_file(output).expect("valid output").rows, 1);

    let rejected_scan = scan_framed_notepack(
        File::open(rejects).expect("reject segment"),
        DEFAULT_MAX_EVENT_BYTES,
    )
    .expect("reject segment should retain valid framing");
    assert!(rejected_scan.events.is_empty());
    assert_eq!(rejected_scan.rejected.len(), 1);
}

#[test]
fn truncated_segment_never_publishes_an_output_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("truncated.notepack");
    let output = directory.path().join("events.parquet");
    fs::write(&input, [10, 0, 0, 0, 1, 2, 3]).expect("truncated fixture");

    assert!(matches!(
        convert_segment(&input, &output, DEFAULT_MAX_EVENT_BYTES),
        Err(Error::TruncatedFrame { frame_index: 0 })
    ));
    assert!(!output.exists());
}

#[test]
fn terminal_truncation_salvage_preserves_complete_frames_and_exact_tail() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("segment-000000001.notepack");
    let bundle = directory.path().join("salvage");
    let valid = pack(&event(1, "valid"));
    let mut segment = File::create(&input).expect("input segment");
    write_frame(&mut segment, &[1, 2, 3]);
    write_frame(&mut segment, &valid);
    segment
        .write_all(&10u32.to_le_bytes())
        .expect("tail length");
    segment.write_all(&[4, 5, 6]).expect("tail payload");
    segment.sync_all().expect("sync input");

    let report = salvage_truncated_segment(&input, &bundle, DEFAULT_MAX_EVENT_BYTES)
        .expect("salvage terminal truncation");
    assert_eq!(report.complete_frames(), 2);
    assert_eq!(report.valid_events(), 1);
    assert_eq!(report.rejected_events(), 1);
    assert_eq!(report.truncated_frame_index(), 2);

    let scan = scan_framed_notepack(
        File::open(bundle.join(SALVAGED_SEGMENT_NAME)).expect("salvaged segment"),
        DEFAULT_MAX_EVENT_BYTES,
    )
    .expect("complete salvaged prefix");
    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.rejected.len(), 1);
    let mut expected_tail = 10u32.to_le_bytes().to_vec();
    expected_tail.extend([4, 5, 6]);
    assert_eq!(
        fs::read(bundle.join(TRUNCATED_TAIL_NAME)).expect("tail"),
        expected_tail
    );
    let stored_report: SalvageReport =
        serde_json::from_slice(&fs::read(bundle.join(SALVAGE_REPORT_NAME)).expect("report"))
            .expect("canonical report JSON");
    assert_eq!(stored_report, report);
    assert_eq!(
        read_salvage_report(bundle.join(SALVAGE_REPORT_NAME)).expect("validated report"),
        report
    );
    assert!(matches!(
        salvage_truncated_segment(&input, &bundle, DEFAULT_MAX_EVENT_BYTES),
        Err(Error::OutputExists { .. })
    ));
}

#[test]
fn complete_segment_is_not_silently_reclassified_as_salvage() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("complete.notepack");
    let bundle = directory.path().join("salvage");
    let mut segment = File::create(&input).expect("input segment");
    write_frame(&mut segment, &pack(&event(1, "complete")));
    segment.sync_all().expect("sync input");

    assert!(matches!(
        salvage_truncated_segment(&input, &bundle, DEFAULT_MAX_EVENT_BYTES),
        Err(Error::SegmentNotTruncated)
    ));
    assert!(!bundle.exists());
}
