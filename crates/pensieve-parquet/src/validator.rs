//! Strict validation of canonical V1 Parquet archive files.

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;

use arrow_array::{Array, FixedSizeBinaryArray, ListArray, StringArray, UInt16Array, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, Encoding};
use parquet::column::page::Page;
use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use parquet::file::statistics::Statistics;
use parquet::schema::parser::parse_message_type;

use crate::{ARCHIVE_VERSION, ARCHIVE_VERSION_KEY, CanonicalEvent, Error, RawEvent, Result};

const CANONICAL_PARQUET_SCHEMA: &str = r#"
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
"#;

/// Counts and extrema established by a successful strict validation pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationReport {
    /// Number of validated event rows.
    pub rows: usize,
    /// Number of validated row groups.
    pub row_groups: usize,
    /// Smallest unsigned creation timestamp, if the file has rows.
    pub min_created_at: Option<u64>,
    /// Largest unsigned creation timestamp, if the file has rows.
    pub max_created_at: Option<u64>,
}

/// Strictly validate a local file against the complete canonical V1 profile.
pub fn validate_file(path: impl AsRef<Path>) -> Result<ValidationReport> {
    let path = path.as_ref();
    let file_reader = SerializedFileReader::new(File::open(path)?)?;
    validate_schema_and_metadata(&file_reader)?;
    validate_column_profiles_and_pages(&file_reader)?;

    let row_group_count = file_reader.num_row_groups();
    let mut rows = 0usize;
    let mut previous_key: Option<(u64, [u8; 32])> = None;
    let mut file_min = None;
    let mut file_max = None;

    for row_group_index in 0..row_group_count {
        let row_group_metadata = file_reader.metadata().row_group(row_group_index);
        let created_at_statistics = validate_created_at_statistics(row_group_metadata)?;
        let mut observed_min = None;
        let mut observed_max = None;
        let mut observed_rows = 0usize;

        let mut batches = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?
            .with_row_groups(vec![row_group_index])
            .build()?;
        for batch in &mut batches {
            let batch = batch?;
            validate_batch_nulls(&batch)?;

            let ids = downcast::<FixedSizeBinaryArray>(&batch, 0, "id")?;
            let pubkeys = downcast::<FixedSizeBinaryArray>(&batch, 1, "pubkey")?;
            let created_at = downcast::<UInt64Array>(&batch, 2, "created_at")?;
            let kinds = downcast::<UInt16Array>(&batch, 3, "kind")?;
            let tags = downcast::<ListArray>(&batch, 4, "tags")?;
            let contents = downcast::<StringArray>(&batch, 5, "content")?;
            let signatures = downcast::<FixedSizeBinaryArray>(&batch, 6, "sig")?;

            for row_index in 0..batch.num_rows() {
                let id = fixed_bytes::<32>(ids, row_index, "id")?;
                let timestamp = created_at.value(row_index);
                let key = (timestamp, id);
                if previous_key
                    .as_ref()
                    .is_some_and(|previous| previous >= &key)
                {
                    return Err(Error::InvalidFile(format!(
                        "row {rows} is not strictly ordered by unsigned (created_at, id)"
                    )));
                }

                let raw = RawEvent {
                    id,
                    pubkey: fixed_bytes::<32>(pubkeys, row_index, "pubkey")?,
                    created_at: timestamp,
                    kind: kinds.value(row_index),
                    tags: tags_at(tags, row_index, rows)?,
                    content: contents.value(row_index).to_owned(),
                    sig: fixed_bytes::<64>(signatures, row_index, "sig")?,
                };
                CanonicalEvent::from_raw(raw)?;

                previous_key = Some(key);
                observed_min =
                    Some(observed_min.map_or(timestamp, |value: u64| value.min(timestamp)));
                observed_max =
                    Some(observed_max.map_or(timestamp, |value: u64| value.max(timestamp)));
                file_min = Some(file_min.map_or(timestamp, |value: u64| value.min(timestamp)));
                file_max = Some(file_max.map_or(timestamp, |value: u64| value.max(timestamp)));
                observed_rows += 1;
                rows += 1;
            }
        }

        if i64::try_from(observed_rows).ok() != Some(row_group_metadata.num_rows()) {
            return Err(Error::InvalidFile(format!(
                "row group {row_group_index} footer row count does not match decoded rows"
            )));
        }
        if (observed_min, observed_max) != created_at_statistics {
            return Err(Error::InvalidFile(format!(
                "row group {row_group_index} created_at statistics do not match decoded rows"
            )));
        }
    }

    if i64::try_from(rows).ok() != Some(file_reader.metadata().file_metadata().num_rows()) {
        return Err(Error::InvalidFile(
            "footer row count does not match decoded rows".to_owned(),
        ));
    }

    Ok(ValidationReport {
        rows,
        row_groups: row_group_count,
        min_created_at: file_min,
        max_created_at: file_max,
    })
}

