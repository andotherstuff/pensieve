//! Read-only access to the ingester's relay-stats SQLite database.
//!
//! The ingester keeps `relay-stats.db` (`RELAY_DB_PATH`) open in WAL mode for
//! its whole lifetime and commits to it continuously. The API reads it as an
//! ordinary WAL reader:
//!
//! - **One connection per request.** [`RelayDb::read`] opens a fresh
//!   connection on a blocking thread, runs the closure and drops it, so a
//!   request always starts from the latest committed transaction and an
//!   operator replacing the file (e.g. `relay-cleanup`) is picked up without a
//!   restart. Relay endpoints are cold and HTTP-cached, so the cost of an
//!   open (a file open and a schema read) does not matter.
//! - **Read-only, never `immutable`.** The connection is opened with
//!   `SQLITE_OPEN_READONLY` (the flag behind the `mode=ro` URI parameter) and
//!   `PRAGMA query_only`, so it can never write the database or its WAL.
//!   `immutable=1`, which this module replaced, told SQLite the file never
//!   changes: it ignored the WAL (serving whatever had been checkpointed into
//!   the main file) and took no locks (so a checkpoint could tear a read).
//! - **`busy_timeout`.** A WAL reader rarely sees `SQLITE_BUSY` (WAL-index
//!   recovery, a concurrent checkpoint restart); it waits up to
//!   [`BUSY_TIMEOUT`], then the request fails with `503 analytics_busy`.
//!
//! # `-wal` and `-shm` files
//!
//! A WAL reader needs the `-wal` and `-shm` files next to the database:
//!
//! - While the ingester runs, both exist. A reader that can write `-shm` uses
//!   it as shared memory (it updates read marks there, as every WAL reader
//!   does; it never writes the database or the WAL). A reader that cannot
//!   write `-shm` falls back to opening it read-only and building the WAL
//!   index in private memory: slower, still consistent.
//! - When no connection has the database open (ingester stopped), the last
//!   writer has checkpointed and deleted both files. SQLite then *creates*
//!   them on open, owned by the opening process and with the database file's
//!   mode. If the API ran as a different user from the database's owner, the
//!   ingester could later be unable to write those files. [`RelayDb::open`]
//!   therefore refuses (`503 metric_unavailable`) to open a WAL database whose
//!   `-wal`/`-shm` are missing unless this process is root (SQLite hands new
//!   files to the database's owner) or owns the database file. When the
//!   directory is not writable SQLite fails without creating anything.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags};

use crate::error::ApiError;

/// How long a reader waits on `SQLITE_BUSY` before failing the request.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// Errors from opening the relay database.
#[derive(Debug, thiserror::Error)]
pub enum RelayDbError {
    /// The database is in WAL mode, has no `-wal`/`-shm` files (no writer is
    /// attached), and opening it would create them as a user other than the
    /// database's owner.
    #[error(
        "relay database {path} is in WAL mode without -wal/-shm files and this process \
         (euid {euid}) does not own it (uid {owner}); not creating them"
    )]
    SidecarsMissing {
        /// Database path.
        path: PathBuf,
        /// Effective uid of this process.
        euid: u32,
        /// Owner of the database file.
        owner: u32,
    },

    /// Reading the database header or metadata failed.
    #[error("relay database {path}: {source}")]
    Io {
        /// Database path.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },

    /// SQLite failed to open or configure the connection.
    #[error("relay database: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl From<RelayDbError> for ApiError {
    fn from(error: RelayDbError) -> Self {
        match error {
            RelayDbError::SidecarsMissing { .. } => {
                tracing::warn!(error = %error, "relay database unavailable");
                ApiError::MetricUnavailable
            }
            RelayDbError::Sqlite(error) => sqlite_error(error),
            other => ApiError::Internal(other.into()),
        }
    }
}

