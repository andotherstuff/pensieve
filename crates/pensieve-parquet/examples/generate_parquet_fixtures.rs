//! Regenerate the checked-in canonical V1 interoperability fixture corpus.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::builder::{
    FixedSizeBinaryBuilder, ListBuilder, StringBuilder, UInt16Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use nostr::prelude::rand::{SeedableRng, rngs::StdRng};
use nostr::{Event, EventBuilder, Keys, Kind, SECP256K1, Tag, Timestamp};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_writer::ArrowWriterOptions;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use pensieve_parquet::{
    ARCHIVE_VERSION, ARCHIVE_VERSION_KEY, ROW_GROUP_TARGET_BYTES, RawEvent, canonical_arrow_schema,
    write_events,
};

#[derive(Clone, Copy)]
struct RawWriterOptions {
    root_name: &'static str,
    version_metadata: bool,
    duplicate_version_metadata: bool,
    writer_version: WriterVersion,
    statistics: EnabledStatistics,
}

const CANONICAL_OPTIONS: RawWriterOptions = RawWriterOptions {
    root_name: "nostr_event_archive_v1",
    version_metadata: true,
    duplicate_version_metadata: false,
    writer_version: WriterVersion::PARQUET_1_0,
    statistics: EnabledStatistics::Chunk,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
        });
    fs::create_dir_all(&output)?;

    let events = fixture_events()?;
    let valid_path = output.join("valid-v1.parquet");
    write_events(fs::File::create(&valid_path)?, events.iter())?;

    let rows: Vec<_> = events.iter().map(raw_event).collect();
    write_raw_fixture(
        output.join("invalid-missing-version.parquet"),
        &rows,
        RawWriterOptions {
            version_metadata: false,
            ..CANONICAL_OPTIONS
        },
    )?;
    write_raw_fixture(
        output.join("invalid-wrong-root-schema.parquet"),
        &rows,
        RawWriterOptions {
            root_name: "not_the_canonical_root",
            ..CANONICAL_OPTIONS
        },
    )?;
    write_raw_fixture(
        output.join("invalid-data-page-v2.parquet"),
        &rows,
        RawWriterOptions {
            writer_version: WriterVersion::PARQUET_2_0,
            ..CANONICAL_OPTIONS
        },
    )?;

    let mut unsorted = rows.clone();
    unsorted.reverse();
    write_raw_fixture(
        output.join("invalid-unsorted.parquet"),
        &unsorted,
        CANONICAL_OPTIONS,
    )?;

    let mut bad_id = rows.clone();
    bad_id[0].id[0] ^= 1;
    write_raw_fixture(
        output.join("invalid-bad-id.parquet"),
        &bad_id,
        CANONICAL_OPTIONS,
    )?;

    let mut empty_tag = rows.clone();
    empty_tag[0].tags.push(Vec::new());
    write_raw_fixture(
        output.join("invalid-empty-inner-tag.parquet"),
        &empty_tag,
        CANONICAL_OPTIONS,
    )?;

    let mut bad_signature = rows.clone();
    bad_signature[0].sig[0] ^= 1;
    write_raw_fixture(
        output.join("invalid-bad-signature.parquet"),
        &bad_signature,
        CANONICAL_OPTIONS,
    )?;

    let duplicate_rows = vec![rows[0].clone(), rows[0].clone(), rows[1].clone()];
    write_raw_fixture(
        output.join("invalid-duplicate-id.parquet"),
        &duplicate_rows,
        CANONICAL_OPTIONS,
    )?;

    write_raw_fixture(
        output.join("invalid-missing-created-at-statistics.parquet"),
        &rows,
        RawWriterOptions {
            statistics: EnabledStatistics::None,
            ..CANONICAL_OPTIONS
        },
    )?;

    write_raw_fixture(
        output.join("invalid-duplicate-version-metadata.parquet"),
        &rows,
        RawWriterOptions {
            duplicate_version_metadata: true,
            ..CANONICAL_OPTIONS
        },
    )?;

    write_batch_fixture(
        output.join("invalid-null-content.parquet"),
        build_nullable_content_batch(&rows)?,
        CANONICAL_OPTIONS,
    )?;
    write_batch_fixture(
        output.join("invalid-wrong-id-length.parquet"),
        build_wrong_id_length_batch(&rows)?,
        CANONICAL_OPTIONS,
    )?;

    let mut truncated = fs::read(valid_path)?;
    truncated.truncate(truncated.len().saturating_sub(8));
    fs::write(output.join("invalid-truncated-footer.parquet"), truncated)?;

    println!("regenerated fixture corpus in {}", output.display());
    Ok(())
}