/// Strictly validate a canonical V1 file and return every decoded event row.
///
/// This is intended for bounded migration comparisons and recovery audits.
/// Callers should not use it for unbounded query execution because all rows
/// are retained in memory.
pub fn read_validated_file(path: impl AsRef<Path>) -> Result<Vec<CanonicalEvent>> {
    let path = path.as_ref();
    validate_file(path)?;
    let mut rows = Vec::new();
    let batches = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?.build()?;
    for batch in batches {
        let batch = batch?;
        let ids = downcast::<FixedSizeBinaryArray>(&batch, 0, "id")?;
        let pubkeys = downcast::<FixedSizeBinaryArray>(&batch, 1, "pubkey")?;
        let created_at = downcast::<UInt64Array>(&batch, 2, "created_at")?;
        let kinds = downcast::<UInt16Array>(&batch, 3, "kind")?;
        let tags = downcast::<ListArray>(&batch, 4, "tags")?;
        let contents = downcast::<StringArray>(&batch, 5, "content")?;
        let signatures = downcast::<FixedSizeBinaryArray>(&batch, 6, "sig")?;
        for row_index in 0..batch.num_rows() {
            rows.push(CanonicalEvent::from_raw(RawEvent {
                id: fixed_bytes::<32>(ids, row_index, "id")?,
                pubkey: fixed_bytes::<32>(pubkeys, row_index, "pubkey")?,
                created_at: created_at.value(row_index),
                kind: kinds.value(row_index),
                tags: tags_at(tags, row_index, rows.len())?,
                content: contents.value(row_index).to_owned(),
                sig: fixed_bytes::<64>(signatures, row_index, "sig")?,
            })?);
        }
    }
    Ok(rows)
}

fn validate_schema_and_metadata(file_reader: &SerializedFileReader<File>) -> Result<()> {
    let expected_schema = parse_message_type(CANONICAL_PARQUET_SCHEMA)?;
    let actual_schema = file_reader
        .metadata()
        .file_metadata()
        .schema_descr()
        .root_schema();
    if actual_schema != &expected_schema {
        return Err(Error::InvalidFile(
            "physical/logical schema does not exactly match canonical V1".to_owned(),
        ));
    }

    let mut reserved_keys = HashSet::new();
    let mut version_value = None;
    for item in file_reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .into_iter()
        .flatten()
    {
        if item.key.starts_with("nostr.event_archive.") {
            if !reserved_keys.insert(item.key.as_str()) {
                return Err(Error::InvalidFile(format!(
                    "duplicate reserved footer metadata key {}",
                    item.key
                )));
            }
            if item.key == ARCHIVE_VERSION_KEY {
                version_value = item.value.as_deref();
            }
        }
    }

    if version_value != Some(ARCHIVE_VERSION) {
        return Err(Error::InvalidFile(format!(
            "required footer metadata {ARCHIVE_VERSION_KEY}={ARCHIVE_VERSION:?} is missing or invalid"
        )));
    }
    Ok(())
}

fn validate_column_profiles_and_pages(file_reader: &SerializedFileReader<File>) -> Result<()> {
    for row_group_index in 0..file_reader.num_row_groups() {
        let row_group_metadata = file_reader.metadata().row_group(row_group_index);
        for (column_index, column) in row_group_metadata.columns().iter().enumerate() {
            if !matches!(column.compression(), Compression::ZSTD(_)) {
                return Err(Error::InvalidFile(format!(
                    "row group {row_group_index} column {column_index} is not Zstandard-compressed"
                )));
            }
            for encoding in column.encodings() {
                if !is_allowed_encoding(encoding) {
                    return Err(Error::InvalidFile(format!(
                        "row group {row_group_index} column {column_index} footer declares forbidden encoding {encoding}"
                    )));
                }
            }
        }

        let row_group = file_reader.get_row_group(row_group_index)?;
        for column_index in 0..row_group.num_columns() {
            let pages = row_group.get_column_page_reader(column_index)?;
            for page in pages {
                validate_page_profile(&page?, row_group_index, column_index)?;
            }
        }
    }
    Ok(())
}

