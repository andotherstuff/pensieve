//! Read-only access to the ingester's relay-stats SQLite database.
//!
//! The ingester keeps `relay-stats.db` (`RELAY_DB_PATH`) open in WAL mode for
//! its whole lifetime and commits to it continuously. The API reads it as an
//! ordinary WAL reader and never writes it:
//!
//! - **One connection per request.** [`RelayDb::read`] opens a fresh
//!   connection on a blocking thread, runs the closure and drops it, so a
//!   request always starts from the latest committed transaction and an
//!   operator replacing the file (e.g. `relay-cleanup`) is picked up without a
//!   restart. At most [`MAX_CONCURRENT_READS`] run at once; more fail fast with
//!   `503 analytics_busy`.
//! - **Read-only.** Connections are opened with `SQLITE_OPEN_READONLY` (the
//!   flag behind the `mode=ro` URI parameter) and `PRAGMA query_only`.
//! - **`busy_timeout`.** A WAL reader rarely sees `SQLITE_BUSY` (WAL-index
//!   recovery, a concurrent checkpoint restart); it waits up to
//!   [`BUSY_TIMEOUT`], then the request fails with `503 analytics_busy`.
//!
//! # `-wal` and `-shm` files
//!
//! In production the API unit runs with `ProtectSystem=strict` and no
//! `ReadWritePaths`, so `/data` is read-only for it, and it can never create or
//! write the database's `-wal` and `-shm` files. Each read picks one of two
//! [`Access`] modes:
//!
//! - **[`Access::Live`]**, when both files exist (the ingester is running, or
//!   the database is not in WAL mode): a normal read-only WAL reader. If it
//!   can write `-shm` it updates its read marks there, as every WAL reader
//!   does. If it cannot (the production case) SQLite maps `-shm` read-only:
//!   while the ingester holds it, the reader uses the writer's WAL index and
//!   read marks at normal speed. Only when no writer holds `-shm` (after a
//!   crash, or a stop that skipped the final checkpoint) does SQLite rebuild
//!   the WAL index in private memory, which is slower but still consistent.
//!   `tests/relay_db.rs` covers the read-only `-shm` path with the writer in
//!   a separate process; it was also checked on a read-only (`EROFS`) bind
//!   mount, as production has.
//! - **[`Access::Snapshot`]**, when a WAL database has neither file: no
//!   connection has it open, because the last one to close checkpointed the
//!   WAL into the main file and deleted both. The main file is then complete,
//!   so it is read with `immutable=1`, which never looks for (or creates) the
//!   WAL. A plain read-only open would instead try to create `-wal` and fail
//!   on a read-only directory (`SQLITE_READONLY_DIRECTORY`, or
//!   `SQLITE_CANTOPEN` on a read-only file system).
//!
//! The choice is a check of the file system just before opening, so it can
//! race with the ingester:
//!
//! - The ingester starts while a snapshot read runs. It only appends to the
//!   new WAL; the main file changes at the first checkpoint, about 1000 pages
//!   (4 MiB) of commits later, long after the read has finished.
//! - The ingester stops between the check and the open. The live open then
//!   finds no `-wal`; [`RelayDb::open`] re-checks and falls back to a
//!   snapshot when SQLite reports that it could not create the file. If the
//!   API *can* write the directory, SQLite creates `-wal` and `-shm` instead,
//!   owned by the API's user with the database file's mode. Where the API
//!   runs as a different user from the database's owner, that can leave
//!   files the ingester cannot write until someone removes them. Production
//!   is not exposed: `/data` is read-only for the API, and both services run
//!   as `pensieve`, which owns the database.
//!
//! Any remaining "could not open, create or recover" error maps to
//! `503 metric_unavailable`, not `500`, and so does `SQLITE_CORRUPT` in a
//! snapshot read, which can overlap an ingester restart.
//!
//! # Journal mode probe
//!
//! Whether the database is in WAL mode is read from its header once, in
//! [`RelayDb::new`], at startup before this process opens any SQLite
//! connection to it. Reading it later through a second file descriptor would
//! release the POSIX locks held by live connections when that descriptor
//! closes (SQLite's "How To Corrupt", section 2.2).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags};
use tokio::sync::Semaphore;

use crate::error::ApiError;

/// How long a reader waits on `SQLITE_BUSY` before failing the request.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// Relay reads allowed to run at once; more get `503 analytics_busy`.
pub const MAX_CONCURRENT_READS: usize = 16;

/// How a connection reads the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read-only reader that follows the WAL (writer attached, or not a WAL
    /// database).
    Live,
    /// `immutable=1` read of a WAL database that no connection has open.
    Snapshot,
}

