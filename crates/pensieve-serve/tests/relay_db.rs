//! The API reads the ingester's WAL-mode relay-stats database as a live
//! reader: rows committed after it opened are visible, and it cannot write.
//!
//! Before, the database was opened once with `immutable=1`, which ignores the
//! WAL: the API served whatever had been checkpointed into the main file.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use pensieve_serve::{AppState, Config, RelayDb, router};
use rusqlite::Connection;
use tower::ServiceExt;

const TOKEN: &str = "relay-db-test-token";

/// A writer shaped like the ingester's `RelayManager`: WAL mode, one
/// long-lived connection, the tables the relay endpoints read.
fn ingester_writer(path: &std::path::Path) -> Connection {
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
    conn
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

#[test]
fn reader_sees_rows_committed_after_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    let writer = ingester_writer(&path);
    add_relay(&writer, "wss://one.example", "active");

    let reader = RelayDb::new(&path).open().unwrap();
    assert_eq!(relay_count(&reader), 1);

    add_relay(&writer, "wss://two.example", "idle");
    add_relay(&writer, "wss://three.example", "pending");
    // Still in the WAL: nothing has been checkpointed into the main file.
    assert!(
        std::fs::metadata(dir.path().join("relay-stats.db-wal"))
            .unwrap()
            .len()
            > 0
    );
    assert_eq!(relay_count(&reader), 3);
}

#[test]
fn reader_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    let writer = ingester_writer(&path);
    add_relay(&writer, "wss://one.example", "active");

    let reader = RelayDb::new(&path).open().unwrap();
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

#[test]
fn owner_reads_after_the_ingester_stops() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    let writer = ingester_writer(&path);
    add_relay(&writer, "wss://one.example", "active");
    // The last connection checkpoints and deletes -wal and -shm.
    drop(writer);
    assert!(!dir.path().join("relay-stats.db-wal").exists());
    assert!(!dir.path().join("relay-stats.db-shm").exists());

    // This process owns the file, so SQLite may recreate them (as us, with
    // the database's mode); a different non-root user is refused instead.
    let reader = RelayDb::new(&path).open().unwrap();
    assert_eq!(relay_count(&reader), 1);
}

#[test]
fn reader_never_creates_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    assert!(RelayDb::new(&path).open().is_err());
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn relay_summary_follows_the_ingester() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-stats.db");
    let writer = ingester_writer(&path);
    add_relay(&writer, "wss://one.example", "active");

    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        clickhouse_url: "http://127.0.0.1:9".into(),
        clickhouse_database: "nostr".into(),
        latest_event_watermark_path: None,
        latest_event_watermark_max_age_secs: 900,
        api_tokens: [TOKEN.to_string()].into_iter().collect(),
        relay_db_path: Some(path.clone()),
    };
    let api = router(AppState::new(config).await.unwrap());
    let summary = |api: axum::Router| async move {
        let response = api
            .oneshot(
                Request::get("/api/v1/relays/summary")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()
    };

    let before = summary(api.clone()).await;
    assert_eq!(before["total_discovered"], 1);
    assert_eq!(before["total_active"], 1);

    add_relay(&writer, "wss://two.example", "active");
    add_relay(&writer, "wss://three.example", "blocked");

    let after = summary(api).await;
    assert_eq!(after["total_discovered"], 3);
    assert_eq!(after["total_active"], 2);
    assert_eq!(after["total_blocked"], 1);
}
