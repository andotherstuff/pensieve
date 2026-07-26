use arrow_array::{
    Array, FixedSizeBinaryArray, ListArray, RecordBatch, StringArray, UInt16Array, UInt64Array,
};
use bytes::Bytes;
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, Encoding};
use parquet::column::page::Page;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;
use parquet::schema::parser::parse_message_type;
use pensieve_parquet::{
    ARCHIVE_VERSION, ARCHIVE_VERSION_KEY, Error, canonical_arrow_schema, partition_prepared_rows,
    prepare_events, write_events, write_prepared,
};

fn test_keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("valid test secret key")
}

fn signed_event(created_at: u64, kind: Kind, content: &str, tags: Vec<Tag>) -> Event {
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&test_keys())
        .expect("test event should sign")
}

fn write_fixture(events: &[Event]) -> (Vec<u8>, pensieve_parquet::WriteSummary) {
    let mut bytes = Vec::new();
    let summary = write_events(&mut bytes, events.iter()).expect("fixture should write");
    (bytes, summary)
}

#[test]
fn partitions_prepared_rows_by_stable_uncompressed_estimate() {
    let events = [
        signed_event(1, Kind::TextNote, "a", vec![]),
        signed_event(2, Kind::TextNote, "bb", vec![]),
        signed_event(3, Kind::TextNote, "ccc", vec![]),
    ];
    let rows = prepare_events(&events).expect("events should prepare");
    let two_row_target =
        rows[0].estimated_uncompressed_bytes() + rows[1].estimated_uncompressed_bytes();
    assert_eq!(
        partition_prepared_rows(&rows, two_row_target).expect("partition"),
        vec![0..2, 2..3]
    );
    assert_eq!(
        partition_prepared_rows(&rows, 1).expect("oversized rows stand alone"),
        vec![0..1, 1..2, 2..3]
    );
    assert!(matches!(
        partition_prepared_rows(&rows, 0),
        Err(Error::InvalidTargetSize)
    ));
}

fn read_batch(bytes: &[u8]) -> RecordBatch {
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
        .expect("fixture should open")
        .build()
        .expect("Arrow reader should build");
    let batch = reader
        .next()
        .expect("fixture should contain a batch")
        .expect("batch should decode");
    assert!(reader.next().is_none());
    batch
}