/// Map a SQLite error to an API error.
///
/// - `SQLITE_BUSY`/`SQLITE_LOCKED` (after [`BUSY_TIMEOUT`]) become
///   `503 analytics_busy`;
/// - `SQLITE_CANTOPEN` and every `SQLITE_READONLY*` code become
///   `503 metric_unavailable`: the API never writes, so a read-only error
///   means SQLite could not open, create or recover one of the database's
///   files (`_DIRECTORY`, `_CANTINIT`, or `_RECOVERY` on a torn WAL-index
///   header seen through a read-only `-shm`);
/// - everything else is a `500`, keeping the [`rusqlite::Error`] inside the
///   [`ApiError::Internal`] so the snapshot rule of [`sqlite_error_in`] can
///   still inspect it.
pub fn sqlite_error(error: rusqlite::Error) -> ApiError {
    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            tracing::warn!(error = %error, "relay database busy");
            ApiError::Overloaded
        }
        _ if is_unavailable(&error) => {
            tracing::warn!(error = %error, "relay database unavailable");
            ApiError::MetricUnavailable
        }
        _ => ApiError::Internal(anyhow::Error::new(error)),
    }
}

/// [`sqlite_error`] for a read in `access` mode. A [`Access::Snapshot`] read
/// can overlap an ingester restart (see the module docs), so
/// `SQLITE_CORRUPT` there is a `503 metric_unavailable`, not a `500`.
pub fn sqlite_error_in(access: Access, error: rusqlite::Error) -> ApiError {
    remap_in(access, sqlite_error(error))
}

/// Apply the [`Access::Snapshot`] rule of [`sqlite_error_in`] to an error
/// that a read closure already mapped with [`sqlite_error`].
fn remap_in(access: Access, error: ApiError) -> ApiError {
    match error {
        ApiError::Internal(inner)
            if access == Access::Snapshot
                && inner
                    .downcast_ref::<rusqlite::Error>()
                    .and_then(rusqlite::Error::sqlite_error_code)
                    == Some(ErrorCode::DatabaseCorrupt) =>
        {
            tracing::warn!(error = %inner, "relay database snapshot read overlapped a write");
            ApiError::MetricUnavailable
        }
        other => other,
    }
}

/// Whether SQLite could not open, create or recover one of the database's
/// files (`SQLITE_CANTOPEN`, any `SQLITE_READONLY*`).
fn is_unavailable(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::CannotOpen | ErrorCode::ReadOnly)
    )
}

/// Handle to the ingester's relay-stats database. Cheap to clone; holds no
/// connection (see the module docs).
#[derive(Clone, Debug)]
pub struct RelayDb {
    path: Arc<PathBuf>,
    wal: bool,
    permits: Arc<Semaphore>,
}

impl RelayDb {
    /// A handle for the database at `path`.
    ///
    /// Reads the journal mode from the database header (see the module docs),
    /// so call it before this process opens any connection to `path`. An
    /// unreadable or empty file is assumed to be WAL, the ingester's mode.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let wal = match header_says_wal(&path) {
            Ok(Some(wal)) => wal,
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "cannot read relay database header; assuming WAL");
                true
            }
        };
        Self {
            path: Arc::new(path),
            wal,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_READS)),
        }
    }

    /// The database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the header said WAL mode at startup.
    pub fn is_wal(&self) -> bool {
        self.wal
    }

    /// Open a new read-only connection and check that it can read.
    ///
    /// A [`Access::Live`] connection sees every transaction committed before
    /// each of its read transactions starts, including ones committed after
    /// it opened.
    pub fn open(&self) -> rusqlite::Result<(Connection, Access)> {
        let (access, result) = self.open_attempt();
        result.map(|conn| (conn, access))
    }

    /// Open a connection. The mode is returned with a failure too, for
    /// [`sqlite_error_in`].
    fn open_attempt(&self) -> (Access, rusqlite::Result<Connection>) {
        if self.wal && !self.sidecars_exist() {
            return (Access::Snapshot, self.open_snapshot());
        }
        let conn = match self.open_live() {
            Ok(conn) => conn,
            Err(error) => return (Access::Live, Err(error)),
        };
        // The open is lazy: the first read is what opens (or tries to create)
        // the WAL.
        match check_readable(&conn) {
            Ok(()) => (Access::Live, Ok(conn)),
            // The ingester closed between the check and the open.
            Err(error) if self.wal && is_unavailable(&error) && !self.sidecars_exist() => {
                drop(conn);
                (Access::Snapshot, self.open_snapshot())
            }
            Err(error) => (Access::Live, Err(error)),
        }
    }

    /// Run `f` against a fresh read-only connection on a blocking thread.
    pub async fn read<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::Overloaded)?;
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (access, conn) = db.open_attempt();
            let conn = conn.map_err(|error| sqlite_error_in(access, error))?;
            f(&conn).map_err(|error| remap_in(access, error))
        })
        .await
        .map_err(|error| ApiError::Internal(error.into()))?
    }

    fn sidecars_exist(&self) -> bool {
        sidecar(&self.path, "-wal").exists() && sidecar(&self.path, "-shm").exists()
    }

    fn open_live(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(
            self.path.as_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(conn)
    }

    fn open_snapshot(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(
            format!("{}?mode=ro&immutable=1", file_uri(&self.path)),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "query_only", true)?;
        check_readable(&conn)?;
        Ok(conn)
    }
}

