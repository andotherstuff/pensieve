//! Deterministic batch preparation and canonical V1 file writing.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Range;
use std::sync::Arc;

use arrow_array::builder::{
    FixedSizeBinaryBuilder, ListBuilder, StringBuilder, UInt16Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field};
use nostr::Event;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_writer::ArrowWriterOptions;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};

use crate::schema::{
    ARCHIVE_VERSION, ARCHIVE_VERSION_KEY, ROW_GROUP_TARGET_BYTES, canonical_arrow_schema,
};
use crate::{CanonicalEvent, Error, Result};

/// Counts describing a completed canonical archive file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteSummary {
    /// Number of validated input events, including duplicates.
    pub input_events: usize,
    /// Number of canonical rows written after ID deduplication.
    pub output_rows: usize,
    /// Number of duplicate input events removed.
    pub duplicate_events: usize,
    /// Number of row groups in the completed file.
    pub row_groups: usize,
}

/// Partition sorted canonical rows into deterministic target-sized file ranges.
///
/// A row larger than the target occupies a part by itself. Boundaries are based
/// on the stable uncompressed-size estimate exposed by [`CanonicalEvent`], not
/// on author-controlled timestamps or final compressed byte sizes.
pub fn partition_prepared_rows(
    rows: &[CanonicalEvent],
    target_uncompressed_bytes: usize,
) -> Result<Vec<Range<usize>>> {
    if target_uncompressed_bytes == 0 {
        return Err(Error::InvalidTargetSize);
    }
    validate_prepared_rows(rows)?;

    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut estimated_bytes = 0usize;
    for (index, row) in rows.iter().enumerate() {
        let row_bytes = row.estimated_uncompressed_bytes();
        if index > start && estimated_bytes.saturating_add(row_bytes) > target_uncompressed_bytes {
            ranges.push(start..index);
            start = index;
            estimated_bytes = 0;
        }
        estimated_bytes = estimated_bytes.saturating_add(row_bytes);
    }
    ranges.push(start..rows.len());
    Ok(ranges)
}

/// Validate, deduplicate, and sort events for one canonical file.
///
/// Duplicate IDs are collapsed within the batch. When valid variants contain
/// different signatures, the lexicographically smallest raw signature wins.
pub fn prepare_events<'a>(
    events: impl IntoIterator<Item = &'a Event>,
) -> Result<Vec<CanonicalEvent>> {
    prepare_events_with_count(events).map(|(rows, _)| rows)
}

/// Deduplicate and sort rows that have already passed canonical validation.
pub fn prepare_canonical_events(
    rows: impl IntoIterator<Item = CanonicalEvent>,
) -> Vec<CanonicalEvent> {
    let mut by_id = BTreeMap::<[u8; 32], CanonicalEvent>::new();
    for candidate in rows {
        match by_id.entry(candidate.id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if candidate.sig < entry.get().sig {
                    entry.insert(candidate);
                }
            }
        }
    }

    let mut rows: Vec<_> = by_id.into_values().collect();
    rows.sort_unstable_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    rows
}

fn prepare_events_with_count<'a>(
    events: impl IntoIterator<Item = &'a Event>,
) -> Result<(Vec<CanonicalEvent>, usize)> {
    let mut by_id = BTreeMap::<[u8; 32], CanonicalEvent>::new();
    let mut input_events = 0;

    for event in events {
        input_events += 1;
        let candidate = CanonicalEvent::from_event(event)?;
        match by_id.entry(candidate.id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if candidate.sig < entry.get().sig {
                    entry.insert(candidate);
                }
            }
        }
    }

    let mut rows: Vec<_> = by_id.into_values().collect();
    rows.sort_unstable_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok((rows, input_events))
}

/// Validate and write typed Nostr events as one canonical V1 Parquet file.
pub fn write_events<'a, W>(
    sink: W,
    events: impl IntoIterator<Item = &'a Event>,
) -> Result<WriteSummary>
where
    W: Write + Send,
{
    let (rows, input_events) = prepare_events_with_count(events)?;
    write_prepared_with_input_count(sink, &rows, input_events)
}

