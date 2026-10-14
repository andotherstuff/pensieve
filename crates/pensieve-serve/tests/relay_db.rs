#![cfg(unix)]
//! The API reads the ingester's WAL-mode relay-stats database as a live
//! reader: rows committed after it opened are visible, and it cannot write.
//! With no writer attached it reads a snapshot, even from a directory it cannot
//! write (production: `ProtectSystem=strict` makes `/data` read-only for the
//! API).
//!
//! Before, the database was opened once with `immutable=1`, which ignores the
//! WAL: the API served whatever had been checkpointed into the main file.
//!
//! [`RelayDb::new`] reads the database header, so every test constructs it
//! (directly or through `AppState::new`) while no connection to the database
//! is open, as the server does at startup.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use pensieve_serve::relay_db::{Access, sqlite_error};
use pensieve_serve::{ApiError, AppState, Config, RelayDb, router};
use rusqlite::{Connection, OpenFlags};
use tower::ServiceExt;

const TOKEN: &str = "relay-db-test-token";

/// Create the database the way the ingester's `RelayManager` does (WAL mode,
/// the tables the relay endpoints read), then close it.
fn create_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE relays (
             url TEXT PRIMARY KEY,
             first_seen_at INTEGER NOT NULL,
             last_connected_at INTEGER,
             tier TEXT NOT NULL DEFAULT 'discovered',
             status TEXT NOT NULL DEFAULT 'pending',
             consecutive_failures INTEGER NOT NULL DEFAULT 0,
             blocked_reason TEXT,
             notes TEXT
         );
         CREATE TABLE relay_stats_hourly (
             relay_url TEXT NOT NULL,
             hour_start INTEGER NOT NULL,
             events_received INTEGER NOT NULL DEFAULT 0,
             events_novel INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (relay_url, hour_start)
         );
         CREATE TABLE relay_scores (relay_url TEXT PRIMARY KEY, score REAL NOT NULL);",
    )
    .unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

/// The ingester's long-lived writer connection.
fn open_writer(path: &Path) -> Connection {
    Connection::open(path).unwrap()
}

fn add_relay(writer: &Connection, url: &str, status: &str) {
    writer
        .execute(
            "INSERT INTO relays (url, first_seen_at, status) VALUES (?1, 0, ?2)",
            [url, status],
        )
        .unwrap();
}

fn relay_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM relays", [], |row| row.get(0))
        .unwrap()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Makes a path read-only (the API's view of `/data`) until dropped.
struct ReadOnly(PathBuf, u32);

impl ReadOnly {
    fn new(path: &Path, mode: u32) -> Self {
        let original = std::fs::metadata(path).unwrap().permissions().mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        Self(path.to_path_buf(), original)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
    }
}

fn api(path: &Path) -> impl std::future::Future<Output = axum::Router> {
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        clickhouse_url: "http://127.0.0.1:9".into(),
        clickhouse_database: "nostr".into(),
        latest_event_watermark_path: None,
        latest_event_watermark_max_age_secs: 900,
        api_tokens: [TOKEN.to_string()].into_iter().collect(),
        relay_db_path: Some(path.to_path_buf()),
    };
    async move { router(AppState::new(config).await.unwrap()) }
}

async fn summary(api: &axum::Router) -> (StatusCode, serde_json::Value) {
    let response = api
        .clone()
        .oneshot(
            Request::get("/api/v1/relays/summary")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[test]
fn reader_sees_rows_committed_after_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let db = RelayDb::new(&path);
    assert!(db.is_wal());
    let writer = open_writer(&path);
    add_relay(&writer, "wss://one.example", "active");

    let (reader, access) = db.open().unwrap();
    assert_eq!(access, Access::Live);
    assert_eq!(relay_count(&reader), 1);

    add_relay(&writer, "wss://two.example", "idle");
    add_relay(&writer, "wss://three.example", "pending");
    // Still in the WAL: nothing has been checkpointed into the main file.
    assert!(std::fs::metadata(sidecar(&path, "-wal")).unwrap().len() > 0);
    assert_eq!(relay_count(&reader), 3);
}

#[test]
fn reader_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let db = RelayDb::new(&path);
    let writer = open_writer(&path);
    add_relay(&writer, "wss://one.example", "active");

    let (reader, _) = db.open().unwrap();
    let error = reader
        .execute(
            "INSERT INTO relays (url, first_seen_at) VALUES ('wss://x.example', 0)",
            [],
        )
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly),
        "{error}"
    );
    assert_eq!(relay_count(&writer), 1);
}

