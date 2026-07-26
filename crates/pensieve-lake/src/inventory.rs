//! Durable SQLite work-unit journal and external object inventory.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{Error, Result};

/// Durable state of one conversion or live-seal work unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkState {
    /// Work identity is registered but no output is being written.
    Pending,
    /// Local artifacts are being generated.
    Writing,
    /// Every local artifact passed strict validation and was inventoried.
    Validated,
    /// One or more immutable objects are being published.
    Uploading,
    /// Every object is durably present in the configured object store.
    Uploaded,
    /// The object set is active in the inventory.
    Published,
    /// The caller recorded completion against the authoritative source.
    SourceCommitted,
    /// The last attempt failed and may be resumed.
    Failed,
}

impl WorkState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Writing => "writing",
            Self::Validated => "validated",
            Self::Uploading => "uploading",
            Self::Uploaded => "uploaded",
            Self::Published => "published",
            Self::SourceCommitted => "source_committed",
            Self::Failed => "failed",
        }
    }

    fn permits(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Pending | Self::Failed, Self::Writing)
                    | (Self::Writing, Self::Validated | Self::Failed)
                    | (Self::Validated, Self::Uploading | Self::Failed)
                    | (Self::Uploading, Self::Uploaded | Self::Failed)
                    | (Self::Uploaded, Self::Published | Self::Failed)
                    | (Self::Published, Self::SourceCommitted)
            )
    }
}

impl fmt::Display for WorkState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WorkState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "writing" => Ok(Self::Writing),
            "validated" => Ok(Self::Validated),
            "uploading" => Ok(Self::Uploading),
            "uploaded" => Ok(Self::Uploaded),
            "published" => Ok(Self::Published),
            "source_committed" => Ok(Self::SourceCommitted),
            "failed" => Ok(Self::Failed),
            _ => Err(Error::InvalidInventoryValue {
                field: "work_state",
                value: value.to_owned(),
            }),
        }
    }
}

/// Role of one immutable work-unit object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    /// Canonical V1 Parquet data.
    Parquet,
    /// Framed notepack quarantine data.
    Reject,
}

impl ObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Parquet => "parquet",
            Self::Reject => "reject",
        }
    }
}

impl FromStr for ObjectKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "parquet" => Ok(Self::Parquet),
            "reject" => Ok(Self::Reject),
            _ => Err(Error::InvalidInventoryValue {
                field: "object_kind",
                value: value.to_owned(),
            }),
        }
    }
}

/// Durable publication/activation state of one immutable object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectState {
    /// Local bytes exist but have not completed the validation gate.
    Staged,
    /// Local bytes and inventory metadata passed validation.
    Validated,
    /// The object store confirmed durable bytes with the expected checksum.
    Uploaded,
    /// Canonical data is active in the logical raw-lake snapshot.
    ActiveRaw,
    /// Invalid input is retained but excluded from canonical queries.
    Quarantined,
}

impl ObjectState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Validated => "validated",
            Self::Uploaded => "uploaded",
            Self::ActiveRaw => "active_raw",
            Self::Quarantined => "quarantined",
        }
    }
}

impl FromStr for ObjectState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "staged" => Ok(Self::Staged),
            "validated" => Ok(Self::Validated),
            "uploaded" => Ok(Self::Uploaded),
            "active_raw" => Ok(Self::ActiveRaw),
            "quarantined" => Ok(Self::Quarantined),
            _ => Err(Error::InvalidInventoryValue {
                field: "object_state",
                value: value.to_owned(),
            }),
        }
    }
}

/// One durable work-unit row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkUnitRecord {
    /// Content-derived work-unit identifier.
    pub id: String,
    /// Original source path for operator traceability.
    pub source_path: PathBuf,
    /// Source object bytes.
    pub source_bytes: u64,
    /// Lowercase SHA-256 of the exact source object bytes.
    pub source_sha256: String,
    /// Deterministic target used to partition canonical rows.
    pub target_uncompressed_bytes: u64,
    /// Maximum accepted source frame bytes.
    pub max_event_bytes: u64,
    /// Object namespace prefix used for this work.
    pub object_prefix: String,
    /// Writer implementation selected for this work.
    pub writer_version: String,
    /// Current state.
    pub state: WorkState,
    /// Input frame count once known.
    pub input_events: u64,
    /// Canonical output row count once known.
    pub output_rows: u64,
    /// Invalid input frame count once known.
    pub rejected_events: u64,
    /// Last failure text, cleared by the next successful transition.
    pub error: Option<String>,
}