/// Deduplicate, sort, and write rows that were validated at a typed decoder boundary.
pub fn write_canonical_events<W>(
    sink: W,
    rows: impl IntoIterator<Item = CanonicalEvent>,
) -> Result<WriteSummary>
where
    W: Write + Send,
{
    let rows: Vec<_> = rows.into_iter().collect();
    let input_events = rows.len();
    let rows = prepare_canonical_events(rows);
    write_prepared_with_input_count(sink, &rows, input_events)
}

/// Write already validated, deduplicated, and sorted canonical rows.
///
/// This entry point rechecks ordering and uniqueness before emitting bytes.
pub fn write_prepared<W>(sink: W, rows: &[CanonicalEvent]) -> Result<WriteSummary>
where
    W: Write + Send,
{
    validate_prepared_rows(rows)?;
    write_prepared_with_input_count(sink, rows, rows.len())
}

fn write_prepared_with_input_count<W>(
    sink: W,
    rows: &[CanonicalEvent],
    input_events: usize,
) -> Result<WriteSummary>
where
    W: Write + Send,
{
    if rows.is_empty() {
        return Err(Error::EmptyBatch);
    }

    let options = ArrowWriterOptions::new()
        .with_properties(writer_properties())
        .with_schema_root("nostr_event_archive_v1".to_string())
        .with_skip_arrow_metadata(true);
    let mut writer = ArrowWriter::try_new_with_options(sink, canonical_arrow_schema(), options)?;
    for range in partition_prepared_rows(rows, ROW_GROUP_TARGET_BYTES)? {
        let batch = build_record_batch(&rows[range])?;
        writer.write(&batch)?;
        writer.flush()?;
    }
    let metadata = writer.close()?;

    Ok(WriteSummary {
        input_events,
        output_rows: rows.len(),
        duplicate_events: input_events - rows.len(),
        row_groups: metadata.num_row_groups(),
    })
}

fn validate_prepared_rows(rows: &[CanonicalEvent]) -> Result<()> {
    if rows.is_empty() {
        return Err(Error::EmptyBatch);
    }

    for pair in rows.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        let correctly_ordered =
            (left.created_at, left.id.as_slice()) < (right.created_at, right.id.as_slice());
        if !correctly_ordered {
            return Err(Error::UnsortedOrDuplicateRows);
        }
    }
    Ok(())
}

fn build_record_batch(rows: &[CanonicalEvent]) -> Result<RecordBatch> {
    let row_count = rows.len();
    let mut ids = FixedSizeBinaryBuilder::with_capacity(row_count, 32);
    let mut pubkeys = FixedSizeBinaryBuilder::with_capacity(row_count, 32);
    let mut created_at = UInt64Builder::with_capacity(row_count);
    let mut kinds = UInt16Builder::with_capacity(row_count);
    let mut tags = tag_builder();
    let mut content = StringBuilder::with_capacity(row_count, 0);
    let mut signatures = FixedSizeBinaryBuilder::with_capacity(row_count, 64);

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

        content.append_value(&row.content);
        signatures.append_value(row.sig)?;
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(ids.finish()),
        Arc::new(pubkeys.finish()),
        Arc::new(created_at.finish()),
        Arc::new(kinds.finish()),
        Arc::new(tags.finish()),
        Arc::new(content.finish()),
        Arc::new(signatures.finish()),
    ];

    Ok(RecordBatch::try_new(canonical_arrow_schema(), columns)?)
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

fn writer_properties() -> WriterProperties {
    WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_1_0)
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_encoding(Encoding::PLAIN)
        .set_dictionary_enabled(true)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .set_max_row_group_row_count(None)
        .set_max_row_group_bytes(Some(ROW_GROUP_TARGET_BYTES))
        .set_key_value_metadata(Some(vec![KeyValue::new(
            ARCHIVE_VERSION_KEY.to_string(),
            Some(ARCHIVE_VERSION.to_string()),
        )]))
        .build()
}