#[tokio::test]
async fn stopped_ingester_is_read_as_a_snapshot_from_a_read_only_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let writer = open_writer(&path);
    add_relay(&writer, "wss://one.example", "active");
    add_relay(&writer, "wss://two.example", "blocked");
    // A clean stop: the last connection checkpoints and deletes -wal and -shm.
    drop(writer);
    assert_eq!(files_in(dir.path()), ["relay-stats.db"]);

    let _read_only = ReadOnly::new(dir.path(), 0o555);
    let db = RelayDb::new(&path);
    let (reader, access) = db.open().unwrap();
    assert_eq!(access, Access::Snapshot);
    assert_eq!(relay_count(&reader), 2);
    drop(reader);

    let (status, body) = summary(&api(&path).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_discovered"], 2);
    assert_eq!(body["total_blocked"], 1);
    assert_eq!(files_in(dir.path()), ["relay-stats.db"]);
}

#[test]
fn stopped_ingester_snapshot_creates_no_files_in_a_writable_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let db = RelayDb::new(&path);
    let (reader, access) = db.open().unwrap();
    assert_eq!(access, Access::Snapshot);
    assert_eq!(relay_count(&reader), 0);
    assert_eq!(files_in(dir.path()), ["relay-stats.db"]);
}

#[test]
fn cannot_create_wal_maps_to_metric_unavailable() {
    // What a plain read-only open does with the API's view of a stopped
    // ingester's database, and what `RelayDb` maps it to if it ever surfaces.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let _read_only = ReadOnly::new(dir.path(), 0o555);
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let error = conn
        .query_row("SELECT count(*) FROM relays", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_err();
    assert!(
        matches!(sqlite_error(error), ApiError::MetricUnavailable),
        "plain read-only open of a WAL database without -wal in a read-only directory"
    );
}

#[tokio::test]
async fn unopenable_wal_files_return_503() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let api = api(&path).await;
    // -wal and -shm that the API can neither write nor read.
    for suffix in ["-wal", "-shm"] {
        let file = sidecar(&path, suffix);
        std::fs::File::create(&file).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    if std::fs::File::open(sidecar(&path, "-wal")).is_ok() {
        eprintln!("running as root: file permissions are not enforced; skipping");
        return;
    }

    let (status, body) = summary(&api).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "metric_unavailable");
}

#[tokio::test]
async fn busy_database_returns_503_after_the_busy_timeout() {
    // A rollback-journal database: an exclusive lock blocks readers outright
    // (in WAL mode readers never wait on the writer).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE relays (url TEXT PRIMARY KEY, status TEXT)")
        .unwrap();
    let api = api(&path).await;
    let locker = Connection::open(&path).unwrap();
    locker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let started = std::time::Instant::now();
    let (status, body) = summary(&api).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "analytics_busy");
    assert!(
        started.elapsed() >= pensieve_serve::relay_db::BUSY_TIMEOUT,
        "gave up after {:?}",
        started.elapsed()
    );

    locker.execute_batch("COMMIT").unwrap();
    let (status, body) = summary(&api).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn reader_never_creates_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    assert!(RelayDb::new(&path).open().is_err());
    assert!(files_in(dir.path()).is_empty());
}

#[tokio::test]
async fn relay_summary_follows_the_ingester() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    create_db(&path);
    let api = api(&path).await;
    let writer = open_writer(&path);
    add_relay(&writer, "wss://one.example", "active");

    let (status, before) = summary(&api).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(before["total_discovered"], 1);
    assert_eq!(before["total_active"], 1);

    add_relay(&writer, "wss://two.example", "active");
    add_relay(&writer, "wss://three.example", "blocked");

    let (status, after) = summary(&api).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["total_discovered"], 3);
    assert_eq!(after["total_active"], 2);
    assert_eq!(after["total_blocked"], 1);
}