fn fixture_events() -> Result<Vec<Event>, Box<dyn std::error::Error>> {
    let keys = Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")?;
    let mut rng = StdRng::seed_from_u64(0x5041_5251_5545_5431);

    let first = EventBuilder::new(Kind::Metadata, "")
        .custom_created_at(Timestamp::from(0))
        .build(keys.public_key())
        .sign_with_ctx(SECP256K1, &mut rng, &keys)?;
    let whitespace = EventBuilder::new(Kind::TextNote, " \n\t")
        .tag(Tag::parse(["alt"])?)
        .custom_created_at(Timestamp::from(1))
        .build(keys.public_key())
        .sign_with_ctx(SECP256K1, &mut rng, &keys)?;
    let second = EventBuilder::new(Kind::Custom(u16::MAX), "  \nUnicode: 🦀\n{\"exact\":true}")
        .tags([
            Tag::parse(["alt"])?,
            Tag::parse(["d", ""])?,
            Tag::parse([
                "e",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "wss://relay.example.com",
                "root",
            ])?,
            Tag::parse(["client", "fixture", "1"])?,
        ])
        .custom_created_at(Timestamp::from(i64::MAX as u64 + 1))
        .build(keys.public_key())
        .sign_with_ctx(SECP256K1, &mut rng, &keys)?;

    Ok(vec![first, whitespace, second])
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

fn write_raw_fixture(
    path: PathBuf,
    rows: &[RawEvent],
    options: RawWriterOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    write_batch_fixture(path, build_record_batch(rows)?, options)
}

fn write_batch_fixture(
    path: PathBuf,
    batch: RecordBatch,
    options: RawWriterOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let properties = WriterProperties::builder()
        .set_writer_version(options.writer_version)
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_encoding(Encoding::PLAIN)
        .set_dictionary_enabled(true)
        .set_statistics_enabled(options.statistics)
        .set_max_row_group_row_count(None)
        .set_max_row_group_bytes(Some(ROW_GROUP_TARGET_BYTES))
        .set_key_value_metadata(options.version_metadata.then(|| {
            let version = KeyValue::new(
                ARCHIVE_VERSION_KEY.to_owned(),
                Some(ARCHIVE_VERSION.to_owned()),
            );
            if options.duplicate_version_metadata {
                vec![version.clone(), version]
            } else {
                vec![version]
            }
        }))
        .build();
    let writer_options = ArrowWriterOptions::new()
        .with_properties(properties)
        .with_schema_root(options.root_name.to_owned())
        .with_skip_arrow_metadata(true);
    let mut writer =
        ArrowWriter::try_new_with_options(fs::File::create(path)?, batch.schema(), writer_options)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

fn build_nullable_content_batch(
    rows: &[RawEvent],
) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let canonical = build_record_batch(rows)?;
    let mut fields: Vec<Field> = canonical
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect();
    fields[5] = Field::new("content", DataType::Utf8, true);
    let schema = Arc::new(Schema::new(fields));
    let mut content = StringBuilder::new();
    content.append_null();
    for row in &rows[1..] {
        content.append_value(&row.content);
    }
    let mut columns = canonical.columns().to_vec();
    columns[5] = Arc::new(content.finish());
    RecordBatch::try_new(schema, columns)
}

fn build_wrong_id_length_batch(rows: &[RawEvent]) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let canonical = build_record_batch(rows)?;
    let mut fields: Vec<Field> = canonical
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect();
    fields[0] = Field::new("id", DataType::FixedSizeBinary(31), false);
    let schema = Arc::new(Schema::new(fields));
    let mut ids = FixedSizeBinaryBuilder::with_capacity(rows.len(), 31);
    for row in rows {
        ids.append_value(&row.id[..31])?;
    }
    let mut columns = canonical.columns().to_vec();
    columns[0] = Arc::new(ids.finish());
    RecordBatch::try_new(schema, columns)
}

fn build_record_batch(rows: &[RawEvent]) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let mut ids = FixedSizeBinaryBuilder::with_capacity(rows.len(), 32);
    let mut pubkeys = FixedSizeBinaryBuilder::with_capacity(rows.len(), 32);
    let mut created_at = UInt64Builder::with_capacity(rows.len());
    let mut kinds = UInt16Builder::with_capacity(rows.len());
    let mut tags = tag_builder();
    let mut contents = StringBuilder::new();
    let mut signatures = FixedSizeBinaryBuilder::with_capacity(rows.len(), 64);

    for row in rows {
        ids.append_value(row.id)?;
        pubkeys.append_value(row.pubkey)?;
        created_at.append_value(row.created_at);
        kinds.append_value(row.kind);
        for tag in &row.tags {
            for value in tag {
                tags.values().values().append_value(value);
            }
            tags.values().append(true);
        }
        tags.append(true);
        contents.append_value(&row.content);
        signatures.append_value(row.sig)?;
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(ids.finish()),
        Arc::new(pubkeys.finish()),
        Arc::new(created_at.finish()),
        Arc::new(kinds.finish()),
        Arc::new(tags.finish()),
        Arc::new(contents.finish()),
        Arc::new(signatures.finish()),
    ];
    RecordBatch::try_new(canonical_arrow_schema(), columns)
}

fn tag_builder() -> ListBuilder<ListBuilder<StringBuilder>> {
    let string_element = Arc::new(Field::new("element", DataType::Utf8, false));
    let inner_tag = Arc::new(Field::new(
        "element",
        DataType::List(Arc::clone(&string_element)),
        false,
    ));
    let strings = StringBuilder::new();
    let inner = ListBuilder::new(strings).with_field(string_element);
    ListBuilder::new(inner).with_field(inner_tag)
}