fn read_tags(array: &ListArray, row_index: usize) -> Vec<Vec<String>> {
    let tags = array.value(row_index);
    let tags = tags
        .as_any()
        .downcast_ref::<ListArray>()
        .expect("outer list values should be inner lists");

    (0..tags.len())
        .map(|tag_index| {
            let values = tags.value(tag_index);
            let values = values
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("inner list values should be strings");
            values
                .iter()
                .map(|value| value.unwrap().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn writes_exact_v1_schema_profile_and_metadata() {
    let event = signed_event(
        1_700_000_000,
        Kind::TextNote,
        "",
        vec![
            Tag::parse(["alt"]).expect("one-element tag"),
            Tag::parse(["client", "", "third"]).expect("variable tag"),
        ],
    );
    let (bytes, summary) = write_fixture(&[event]);

    assert_eq!(summary.input_events, 1);
    assert_eq!(summary.output_rows, 1);
    assert_eq!(summary.duplicate_events, 0);
    assert_eq!(summary.row_groups, 1);

    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(&bytes))
        .expect("fixture should open");
    let metadata = builder.metadata();
    let expected_schema = parse_message_type(
        r#"
        message nostr_event_archive_v1 {
          required fixed_len_byte_array(32) id;
          required fixed_len_byte_array(32) pubkey;
          required int64 created_at (INTEGER(64, false));
          required int32 kind (INTEGER(16, false));
          required group tags (LIST) {
            repeated group list {
              required group element (LIST) {
                repeated group list {
                  required binary element (STRING);
                }
              }
            }
          }
          required binary content (STRING);
          required fixed_len_byte_array(64) sig;
        }
        "#,
    )
    .expect("expected schema should parse");
    assert_eq!(builder.parquet_schema().root_schema(), &expected_schema);

    let key_values = metadata
        .file_metadata()
        .key_value_metadata()
        .expect("V1 version metadata should exist");
    assert_eq!(key_values.len(), 1);
    assert_eq!(key_values[0].key, ARCHIVE_VERSION_KEY);
    assert_eq!(key_values[0].value.as_deref(), Some(ARCHIVE_VERSION));

    let allowed_encodings = [Encoding::PLAIN, Encoding::RLE, Encoding::RLE_DICTIONARY];
    for column in metadata.row_group(0).columns() {
        assert_eq!(column.compression(), Compression::ZSTD(Default::default()));
        assert!(
            column
                .encodings()
                .all(|encoding| allowed_encodings.contains(&encoding)),
            "unexpected encoding for {}",
            column.column_path().string()
        );
    }

    let reader =
        SerializedFileReader::new(Bytes::copy_from_slice(&bytes)).expect("page reader should open");
    for column_index in 0..reader
        .metadata()
        .file_metadata()
        .schema_descr()
        .num_columns()
    {
        let mut pages = reader
            .get_row_group(0)
            .expect("row group should exist")
            .get_column_page_reader(column_index)
            .expect("column page reader should open");
        let mut saw_data_page = false;
        while let Some(page) = pages.get_next_page().expect("page should decode") {
            match page {
                Page::DataPage { .. } => saw_data_page = true,
                Page::DataPageV2 { .. } => panic!("V1 files must use Data Page V1"),
                Page::DictionaryPage { .. } => {}
            }
        }
        assert!(saw_data_page);
    }
}

#[test]
fn round_trips_edge_values_and_sorts_and_deduplicates() {
    let early = signed_event(1, Kind::TextNote, "", vec![]);
    let late = signed_event(
        i64::MAX as u64 + 1,
        Kind::Custom(u16::MAX),
        "  \nUnicode: 🦀\n{\"exact\":true}",
        vec![
            Tag::parse(["alt"]).expect("one-element tag"),
            Tag::parse(["x", "", "終"]).expect("tag with empty string"),
        ],
    );

    let (bytes, summary) = write_fixture(&[late.clone(), early.clone(), early]);
    assert_eq!(summary.input_events, 3);
    assert_eq!(summary.output_rows, 2);
    assert_eq!(summary.duplicate_events, 1);

    let batch = read_batch(&bytes);
    assert_eq!(batch.schema(), canonical_arrow_schema());
    assert_eq!(batch.num_rows(), 2);

    let created_at = batch
        .column(2)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .expect("created_at should remain unsigned");
    assert_eq!(created_at.values(), &[1, i64::MAX as u64 + 1]);

    let kind = batch
        .column(3)
        .as_any()
        .downcast_ref::<UInt16Array>()
        .expect("kind should remain unsigned");
    assert_eq!(kind.value(1), u16::MAX);

    let tags = batch
        .column(4)
        .as_any()
        .downcast_ref::<ListArray>()
        .expect("tags should be a list");
    assert_eq!(read_tags(tags, 0), Vec::<Vec<String>>::new());
    assert_eq!(
        read_tags(tags, 1),
        vec![
            vec!["alt".to_string()],
            vec!["x".to_string(), String::new(), "終".to_string()]
        ]
    );

    let content = batch
        .column(5)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("content should be UTF-8");
    assert_eq!(content.value(0), "");
    assert_eq!(content.value(1), "  \nUnicode: 🦀\n{\"exact\":true}");

    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
        .expect("fixture should reopen");
    let created_at_column = &builder.metadata().row_group(0).columns()[2];
    let statistics = created_at_column
        .statistics()
        .expect("created_at statistics should exist");
    let Statistics::Int64(statistics) = statistics else {
        panic!("created_at should have INT64 physical statistics");
    };
    assert_eq!(statistics.null_count_opt(), Some(0));
    assert_eq!(
        statistics.min_opt().copied().map(|value| value as u64),
        Some(1)
    );
    assert_eq!(
        statistics.max_opt().copied().map(|value| value as u64),
        Some(i64::MAX as u64 + 1)
    );
}

#[test]
fn rejects_invalid_events_before_writing_bytes() {
    let valid = signed_event(1_700_000_000, Kind::TextNote, "original", vec![]);
    let tampered_json = valid.as_json().replace("original", "tampered");
    let tampered = Event::from_json(tampered_json).expect("tampered event should deserialize");
    let mut bytes = Vec::new();

    let error = write_events(&mut bytes, [&tampered]).expect_err("invalid ID must fail");
    assert!(matches!(error, Error::InvalidEventId { .. }));
    assert!(bytes.is_empty());
}

#[test]
fn equal_timestamps_are_sorted_by_raw_event_id() {
    let first = signed_event(1_700_000_000, Kind::TextNote, "first", vec![]);
    let second = signed_event(1_700_000_000, Kind::TextNote, "second", vec![]);
    let mut expected = [*first.id.as_bytes(), *second.id.as_bytes()];
    expected.sort_unstable();

    let (bytes, _) = write_fixture(&[second, first]);
    let batch = read_batch(&bytes);
    let ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .expect("id should be fixed-size binary");

    assert_eq!(ids.value(0), expected[0]);
    assert_eq!(ids.value(1), expected[1]);
}

#[test]
fn duplicate_id_keeps_lexicographically_smallest_valid_signature() {
    let first = signed_event(1_700_000_000, Kind::TextNote, "same event", vec![]);
    let second = signed_event(1_700_000_000, Kind::TextNote, "same event", vec![]);
    assert_eq!(first.id, second.id);
    assert_ne!(first.sig, second.sig);

    let first_sig: [u8; 64] = *first.sig.as_ref();
    let second_sig: [u8; 64] = *second.sig.as_ref();
    let expected = std::cmp::min(first_sig, second_sig);

    let (bytes, summary) = write_fixture(&[first, second]);
    assert_eq!(summary.input_events, 2);
    assert_eq!(summary.output_rows, 1);
    assert_eq!(summary.duplicate_events, 1);

    let batch = read_batch(&bytes);
    let signatures = batch
        .column(6)
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .expect("signature should be fixed-size binary");
    assert_eq!(signatures.value(0), expected);
}

#[test]
fn write_prepared_rejects_unsorted_rows() {
    let early = signed_event(1, Kind::TextNote, "early", vec![]);
    let late = signed_event(2, Kind::TextNote, "late", vec![]);
    let mut rows = prepare_events([&early, &late]).expect("events should prepare");
    rows.reverse();
    let mut bytes = Vec::new();

    let error = write_prepared(&mut bytes, &rows).expect_err("unsorted rows must fail");
    assert!(matches!(error, Error::UnsortedOrDuplicateRows));
    assert!(bytes.is_empty());
}

#[test]
fn refuses_to_create_empty_files() {
    let mut bytes = Vec::new();
    let events: [&Event; 0] = [];

    let error = write_events(&mut bytes, events).expect_err("empty batch must fail");
    assert!(matches!(error, Error::EmptyBatch));
    assert!(bytes.is_empty());
}
