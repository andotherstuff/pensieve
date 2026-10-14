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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clickhouse::Client;
use nostr_sdk::{EventBuilder, Keys};
use pensieve_ingest::{
    ClickHouseConfig, ClickHouseIndexer, EventRow, IndexProgress, IndexQueue, IndexRetryPolicy,
    SegmentConfig, SegmentWriter, UNJOURNALED_ATTEMPTS, pack_nostr_event,
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
            retry: IndexRetryPolicy::default(),
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

/// TCP forwarder in front of ClickHouse that can refuse service, standing in
/// for a stopped or restarting ClickHouse container.
struct Gate {
    url: String,
    open: Arc<AtomicBool>,
}

impl Gate {
    /// A closed gate in front of `upstream` (`http://host:port`).
    async fn closed(upstream: &str) -> Self {
        let upstream = upstream
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let open = Arc::new(AtomicBool::new(false));
        let accepting = Arc::clone(&open);
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if !accepting.load(Ordering::SeqCst) {
                    // Drop the connection: the client sees ClickHouse go away.
                    continue;
                }
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });
        Self { url, open }
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

async fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn seal(writer: &SegmentWriter, batch: &[nostr_sdk::Event]) -> PathBuf {
    for event in batch {
        writer.write(pack_nostr_event(event).unwrap()).unwrap();
    }
    let path = writer.seal().unwrap().unwrap().path;
    writer.wait_for_compression().unwrap();
    path
}

/// ClickHouse is down for far longer than the old retry budget (3 attempts,
/// about 6 s): at start, with a segment left by a previous run, and while
/// further segments are sealed. Once it is back, every segment is indexed in
/// seal order, exactly once, without restarting the indexer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn outage_longer_than_retry_budget_heals_in_order_without_restart() {
    let db = TestDb::new().await;
    // Record when each row arrived, to check segment order.
    db.exec(&format!(
        "CREATE TABLE {}.arrivals (tag String, at DateTime64(9))
         ENGINE = MergeTree ORDER BY at",
        db.name
    ))
    .await;
    db.exec(&format!(
        "CREATE MATERIALIZED VIEW {0}.arrivals_mv TO {0}.arrivals AS
         SELECT splitByChar(' ', content)[1] AS tag, now64(9) AS at FROM {0}.events_local",
        db.name
    ))
    .await;

    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join(".clickhouse_reindex_queue");
    // Boot: a segment sealed by the previous run was never indexed.
    let boot = events(14, "boot");
    let boot_path = write_segments(dir.path(), &[&boot]).remove(0);
    IndexQueue::open(&journal)
        .unwrap()
        .record_sealed(&boot_path)
        .unwrap();

    let gate = Gate::closed(&db.url).await;
    let queue = Arc::new(IndexQueue::open(&journal).unwrap());
    let mut config = db.config(Some(Arc::clone(&queue)));
    config.url = gate.url.clone();
    config.retry = IndexRetryPolicy {
        initial_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(200),
    };
    let indexer = ClickHouseIndexer::new(config).unwrap();
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
    .unwrap()
    .with_index_queue(Arc::clone(&queue));
    let handle = indexer.start(receiver);

    // Well past the old budget, sealing more segments meanwhile.
    let budget = UNJOURNALED_ATTEMPTS as usize;
    wait_for("retries", Duration::from_secs(30), || {
        indexer.stats().retries >= 3 * budget
    })
    .await;
    seal(&writer, &events(23, "first"));
    seal(&writer, &events(5, "second"));
    wait_for("more retries", Duration::from_secs(30), || {
        indexer.stats().retries >= 6 * budget
    })
    .await;
    seal(&writer, &events(31, "third"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "the indexer must keep running");
    assert_eq!(db.rows().await, 0, "nothing reached ClickHouse");
    assert_eq!(queue.len(), 4, "every segment stays journaled");
    assert!(queue.oldest_pending_age().unwrap() > Duration::ZERO);
    assert_eq!(indexer.stats().segments_indexed, 0);

    gate.open();
    wait_for("the backlog to clear", Duration::from_secs(60), || {
        queue.is_empty()
    })
    .await;

    let total = (14 + 23 + 5 + 31) as u64;
    assert_eq!(db.rows().await, total, "no row may be inserted twice");
    assert_eq!(db.view_rows().await, total, "views count each row once");
    assert_eq!(indexer.stats().segments_indexed, 4);
    assert_eq!(indexer.stats().events_indexed, total as usize);
    assert_eq!(queue.oldest_pending_age(), None);

    let order = db
        .client
        .query("SELECT tag FROM arrivals GROUP BY tag ORDER BY min(at)")
        .fetch_all::<String>()
        .await
        .unwrap();
    assert_eq!(order, ["boot", "first", "second", "third"]);
    // Segments do not interleave: each one finished before the next began.
    let overlaps = db
        .count(
            "SELECT count() FROM
               (SELECT tag, min(at) AS lo, max(at) AS hi FROM arrivals GROUP BY tag) AS a
               CROSS JOIN
               (SELECT tag, min(at) AS lo, max(at) AS hi FROM arrivals GROUP BY tag) AS b
             WHERE a.tag != b.tag AND a.lo <= b.hi AND b.lo <= a.hi",
        )
        .await;
    assert_eq!(overlaps, 0);

    drop(writer);
    handle.join().unwrap();
    assert!(!journal.exists());
    db.drop().await;
}

/// A shutdown request during a long backoff wakes the indexer promptly. The
/// journal keeps the failing segment's progress (including the batch whose
/// outcome is unknown) and every segment sealed meanwhile. Needs no
/// ClickHouse: the indexer points at a closed port.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_during_backoff_is_prompt_and_keeps_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join(".clickhouse_reindex_queue");
    let stranded_path = write_segments(dir.path(), &[&events(25, "stranded")]).remove(0);
    IndexQueue::open(&journal)
        .unwrap()
        .record_progress(
            &stranded_path,
            IndexProgress {
                committed: 10,
                verify_end: 10,
            },
        )
        .unwrap();

    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let queue = Arc::new(IndexQueue::open(&journal).unwrap());
    let indexer = ClickHouseIndexer::new(ClickHouseConfig {
        url: format!("http://127.0.0.1:{closed_port}"),
        database: "nostr".to_string(),
        table: "events_local".to_string(),
        batch_size: 10,
        index_queue: Some(Arc::clone(&queue)),
        // 5 ms doubling: the 10th retry sleeps 2.56 s.
        retry: IndexRetryPolicy {
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_secs(60),
        },
    })
    .unwrap();
    let (sender, receiver) = crossbeam_channel::unbounded();
    // A writer without the journal: the notification is the only record, as
    // when the writer's journal write fails.
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
    let handle = indexer.start(receiver);

    wait_for("retries", Duration::from_secs(30), || {
        indexer.stats().retries >= 10
    })
    .await;
    let later_path = seal(&writer, &events(4, "later"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!handle.is_finished());
    assert_eq!(queue.len(), 1, "the later segment waits in memory");

    let asked = Instant::now();
    indexer.stop();
    wait_for("shutdown", Duration::from_secs(1), || handle.is_finished()).await;
    assert!(asked.elapsed() < Duration::from_secs(1));
    assert_eq!(indexer.stats().retries, 10, "no attempt after the stop");
    handle.join().unwrap();
    drop(writer);
    drop(queue);

    let reopened = IndexQueue::open(&journal).unwrap();
    // The batch [10, 20) was sent and failed: its outcome is unknown. Both
    // entries now name the compressed files the indexer was given.
    let gz = |path: &Path| path.with_extension("notepack.gz");
    assert_eq!(
        reopened.previous_run(),
        vec![
            (
                gz(&stranded_path),
                IndexProgress {
                    committed: 10,
                    verify_end: 20,
                }
            ),
            (gz(&later_path), IndexProgress::default()),
        ]
    );
}