fn validate_page_profile(page: &Page, row_group_index: usize, column_index: usize) -> Result<()> {
    if matches!(page, Page::DataPageV2 { .. }) {
        return Err(Error::InvalidFile(format!(
            "row group {row_group_index} column {column_index} contains a Data Page V2"
        )));
    }
    let encoding = page.encoding();
    if !is_allowed_encoding(encoding) {
        return Err(Error::InvalidFile(format!(
            "row group {row_group_index} column {column_index} page uses forbidden encoding {encoding}"
        )));
    }
    Ok(())
}

fn is_allowed_encoding(encoding: Encoding) -> bool {
    matches!(
        encoding,
        Encoding::PLAIN | Encoding::RLE | Encoding::RLE_DICTIONARY
    )
}

fn validate_created_at_statistics(
    row_group: &parquet::file::metadata::RowGroupMetaData,
) -> Result<(Option<u64>, Option<u64>)> {
    let statistics = row_group
        .column(2)
        .statistics()
        .ok_or_else(|| Error::InvalidFile("created_at statistics are missing".to_owned()))?;
    if statistics.null_count_opt() != Some(0) {
        return Err(Error::InvalidFile(
            "created_at null_count must be present and zero".to_owned(),
        ));
    }
    if !statistics.min_is_exact() || !statistics.max_is_exact() {
        return Err(Error::InvalidFile(
            "created_at min/max statistics must be exact".to_owned(),
        ));
    }

    let Statistics::Int64(values) = statistics else {
        return Err(Error::InvalidFile(
            "created_at statistics have the wrong physical type".to_owned(),
        ));
    };
    Ok((
        values.min_opt().copied().map(|value| value as u64),
        values.max_opt().copied().map(|value| value as u64),
    ))
}

fn validate_batch_nulls(batch: &arrow_array::RecordBatch) -> Result<()> {
    for (column_index, column) in batch.columns().iter().enumerate() {
        if column.null_count() != 0 {
            return Err(Error::InvalidFile(format!(
                "column {column_index} contains a null value"
            )));
        }
    }
    Ok(())
}

fn downcast<'a, T: 'static>(
    batch: &'a arrow_array::RecordBatch,
    column_index: usize,
    name: &str,
) -> Result<&'a T> {
    batch
        .column(column_index)
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| Error::InvalidFile(format!("Arrow column {name} has the wrong type")))
}

fn fixed_bytes<const N: usize>(
    array: &FixedSizeBinaryArray,
    row_index: usize,
    name: &str,
) -> Result<[u8; N]> {
    array.value(row_index).try_into().map_err(|_| {
        Error::InvalidFile(format!(
            "row {row_index} column {name} does not contain exactly {N} bytes"
        ))
    })
}

fn tags_at(array: &ListArray, row_index: usize, global_row: usize) -> Result<Vec<Vec<String>>> {
    let tags = array.value(row_index);
    let tags = tags.as_any().downcast_ref::<ListArray>().ok_or_else(|| {
        Error::InvalidFile(format!("row {global_row} tags has the wrong nested type"))
    })?;
    if tags.null_count() != 0 {
        return Err(Error::InvalidFile(format!(
            "row {global_row} contains a null tag"
        )));
    }

    let mut result = Vec::with_capacity(tags.len());
    for tag_index in 0..tags.len() {
        let elements = tags.value(tag_index);
        let elements = elements
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                Error::InvalidFile(format!(
                    "row {global_row} tag {tag_index} has the wrong element type"
                ))
            })?;
        if elements.is_empty() {
            return Err(Error::EmptyTag {
                id: format!("at row {global_row}"),
                tag_index,
            });
        }
        if elements.null_count() != 0 {
            return Err(Error::InvalidFile(format!(
                "row {global_row} tag {tag_index} contains a null string"
            )));
        }
        result.push(
            elements
                .iter()
                .map(|value| value.unwrap().to_owned())
                .collect(),
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn page_profile_rejects_forbidden_encoding_from_page_header() {
        let page = Page::DataPage {
            buf: Bytes::new(),
            num_values: 0,
            encoding: Encoding::DELTA_BINARY_PACKED,
            def_level_encoding: Encoding::RLE,
            rep_level_encoding: Encoding::RLE,
            statistics: None,
        };

        let error = validate_page_profile(&page, 2, 3).expect_err("encoding must be rejected");
        assert!(
            error
                .to_string()
                .contains("row group 2 column 3 page uses forbidden encoding")
        );
    }
}