/// Immutable identity and conversion settings used to register one work unit.
pub struct WorkUnitRegistration<'a> {
    /// Content-derived work-unit identifier.
    pub id: &'a str,
    /// Original source path.
    pub source_path: &'a Path,
    /// Exact source bytes.
    pub source_bytes: u64,
    /// Lowercase SHA-256 of the source bytes.
    pub source_sha256: &'a str,
    /// Target represented bytes per output.
    pub target_uncompressed_bytes: u64,
    /// Maximum accepted frame bytes.
    pub max_event_bytes: u64,
    /// Immutable object namespace prefix.
    pub object_prefix: &'a str,
    /// Writer implementation identity.
    pub writer_version: &'a str,
}

/// One immutable object row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectRecord {
    /// Immutable object key in the configured store.
    pub object_key: String,
    /// Owning work unit.
    pub work_unit_id: String,
    /// Deterministic zero-based part number.
    pub part_number: u32,
    /// Canonical or quarantine role.
    pub kind: ObjectKind,
    /// Publication/activation state.
    pub state: ObjectState,
    /// Local staged artifact.
    pub local_path: PathBuf,
    /// Exact object bytes.
    pub byte_size: u64,
    /// Lowercase SHA-256 of exact object bytes.
    pub sha256: String,
    /// Writer implementation identity; external operational metadata.
    pub writer_version: String,
    /// Canonical rows; zero for reject objects.
    pub row_count: u64,
    /// Unsigned event-time minimum encoded as decimal text.
    pub min_created_at: Option<u64>,
    /// Unsigned event-time maximum encoded as decimal text.
    pub max_created_at: Option<u64>,
}

/// SQLite-backed publication journal and active-object inventory.
pub struct Inventory {
    connection: Connection,
}

impl Inventory {
    /// Open or create an inventory database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    /// Create an isolated in-memory inventory.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS work_units (
                id TEXT PRIMARY KEY,
                source_path TEXT NOT NULL,
                source_bytes INTEGER NOT NULL,
                source_sha256 TEXT NOT NULL,
                target_uncompressed_bytes INTEGER NOT NULL,
                max_event_bytes INTEGER NOT NULL,
                object_prefix TEXT NOT NULL,
                writer_version TEXT NOT NULL,
                state TEXT NOT NULL,
                input_events INTEGER NOT NULL DEFAULT 0,
                output_rows INTEGER NOT NULL DEFAULT 0,
                rejected_events INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE TABLE IF NOT EXISTS objects (
                object_key TEXT PRIMARY KEY,
                work_unit_id TEXT NOT NULL REFERENCES work_units(id),
                part_number INTEGER NOT NULL,
                kind TEXT NOT NULL,
                state TEXT NOT NULL,
                local_path TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                writer_version TEXT NOT NULL,
                row_count INTEGER NOT NULL,
                min_created_at TEXT,
                max_created_at TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(work_unit_id, kind, part_number)
            );

