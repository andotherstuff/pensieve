//! Integration tests for crash-safe ClickHouse indexing against a real
//! ClickHouse (24.8, matching production).
//!
//! Run with a disposable server:
//!
//! ```text
//! just ch-test-up
//! just test-clickhouse
//! ```
//!
//! Each test creates a scratch database with a production-shaped
//! `events_local` plus a `SummingMergeTree` view that counts every inserted
//! row, standing in for the production views that are not duplicate-safe.
//! Merges are stopped, so any re-inserted row shows up as a double count.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clickhouse::Client;
use nostr_sdk::{EventBuilder, Keys};
use pensieve_ingest::{
    ClickHouseConfig, ClickHouseIndexer, EventRow, IndexProgress, IndexQueue, SegmentConfig,
    SegmentWriter, pack_nostr_event,
};
use std::sync::Arc;

const IGNORE_REASON: &str = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL";

struct TestDb {
    url: String,
    name: String,
    admin: Client,
    client: Client,
}

impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("PENSIEVE_TEST_CLICKHOUSE_URL")
            .unwrap_or_else(|_| panic!("{IGNORE_REASON}"));
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "ix_test_{nanos}_{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = Client::default().with_url(&url);
        let client = Client::default().with_url(&url).with_database(&name);
        let db = Self {
            url,
            name,
            admin,
            client,
        };
        db.exec(&format!("CREATE DATABASE {}", db.name)).await;
        db.exec(&format!(
            "CREATE TABLE {}.events_local (
                id String,
                pubkey String,
                created_at DateTime,
                kind UInt16,
                content String CODEC(ZSTD(3)),
                sig String,
                tags Array(Array(String)),
                indexed_at DateTime DEFAULT now(),
                relay_source String DEFAULT ''
            ) ENGINE = ReplacingMergeTree(indexed_at) ORDER BY (id)",
            db.name
        ))
        .await;
        db.exec(&format!(
            "CREATE TABLE {}.row_counts (kind UInt16, n UInt64)
             ENGINE = SummingMergeTree ORDER BY kind",
            db.name
        ))
        .await;
        db.exec(&format!(
            "CREATE MATERIALIZED VIEW {0}.row_counts_mv TO {0}.row_counts AS
             SELECT kind, count() AS n FROM {0}.events_local GROUP BY kind",
            db.name
        ))
        .await;
        for table in ["events_local", "row_counts"] {
            db.exec(&format!("SYSTEM STOP MERGES {}.{table}", db.name))
                .await;
        }
        db
    }

    async fn exec(&self, sql: &str) {
        self.admin
            .query(sql)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    async fn count(&self, sql: &str) -> u64 {
        self.client
            .query(sql)
            .fetch_one::<u64>()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
    }

    /// Rows physically present (duplicates included, merges are stopped).
    async fn rows(&self) -> u64 {
        self.count("SELECT count() FROM events_local").await
    }

    /// Rows seen by the non-idempotent view.
    async fn view_rows(&self) -> u64 {
        self.count("SELECT toUInt64(sum(n)) FROM row_counts").await
    }

    async fn insert(&self, rows: &[EventRow]) {
        let mut inserter = self.client.insert("events_local").unwrap();
        for row in rows {
            inserter.write(row).await.unwrap();
        }
        inserter.end().await.unwrap();
    }

    fn config(&self, queue: Option<Arc<IndexQueue>>) -> ClickHouseConfig {
        ClickHouseConfig {
            url: self.url.clone(),
            database: self.name.clone(),
            table: "events_local".to_string(),
            batch_size: 10,
            index_queue: queue,
        }
    }

    async fn drop(self) {
        self.exec(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .await;
    }
}

fn events(count: usize, tag: &str) -> Vec<nostr_sdk::Event> {
    let keys = Keys::generate();
    (0..count)
        .map(|i| {
            EventBuilder::text_note(format!("{tag} {i}"))
                .sign_with_keys(&keys)
                .unwrap()
        })
        .collect()
}

fn row(event: &nostr_sdk::Event) -> EventRow {
    EventRow {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs() as u32,
        kind: event.kind.as_u16(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
            .collect(),
        relay_source: String::new(),
    }
}

/// Write each group of events to its own sealed, compressed segment and
/// return the sealed (uncompressed) paths, as the writer journals them.
fn write_segments(dir: &Path, groups: &[&[nostr_sdk::Event]]) -> Vec<PathBuf> {
    let writer = SegmentWriter::new(
        SegmentConfig {
            output_dir: dir.to_path_buf(),
            compress: true,
            ..Default::default()
        },
        None,
        None,
    )
    .unwrap();
    let mut paths = Vec::new();
    for group in groups {
        for event in *group {
            writer.write(pack_nostr_event(event).unwrap()).unwrap();
        }
        paths.push(writer.seal().unwrap().unwrap().path);
    }
    writer.wait_for_compression().unwrap();
    paths
}

/// Replays the 2026-10-12 incident: one segment cut off mid-batch, one sealed
/// but never indexed, plus a bare-path entry from an older binary. A restart
/// must fill every gap without inserting any row twice.
#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn restart_resumes_unfinished_segments_without_double_counting() {
    let db = TestDb::new().await;
    let dir = tempfile::tempdir().unwrap();
    let partial = events(35, "partial");
    let unindexed = events(12, "unindexed");
    let legacy = events(15, "legacy");
    let paths = write_segments(dir.path(), &[&partial, &unindexed, &legacy]);

    // Previous run: batches [0,10) and [10,20) acknowledged, [20,30) was in
    // flight when the process died and did reach ClickHouse.
    let partial_rows: Vec<EventRow> = partial.iter().map(row).collect();
    db.insert(&partial_rows[..30]).await;
    // The legacy entry has no progress; some of its rows made it in.
    let legacy_rows: Vec<EventRow> = legacy.iter().map(row).collect();
    db.insert(&legacy_rows[..7]).await;
    assert_eq!(db.view_rows().await, 37);

    let journal = dir.path().join(".clickhouse_reindex_queue");
    {
        let queue = IndexQueue::open(&journal).unwrap();
        queue
            .record_progress(
                &paths[0],
                IndexProgress {
                    committed: 20,
                    verify_end: 30,
                },
            )
            .unwrap();
        queue.record_sealed(&paths[1]).unwrap();
    }
    let mut contents = std::fs::read_to_string(&journal).unwrap();
    contents.push_str(&format!("{}.gz\n", paths[2].display()));
    std::fs::write(&journal, contents).unwrap();

    let queue = Arc::new(IndexQueue::open(&journal).unwrap());
    assert_eq!(queue.previous_run().len(), 3);
    let indexer = ClickHouseIndexer::new(db.config(Some(Arc::clone(&queue)))).unwrap();
    let (sender, receiver) = crossbeam_channel::unbounded();
    drop(sender);
    indexer.start(receiver).join().unwrap();

    let total = (35 + 12 + 15) as u64;
    assert_eq!(db.rows().await, total, "no row may be inserted twice");
    assert_eq!(
        db.view_rows().await,
        total,
        "views must count each row once"
    );
    assert_eq!(indexer.stats().events_indexed, 5 + 12 + 8);
    assert!(queue.is_empty());
    assert!(!journal.exists());
    db.drop().await;
}

/// A shutdown request leaves segments still waiting on the channel in the
/// journal; the next start indexes them.
#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn shutdown_defers_queued_segments_to_next_start() {
    let db = TestDb::new().await;
    let dir = tempfile::tempdir().unwrap();
    let batch = events(23, "deferred");
    let (sender, receiver) = crossbeam_channel::unbounded();
    let writer = SegmentWriter::new(
        SegmentConfig {
            output_dir: dir.path().to_path_buf(),
            compress: true,
            ..Default::default()
        },
        Some(sender),
        None,
    )
    .unwrap();
    for event in &batch {
        writer.write(pack_nostr_event(event).unwrap()).unwrap();
    }
    writer.seal().unwrap().unwrap();
    writer.wait_for_compression().unwrap();
    drop(writer);

    let journal = dir.path().join(".clickhouse_reindex_queue");
    let queue = Arc::new(IndexQueue::open(&journal).unwrap());
    let indexer = ClickHouseIndexer::new(db.config(Some(Arc::clone(&queue)))).unwrap();
    indexer.stop();
    indexer.start(receiver).join().unwrap();
    assert_eq!(db.rows().await, 0);
    assert_eq!(queue.len(), 1, "unindexed segment must be journaled");
    drop(queue);

    let queue = Arc::new(IndexQueue::open(&journal).unwrap());
    let indexer = ClickHouseIndexer::new(db.config(Some(Arc::clone(&queue)))).unwrap();
    let (sender, receiver) = crossbeam_channel::unbounded();
    drop(sender);
    indexer.start(receiver).join().unwrap();
    assert_eq!(db.rows().await, 23);
    assert_eq!(db.view_rows().await, 23);
    assert!(queue.is_empty());
    db.drop().await;
}

/// The existence check spans several queries for large batches.
#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn existing_ids_spans_query_chunks() {
    let db = TestDb::new().await;
    let ids: Vec<String> = (0..2_500u32).map(|i| format!("{i:064x}")).collect();
    let rows: Vec<EventRow> = ids
        .iter()
        .step_by(2)
        .map(|id| EventRow {
            id: id.clone(),
            pubkey: "00".repeat(32),
            created_at: 1_700_000_000,
            kind: 1,
            content: String::new(),
            sig: "00".repeat(64),
            tags: Vec::new(),
            relay_source: String::new(),
        })
        .collect();
    db.insert(&rows).await;

    let lookup: Vec<&str> = ids.iter().map(String::as_str).collect();
    let present = ClickHouseIndexer::existing_ids(&db.client, "events_local", &lookup)
        .await
        .unwrap();
    assert_eq!(present.len(), 1_250);
    assert!(present.contains(&ids[0]));
    assert!(!present.contains(&ids[1]));
    assert!(present.contains(&ids[2_498]));
    db.drop().await;
}