/// Map a SQLite error to an API error: `SQLITE_BUSY`/`SQLITE_LOCKED` after
/// [`BUSY_TIMEOUT`] become `503 analytics_busy`, everything else `500`.
pub fn sqlite_error(error: rusqlite::Error) -> ApiError {
    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            tracing::warn!(error = %error, "relay database busy");
            ApiError::Overloaded
        }
        _ => ApiError::Internal(anyhow::anyhow!("relay database query failed: {error}")),
    }
}

/// Handle to the ingester's relay-stats database. Cheap to clone; holds no
/// connection (see the module docs).
#[derive(Clone, Debug)]
pub struct RelayDb {
    path: Arc<PathBuf>,
}

impl RelayDb {
    /// A handle for the database at `path`. Nothing is opened yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
        }
    }

    /// The database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a new read-only connection.
    ///
    /// The connection sees every transaction committed before each of its
    /// read transactions starts, including ones committed after it opened.
    pub fn open(&self) -> Result<Connection, RelayDbError> {
        check_sidecars(&self.path)?;
        let conn = Connection::open_with_flags(
            self.path.as_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(conn)
    }

    /// Run `f` against a fresh read-only connection on a blocking thread.
    pub async fn read<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let conn = db.open()?;
            f(&conn)
        })
        .await
        .map_err(|error| ApiError::Internal(error.into()))?
    }
}

/// Refuse to let SQLite create `-wal`/`-shm` files owned by the wrong user.
fn check_sidecars(path: &Path) -> Result<(), RelayDbError> {
    let io = |source| RelayDbError::Io {
        path: path.to_path_buf(),
        source,
    };
    if !is_wal_database(path).map_err(io)? {
        return Ok(());
    }
    let sidecars_exist = sidecar(path, "-wal").exists() && sidecar(path, "-shm").exists();
    if sidecars_exist {
        return Ok(());
    }
    let (euid, owner) = ownership(path).map_err(io)?;
    if may_create_sidecars(euid, owner) {
        Ok(())
    } else {
        Err(RelayDbError::SidecarsMissing {
            path: path.to_path_buf(),
            euid,
            owner,
        })
    }
}

/// Whether SQLite may create missing `-wal`/`-shm` files on our behalf: root
/// (SQLite chowns new files to the database's owner) or the owner itself.
fn may_create_sidecars(euid: u32, owner: u32) -> bool {
    euid == 0 || euid == owner
}

/// `<path>-wal` / `<path>-shm`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether the database header says WAL mode (file format version 2 at byte
/// offsets 18 and 19). Short or empty files are not WAL databases.
fn is_wal_database(path: &Path) -> std::io::Result<bool> {
    let mut header = [0u8; 20];
    let mut file = File::open(path)?;
    let mut read = 0;
    while read < header.len() {
        match file.read(&mut header[read..])? {
            0 => return Ok(false),
            n => read += n,
        }
    }
    Ok(header[18] == 2 || header[19] == 2)
}

#[cfg(unix)]
fn ownership(path: &Path) -> std::io::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(path)?.uid();
    Ok((rustix::process::geteuid().as_raw(), owner))
}

#[cfg(not(unix))]
fn ownership(_path: &Path) -> std::io::Result<(u32, u32)> {
    // No uid model: SQLite's own permission handling is all there is.
    Ok((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_or_the_owner_may_create_sidecars() {
        assert!(may_create_sidecars(0, 999));
        assert!(may_create_sidecars(999, 999));
        assert!(!may_create_sidecars(1000, 999));
    }

    #[test]
    fn sidecar_paths_append_the_suffix() {
        assert_eq!(
            sidecar(Path::new("/data/relay-stats.db"), "-wal"),
            PathBuf::from("/data/relay-stats.db-wal")
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
        assert!(!is_wal_database(&rollback).unwrap());

        let wal = dir.path().join("wal.db");
        Connection::open(&wal)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x)")
            .unwrap();
        assert!(is_wal_database(&wal).unwrap());

        let empty = dir.path().join("empty.db");
        File::create(&empty).unwrap();
        assert!(!is_wal_database(&empty).unwrap());
    }
}
