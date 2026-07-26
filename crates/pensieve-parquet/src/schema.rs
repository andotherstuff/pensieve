//! Canonical V1 Arrow schema and format constants.

use std::sync::{Arc, OnceLock};

use arrow_schema::{DataType, Field, Schema, SchemaRef};

/// Required V1 footer metadata key.
pub const ARCHIVE_VERSION_KEY: &str = "nostr.event_archive.version";

/// Required V1 footer metadata value.
pub const ARCHIVE_VERSION: &str = "1";

/// Operational target for uncompressed data represented by a row group.
pub const ROW_GROUP_TARGET_BYTES: usize = 128 * 1024 * 1024;

/// Return the Arrow schema that maps to the exact canonical V1 Parquet schema.
pub fn canonical_arrow_schema() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();

    Arc::clone(SCHEMA.get_or_init(|| {
        let string_element = Arc::new(Field::new("element", DataType::Utf8, false));
        let inner_tag = Arc::new(Field::new("element", DataType::List(string_element), false));

        Arc::new(Schema::new(vec![
            Field::new("id", DataType::FixedSizeBinary(32), false),
            Field::new("pubkey", DataType::FixedSizeBinary(32), false),
            Field::new("created_at", DataType::UInt64, false),
            Field::new("kind", DataType::UInt16, false),
            Field::new("tags", DataType::List(inner_tag), false),
            Field::new("content", DataType::Utf8, false),
            Field::new("sig", DataType::FixedSizeBinary(64), false),
        ]))
    }))
}