/// One cheap read, so that a connection that cannot read fails here.
fn check_readable(conn: &Connection) -> rusqlite::Result<()> {
    conn.query_row("PRAGMA schema_version", [], |_| Ok(()))
}

/// `<path>-wal` / `<path>-shm`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `file:` URI for `path`, escaping the characters SQLite's URI parser treats
/// specially.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for c in path.to_string_lossy().chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri
}

/// The journal mode in the database header: `Some(true)` for WAL (file format
/// version 2 at byte offsets 18 and 19), `None` for a file too short to have a
/// header.
fn header_says_wal(path: &Path) -> std::io::Result<Option<bool>> {
    let mut header = [0u8; 20];
    let mut file = File::open(path)?;
    let mut read = 0;
    while read < header.len() {
        match file.read(&mut header[read..])? {
            0 => return Ok(None),
            n => read += n,
        }
    }
    Ok(Some(header[18] == 2 || header[19] == 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None)
    }

    #[test]
    fn busy_and_locked_are_overloaded() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
            assert!(matches!(sqlite_error(failure(code)), ApiError::Overloaded));
        }
    }

    #[test]
    fn open_and_read_only_failures_are_unavailable() {
        use rusqlite::ffi;
        for code in [
            ffi::SQLITE_CANTOPEN,
            ffi::SQLITE_READONLY,
            ffi::SQLITE_READONLY_RECOVERY,
            ffi::SQLITE_READONLY_CANTINIT,
            ffi::SQLITE_READONLY_DIRECTORY,
            ffi::SQLITE_READONLY_ROLLBACK,
            ffi::SQLITE_READONLY_DBMOVED,
        ] {
            for access in [Access::Live, Access::Snapshot] {
                assert!(
                    matches!(
                        sqlite_error_in(access, failure(code)),
                        ApiError::MetricUnavailable
                    ),
                    "code {code} in {access:?}"
                );
            }
        }
    }

    #[test]
    fn corrupt_is_unavailable_only_in_a_snapshot() {
        let corrupt = rusqlite::ffi::SQLITE_CORRUPT;
        assert!(matches!(
            sqlite_error_in(Access::Live, failure(corrupt)),
            ApiError::Internal(_)
        ));
        assert!(matches!(
            sqlite_error_in(Access::Snapshot, failure(corrupt)),
            ApiError::MetricUnavailable
        ));
        // Errors a read closure already mapped with `sqlite_error`.
        assert!(matches!(
            remap_in(Access::Snapshot, sqlite_error(failure(corrupt))),
            ApiError::MetricUnavailable
        ));
        assert!(matches!(
            remap_in(Access::Live, sqlite_error(failure(corrupt))),
            ApiError::Internal(_)
        ));
        // Other internal errors stay 500 in a snapshot too.
        assert!(matches!(
            sqlite_error_in(Access::Snapshot, failure(rusqlite::ffi::SQLITE_ERROR)),
            ApiError::Internal(_)
        ));
        assert!(matches!(
            remap_in(
                Access::Snapshot,
                ApiError::Internal(anyhow::anyhow!("other"))
            ),
            ApiError::Internal(_)
        ));
    }

    #[test]
    fn sidecar_paths_append_the_suffix() {
        assert_eq!(
            sidecar(Path::new("/data/relay-stats.db"), "-wal"),
            PathBuf::from("/data/relay-stats.db-wal")
        );
    }

    #[test]
    fn file_uri_escapes_query_and_fragment() {
        assert_eq!(
            file_uri(Path::new("/data/a?b#c%d.db")),
            "file:/data/a%3fb%23c%25d.db"
        );
    }

    #[test]
    fn header_detects_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let rollback = dir.path().join("rollback.db");
        Connection::open(&rollback)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        assert_eq!(header_says_wal(&rollback).unwrap(), Some(false));

        let wal = dir.path().join("wal.db");
        Connection::open(&wal)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x)")
            .unwrap();
        assert_eq!(header_says_wal(&wal).unwrap(), Some(true));

        let empty = dir.path().join("empty.db");
        File::create(&empty).unwrap();
        assert_eq!(header_says_wal(&empty).unwrap(), None);
    }

    #[tokio::test]
    async fn saturated_reads_fail_fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        let db = RelayDb::new(&path);
        let held = db
            .permits
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_READS as u32)
            .await
            .unwrap();
        let result = db.read(|_| Ok(())).await;
        assert!(matches!(result, Err(ApiError::Overloaded)));
        drop(held);
        db.read(|_| Ok(())).await.unwrap();
    }
}