            CREATE INDEX IF NOT EXISTS objects_active_kind
                ON objects(state, kind, object_key);
            "#,
        )?;
        ensure_column(
            &connection,
            "work_units",
            "max_event_bytes",
            "ALTER TABLE work_units ADD COLUMN max_event_bytes INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &connection,
            "work_units",
            "object_prefix",
            "ALTER TABLE work_units ADD COLUMN object_prefix TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &connection,
            "work_units",
            "writer_version",
            "ALTER TABLE work_units ADD COLUMN writer_version TEXT NOT NULL DEFAULT 'unknown'",
        )?;
        ensure_column(
            &connection,
            "objects",
            "writer_version",
            "ALTER TABLE objects ADD COLUMN writer_version TEXT NOT NULL DEFAULT 'unknown'",
        )?;
        Ok(Self { connection })
    }

    /// Register a work unit or verify that an existing identity is compatible.
    pub fn ensure_work_unit(
        &mut self,
        registration: &WorkUnitRegistration<'_>,
    ) -> Result<WorkUnitRecord> {
        let source_bytes_db = to_i64(registration.source_bytes, "source_bytes")?;
        let target_db = to_i64(
            registration.target_uncompressed_bytes,
            "target_uncompressed_bytes",
        )?;
        let max_event_db = to_i64(registration.max_event_bytes, "max_event_bytes")?;
        self.connection.execute(
            r#"
            INSERT OR IGNORE INTO work_units (
                id, source_path, source_bytes, source_sha256,
                target_uncompressed_bytes, max_event_bytes, object_prefix,
                writer_version, state
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')
            "#,
            params![
                registration.id,
                registration.source_path.to_string_lossy(),
                source_bytes_db,
                registration.source_sha256,
                target_db,
                max_event_db,
                registration.object_prefix,
                registration.writer_version
            ],
        )?;
        let record = self
            .work_unit(registration.id)?
            .expect("work unit was inserted or already existed");
        let conflict = if record.source_sha256 != registration.source_sha256 {
            Some("source checksum changed")
        } else if record.source_bytes != registration.source_bytes {
            Some("source byte size changed")
        } else if record.target_uncompressed_bytes != registration.target_uncompressed_bytes {
            Some("target uncompressed size changed")
        } else if record.max_event_bytes != registration.max_event_bytes {
            Some("maximum event frame size changed")
        } else if record.object_prefix != registration.object_prefix {
            Some("object prefix changed")
        } else if record.writer_version != registration.writer_version {
            Some("writer implementation changed")
        } else {
            None
        };
        if let Some(reason) = conflict {
            return Err(Error::WorkUnitConflict {
                work_unit_id: registration.id.to_owned(),
                reason: reason.to_owned(),
            });
        }
        Ok(record)
    }

    /// Load one work unit.
    pub fn work_unit(&self, id: &str) -> Result<Option<WorkUnitRecord>> {
        self.connection
            .query_row(
                r#"
                SELECT id, source_path, source_bytes, source_sha256,
                       target_uncompressed_bytes, max_event_bytes, object_prefix,
                       writer_version, state, input_events, output_rows,
                       rejected_events, error
                FROM work_units WHERE id = ?1
                "#,
                [id],
                work_unit_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Transition a work unit, enforcing the durable state machine.
    pub fn transition_work(
        &mut self,
        id: &str,
        next: WorkState,
        error: Option<&str>,
    ) -> Result<()> {
        let current = self.work_unit(id)?.ok_or_else(|| Error::WorkUnitConflict {
            work_unit_id: id.to_owned(),
            reason: "work unit is not registered".to_owned(),
        })?;
        if !current.state.permits(next) {
            return Err(Error::InvalidTransition {
                work_unit_id: id.to_owned(),
                from: current.state.to_string(),
                to: next.to_string(),
            });
        }
        self.connection.execute(
            r#"
            UPDATE work_units
            SET state = ?2, error = ?3, updated_at = unixepoch()
            WHERE id = ?1
            "#,
            params![id, next.as_str(), error],
        )?;
        Ok(())
    }

    /// Replace the validated object set and event counts for a writing work unit.
    pub fn record_validated_objects(
        &mut self,
        id: &str,
        input_events: u64,
        output_rows: u64,
        rejected_events: u64,
        objects: &[ObjectRecord],
    ) -> Result<()> {
        if objects
            .iter()
            .any(|object| object.work_unit_id != id || object.state != ObjectState::Validated)
        {
            return Err(Error::WorkUnitConflict {
                work_unit_id: id.to_owned(),
                reason: "validated object set has the wrong owner or state".to_string(),
            });
        }
        let transaction = self.connection.transaction()?;
        let current: String =
            transaction.query_row("SELECT state FROM work_units WHERE id = ?1", [id], |row| {
                row.get(0)
            })?;
        if current != WorkState::Writing.as_str() {
            return Err(Error::InvalidTransition {
                work_unit_id: id.to_owned(),
                from: current,
                to: WorkState::Validated.to_string(),
            });
        }
        transaction.execute("DELETE FROM objects WHERE work_unit_id = ?1", [id])?;
        for object in objects {
            insert_object(&transaction, object)?;
        }
        transaction.execute(
            r#"
            UPDATE work_units
            SET input_events = ?2, output_rows = ?3, rejected_events = ?4,
                state = 'validated', error = NULL, updated_at = unixepoch()
            WHERE id = ?1 AND state = 'writing'
            "#,
            params![
                id,
                to_i64(input_events, "input_events")?,
                to_i64(output_rows, "output_rows")?,
                to_i64(rejected_events, "rejected_events")?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// List all objects belonging to a work unit.
    pub fn objects_for_work(&self, id: &str) -> Result<Vec<ObjectRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT object_key, work_unit_id, part_number, kind, state,
                   local_path, byte_size, sha256, writer_version, row_count,
                   min_created_at, max_created_at
            FROM objects
            WHERE work_unit_id = ?1
            ORDER BY kind, part_number
            "#,
        )?;
        let records = statement
            .query_map([id], object_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Mark one immutable object as durably uploaded.
    pub fn mark_object_uploaded(&mut self, object_key: &str) -> Result<()> {
        self.connection.execute(
            r#"
            UPDATE objects
            SET state = 'uploaded', updated_at = unixepoch()
            WHERE object_key = ?1 AND state IN ('validated', 'uploaded')
            "#,
            [object_key],
        )?;
        Ok(())
    }

    /// Atomically activate every uploaded object and publish the work unit.
    pub fn activate_work_unit(&mut self, id: &str) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let not_uploaded: i64 = transaction.query_row(
            r#"
            SELECT count(*) FROM objects
            WHERE work_unit_id = ?1 AND state != 'uploaded'
            "#,
            [id],
            |row| row.get(0),
        )?;
        if not_uploaded != 0 {
            return Err(Error::InvalidTransition {
                work_unit_id: id.to_owned(),
                from: "uploaded objects incomplete".to_owned(),
                to: WorkState::Published.to_string(),
            });
        }
        transaction.execute(
            r#"
            UPDATE objects
            SET state = CASE kind
                WHEN 'parquet' THEN 'active_raw'
                ELSE 'quarantined'
            END,
            updated_at = unixepoch()
            WHERE work_unit_id = ?1
            "#,
            [id],
        )?;
        transaction.execute(
            r#"
            UPDATE work_units
            SET state = 'published', error = NULL, updated_at = unixepoch()
            WHERE id = ?1 AND state = 'uploaded'
            "#,
            [id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// List the active raw canonical snapshot in key order.
    pub fn active_raw_objects(&self) -> Result<Vec<ObjectRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT object_key, work_unit_id, part_number, kind, state,
                   local_path, byte_size, sha256, writer_version, row_count,
                   min_created_at, max_created_at
            FROM objects
            WHERE state = 'active_raw' AND kind = 'parquet'
            ORDER BY object_key
            "#,
        )?;
        let records = statement
            .query_map([], object_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }
}

fn insert_object(transaction: &Transaction<'_>, object: &ObjectRecord) -> Result<()> {
    transaction.execute(
        r#"
        INSERT INTO objects (
            object_key, work_unit_id, part_number, kind, state,
            local_path, byte_size, sha256, writer_version, row_count,
            min_created_at, max_created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
        params![
            object.object_key,
            object.work_unit_id,
            i64::from(object.part_number),
            object.kind.as_str(),
            object.state.as_str(),
            object.local_path.to_string_lossy(),
            to_i64(object.byte_size, "object.byte_size")?,
            object.sha256,
            object.writer_version,
            to_i64(object.row_count, "object.row_count")?,
            object.min_created_at.map(|value| value.to_string()),
            object.max_created_at.map(|value| value.to_string())
        ],
    )?;
    Ok(())
}

fn work_unit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkUnitRecord> {
    let state: String = row.get(8)?;
    Ok(WorkUnitRecord {
        id: row.get(0)?,
        source_path: PathBuf::from(row.get::<_, String>(1)?),
        source_bytes: from_i64(row.get(2)?, "source_bytes")?,
        source_sha256: row.get(3)?,
        target_uncompressed_bytes: from_i64(row.get(4)?, "target_uncompressed_bytes")?,
        max_event_bytes: from_i64(row.get(5)?, "max_event_bytes")?,
        object_prefix: row.get(6)?,
        writer_version: row.get(7)?,
        state: state.parse().map_err(to_sql_error)?,
        input_events: from_i64(row.get(9)?, "input_events")?,
        output_rows: from_i64(row.get(10)?, "output_rows")?,
        rejected_events: from_i64(row.get(11)?, "rejected_events")?,
        error: row.get(12)?,
    })
}

fn object_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ObjectRecord> {
    let part_number: i64 = row.get(2)?;
    let kind: String = row.get(3)?;
    let state: String = row.get(4)?;
    let min_created_at: Option<String> = row.get(10)?;
    let max_created_at: Option<String> = row.get(11)?;
    Ok(ObjectRecord {
        object_key: row.get(0)?,
        work_unit_id: row.get(1)?,
        part_number: u32::try_from(part_number).map_err(|_| {
            to_sql_error(Error::NumericOutOfRange {
                field: "part_number",
            })
        })?,
        kind: kind.parse().map_err(to_sql_error)?,
        state: state.parse().map_err(to_sql_error)?,
        local_path: PathBuf::from(row.get::<_, String>(5)?),
        byte_size: from_i64(row.get(6)?, "object.byte_size")?,
        sha256: row.get(7)?,
        writer_version: row.get(8)?,
        row_count: from_i64(row.get(9)?, "object.row_count")?,
        min_created_at: parse_u64_text(min_created_at, "min_created_at")?,
        max_created_at: parse_u64_text(max_created_at, "max_created_at")?,
    })
}

fn parse_u64_text(value: Option<String>, field: &'static str) -> rusqlite::Result<Option<u64>> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| to_sql_error(Error::InvalidInventoryValue { field, value }))
        })
        .transpose()
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    migration: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    if !columns.iter().any(|existing| existing == column) {
        connection.execute_batch(migration)?;
    }
    Ok(())
}

fn to_i64(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOutOfRange { field })
}

fn from_i64(value: i64, field: &'static str) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| to_sql_error(Error::NumericOutOfRange { field }))
}

fn to_sql_error(error: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
