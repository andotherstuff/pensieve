//! Integration tests for the migration 019 API rollups against a real
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
//! `events_local` (no projections) and migration 019. Reads are asserted
//! against an independent Rust oracle over the stored source rows, with merges
//! stopped (duplicate rows and states physically present) and after
//! `OPTIMIZE ... FINAL`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clickhouse::Client;
use serde::{Deserialize, Serialize};

use pensieve_serve::rollups::backfill::{self, BackfillOptions, StopReason};
use pensieve_serve::rollups::plan::{self, CheckpointDir, PendingShard, PlanOptions, utc_day};
use pensieve_serve::rollups::validate::{self, Outcome, ValidateOptions};
use pensieve_serve::rollups::{
    ALL_KINDS, EXCLUDED_PUBLISHER_KINDS, Target, is_unknown_table, longform_sql, publishers_sql,
};

const IGNORE_REASON: &str = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL";
const BASE_SCHEMA: &str = include_str!("fixtures/events_local.sql");
const MIGRATION_019: &str =
    include_str!("../../../docs/migrations/prepared/019_api_publishers_longform.sql");
const MIGRATION_017: &str =
    include_str!("../../../docs/migrations/prepared/017_preview_compact_lookups.sql");
const ROLLBACK_019: &str =
    include_str!("../../../docs/migrations/rollback/019_drop_api_publishers_longform.sql");
const DAY: u32 = 86_400;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestDb {
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
            "api_rollups_{nanos}_{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = Client::default().with_url(&url);
        admin
            .query(&format!("CREATE DATABASE {name}"))
            .execute()
            .await
            .expect("create database");
        let client = Client::default().with_url(&url).with_database(&name);
        let db = Self {
            name,
            admin,
            client,
        };
        db.apply(BASE_SCHEMA).await.expect("base schema");
        db
    }

    async fn apply(&self, sql: &str) -> Result<(), String> {
        for statement in statements(sql, &self.name) {
            self.admin
                .query(&statement)
                .execute()
                .await
                .map_err(|e| format!("{e}\n{statement}"))?;
        }
        Ok(())
    }

    async fn apply_migration(&self) {
        self.apply(MIGRATION_019).await.expect("migration 019");
    }

    async fn exec(&self, sql: &str) {
        self.admin
            .query(sql)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    async fn set_merges(&self, start: bool) {
        for table in ["events_local", "publisher_activity", "longform_latest"] {
            let verb = if start { "START" } else { "STOP" };
            self.exec(&format!("SYSTEM {verb} MERGES {}.{table}", self.name))
                .await;
        }
    }

    async fn merge_all(&self) {
        self.set_merges(true).await;
        for table in ["events_local", "publisher_activity", "longform_latest"] {
            self.exec(&format!("OPTIMIZE TABLE {}.{table} FINAL", self.name))
                .await;
        }
    }

    async fn insert_sync(&self, events: &[Ev]) {
        insert_rows(
            &self.client.clone().with_option("async_insert", "0"),
            events,
        )
        .await;
    }

    /// Production-like fire-and-forget async insert, then flush the queue.
    async fn insert_async(&self, events: &[Ev]) {
        let client = self
            .client
            .clone()
            .with_option("async_insert", "1")
            .with_option("wait_for_async_insert", "0");
        insert_rows(&client, events).await;
        self.exec("SYSTEM FLUSH ASYNC INSERT QUEUE").await;
    }

    async fn now(&self) -> u32 {
        self.client
            .query("SELECT toUInt32(now())")
            .fetch_one()
            .await
            .unwrap()
    }

    async fn stored(&self) -> Vec<Stored> {
        self.client
            .query(
                "SELECT id, pubkey, kind, toUInt32(created_at), toUInt32(indexed_at), \
                 toUInt64(length(content)), d_tag FROM events_local",
            )
            .fetch_all()
            .await
            .unwrap()
    }

    async fn drop(self) {
        self.admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await
            .expect("drop database");
    }
}

/// Split a SQL file into statements, point `nostr` at the scratch database
/// and drop comments.
fn statements(sql: &str, database: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in sql.lines() {
        let code = match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        };
        if code.trim().is_empty() {
            continue;
        }
        current.push_str(code);
        current.push('\n');
        if code.trim_end().ends_with(';') {
            let statement = current.trim().trim_end_matches(';').to_string();
            out.push(
                statement
                    .replace("nostr.", &format!("{database}."))
                    .replace("database = 'nostr'", &format!("database = '{database}'")),
            );
            current.clear();
        }
    }
    assert!(
        current.trim().is_empty(),
        "unterminated statement: {current}"
    );
    for statement in &out {
        assert!(
            !statement.contains('?'),
            "statement contains '?': {statement}"
        );
    }
    out
}

#[derive(Debug, Clone)]
struct Ev {
    id: String,
    pubkey: String,
    created_at: u32,
    kind: u16,
    content: String,
    tags: Vec<Vec<String>>,
}

#[derive(Serialize, clickhouse::Row)]
struct InsertRow<'a> {
    id: &'a str,
    pubkey: &'a str,
    created_at: u32,
    kind: u16,
    content: &'a str,
    sig: &'a str,
    tags: &'a [Vec<String>],
    relay_source: &'a str,
}

async fn insert_rows(client: &Client, events: &[Ev]) {
    let mut insert = client.insert("events_local").expect("insert");
    for e in events {
        insert
            .write(&InsertRow {
                id: &e.id,
                pubkey: &e.pubkey,
                created_at: e.created_at,
                kind: e.kind,
                content: &e.content,
                sig: "",
                tags: &e.tags,
                relay_source: "",
            })
            .await
            .expect("write row");
    }
    insert.end().await.expect("end insert");
}

#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct Stored {
    id: String,
    pubkey: String,
    kind: u16,
    created_at: u32,
    indexed_at: u32,
    content_length: u64,
    d_tag: String,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A 64-hex event id whose prefix is spread over the keyspace.
fn event_id(n: u64) -> String {
    format!("{:016x}{n:048x}", splitmix(n))
}

fn pubkey(n: u64) -> String {
    format!("{:064x}", 0xa000 + n)
}

fn ev(n: u64, author: u64, kind: u16, created_at: u32, content: &str, tags: &[&[&str]]) -> Ev {
    Ev {
        id: event_id(n),
        pubkey: pubkey(author),
        created_at,
        kind,
        content: content.to_string(),
        tags: tags
            .iter()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .collect(),
    }
}

/// A mixed workload: kinds (including excluded ones), authors and timestamps
/// from 500 days ago to 100 days ahead.
fn workload(first_id: u64, count: u64, now: u32) -> Vec<Ev> {
    const KINDS: [u16; 9] = [1, 1, 7, 3, 0, 30023, 445, 1059, 6];
    (first_id..first_id + count)
        .map(|n| {
            let r = splitmix(n ^ 0x5eed);
            let author = r % 7;
            let kind = KINDS[(r >> 8) as usize % KINDS.len()];
            let offset = match (r >> 16) % 10 {
                0 => 500 * DAY,
                1 => 380 * DAY + (r >> 24) as u32 % DAY,
                2 => 0,
                _ => (r >> 24) as u32 % (370 * DAY),
            };
            let created_at = match (r >> 40) % 23 {
                // Far future and slightly future timestamps.
                0 => now + 100 * DAY,
                1 => now + DAY / 2,
                _ => now - offset,
            };
            let mut e = ev(
                n,
                author,
                kind,
                created_at,
                &"x".repeat((r % 50) as usize),
                &[],
            );
            if kind == 30023 {
                e.tags = vec![vec!["d".into(), ((r >> 48) % 3).to_string()]];
            }
            e
        })
        .collect()
}

/// Events whose tags are malformed in every way the views must survive.
fn malformed(first_id: u64, now: u32) -> Vec<Ev> {
    vec![
        ev(first_id, 1, 30023, now - DAY / 2, "a", &[&[]]),
        ev(first_id + 1, 1, 30023, now - DAY / 2, "bb", &[&["d"]]),
        ev(
            first_id + 2,
            1,
            30023,
            now - DAY / 2,
            "ccc",
            &[&["", ""], &["d", ""]],
        ),
        ev(first_id + 3, 2, 1, now - DAY / 2, "d", &[&[], &["e"]]),
    ]
}

// ---------------------------------------------------------------------------
// Oracle
// ---------------------------------------------------------------------------

type PublisherOut = (String, u64, u64, u32, u32);

fn expected_publishers(
    rows: &[Stored],
    now: u32,
    days: u32,
    kind: Option<u16>,
    limit: usize,
) -> Vec<PublisherOut> {
    let start = utc_day(now - days * DAY);
    let mut by_pubkey: BTreeMap<&str, (u64, BTreeSet<u16>, u32, u32)> = BTreeMap::new();
    for r in rows {
        if EXCLUDED_PUBLISHER_KINDS.contains(&r.kind) || kind.is_some_and(|k| k != r.kind) {
            continue;
        }
        let activity = r.created_at.min(r.indexed_at + DAY);
        if utc_day(activity) < start {
            continue;
        }
        let entry = by_pubkey
            .entry(&r.pubkey)
            .or_insert((0, BTreeSet::new(), u32::MAX, 0));
        entry.0 += 1;
        entry.1.insert(r.kind);
        entry.2 = entry.2.min(r.created_at);
        entry.3 = entry.3.max(r.created_at);
    }
    let mut out: Vec<PublisherOut> = by_pubkey
        .into_iter()
        .map(|(p, (n, kinds, first, last))| (p.to_string(), n, kinds.len() as u64, first, last))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit);
    out
}

fn expected_longform(rows: &[Stored], now: u32, days: Option<u32>) -> (u64, u64, u64) {
    let mut winners: BTreeMap<(&str, &str), &Stored> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.kind == 30023) {
        let key = (r.pubkey.as_str(), r.d_tag.as_str());
        let better = match winners.get(&key) {
            None => true,
            Some(w) => r.created_at > w.created_at || (r.created_at == w.created_at && r.id < w.id),
        };
        if better {
            winners.insert(key, r);
        }
    }
    let kept: Vec<&&Stored> = winners
        .values()
        .filter(|w| days.is_none_or(|d| w.created_at >= now - d * DAY))
        .collect();
    let authors: BTreeSet<&str> = kept.iter().map(|w| w.pubkey.as_str()).collect();
    (
        kept.len() as u64,
        authors.len() as u64,
        kept.iter().map(|w| w.content_length).sum(),
    )
}

async fn read_publishers(
    c: &Client,
    days: u32,
    kind: Option<u16>,
    limit: u32,
) -> Vec<PublisherOut> {
    c.query(&publishers_sql())
        .bind(days)
        .bind(kind.map_or(ALL_KINDS, i32::from))
        .bind(limit)
        .fetch_all()
        .await
        .unwrap()
}

async fn read_longform(c: &Client, days: Option<u32>) -> (u64, u64, u64) {
    let q = c.query(&longform_sql(days.is_some()));
    let q = match days {
        Some(d) => q.bind(d),
        None => q,
    };
    q.fetch_one().await.unwrap()
}

/// Every endpoint shape equals the oracle over the stored rows.
async fn assert_endpoints_match(db: &TestDb, phase: &str) {
    let rows = db.stored().await;
    let now = db.now().await;
    for days in [1, 7, 30, 90, 366] {
        for kind in [None, Some(1), Some(30023), Some(6)] {
            for limit in [1000, 2] {
                let got = read_publishers(&db.client, days, kind, limit).await;
                let want = expected_publishers(&rows, now, days, kind, limit as usize);
                assert_eq!(
                    got, want,
                    "{phase}: publishers days={days} kind={kind:?} limit={limit}"
                );
            }
        }
    }
    for days in [None, Some(1), Some(30), Some(400)] {
        assert_eq!(
            read_longform(&db.client, days).await,
            expected_longform(&rows, now, days),
            "{phase}: longform days={days:?}"
        );
    }
}

fn backfill_options() -> BackfillOptions {
    BackfillOptions {
        delay_between_shards: Duration::ZERO,
        grace: Duration::ZERO,
        min_free_bytes: 0,
        quiet_minutes: None,
        ..BackfillOptions::default()
    }
}

fn plan_options() -> PlanOptions {
    PlanOptions {
        id_prefix_len: 1,
        boundary_margin_secs: 5,
    }
}

async fn total_day_events(db: &TestDb) -> u64 {
    db.client
        .query(&format!(
            "SELECT sum(event_count) FROM publisher_activity WHERE grain = 'day' AND kind = {ALL_KINDS}"
        ))
        .fetch_one()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn views_feed_endpoint_reads_before_and_after_merges() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    // Re-applying is a no-op (the empty-table guard passes while the view exists).
    db.apply_migration().await;
    db.set_merges(false).await;
    let now = db.now().await;

    let events = workload(0, 600, now);
    for chunk in events[..300].chunks(70) {
        db.insert_sync(chunk).await;
    }
    for chunk in events[300..].chunks(100) {
        db.insert_async(chunk).await;
    }
    db.insert_sync(&malformed(10_000, now)).await;
    db.insert_async(&malformed(10_100, now)).await;

    let parts: u64 = db
        .client
        .query("SELECT count() FROM system.parts WHERE database = currentDatabase() AND table = 'publisher_activity' AND active")
        .fetch_one()
        .await
        .unwrap();
    assert!(parts > 1, "merges are stopped, so several parts must exist");

    // Excluded kinds never reach the rollup.
    let excluded: u64 = db
        .client
        .query("SELECT count() FROM publisher_activity WHERE kind IN (445, 1059) OR has(kinds, 445) OR has(kinds, 1059)")
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(excluded, 0);

    assert_endpoints_match(&db, "before merges").await;
    db.merge_all().await;
    assert_endpoints_match(&db, "after merges").await;

    // Article versions arriving out of order, with a created_at tie and a
    // replayed insert of the same event.
    db.set_merges(false).await;
    let a = 7;
    let v_new_hi = ev(
        20_002,
        a,
        30023,
        now - 100,
        "newer, higher id",
        &[&["d", "post"]],
    );
    let mut v_new_lo = ev(
        20_001,
        a,
        30023,
        now - 100,
        "newer, lowest id wins",
        &[&["d", "post"]],
    );
    v_new_lo.id = format!("0{}", &v_new_lo.id[1..]);
    let v_old = ev(20_000, a, 30023, now - 5 * DAY, "old", &[&["d", "post"]]);
    db.insert_sync(std::slice::from_ref(&v_new_hi)).await;
    db.insert_sync(&[v_old, v_new_lo.clone()]).await;
    db.insert_sync(&[v_new_hi]).await;

    let author = pubkey(a);
    for phase in ["replay before merges", "replay after merges"] {
        let winner: (String, u32, u64) = db
            .client
            .query(
                "SELECT tupleElement(l, 1), toUInt32(tupleElement(l, 2)), tupleElement(l, 3) \
                 FROM (SELECT argMinMerge(latest) AS l FROM longform_latest \
                 WHERE pubkey = ? AND d_tag = 'post')",
            )
            .bind(&author)
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(
            winner.0, v_new_lo.id,
            "{phase}: created_at tie, lowest id wins"
        );
        assert_eq!(winner.2, "newer, lowest id wins".len() as u64, "{phase}");
        let rows = db.stored().await;
        let now = db.now().await;
        for days in [None, Some(1), Some(30)] {
            assert_eq!(
                read_longform(&db.client, days).await,
                expected_longform(&rows, now, days),
                "{phase}: longform days={days:?}"
            );
        }
        // Documented: sums count a replayed insert twice, before and after
        // merges (events_local deduplicates it, the rollup cannot).
        let counted: u64 = db
            .client
            .query(&format!(
                "SELECT sum(event_count) FROM publisher_activity \
                 WHERE grain = 'day' AND kind = {ALL_KINDS} AND pubkey = ?"
            ))
            .bind(&author)
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(counted, 4, "{phase}: 3 distinct events, one inserted twice");
        if phase == "replay before merges" {
            db.merge_all().await;
        }
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn backfill_counts_each_row_exactly_once_around_the_view_boundary() {
    let db = TestDb::new().await;
    // Migration 017's addressable_latest (same winner rule) for the
    // validator's longform cross-check.
    db.apply(MIGRATION_017).await.expect("migration 017");
    let now = db.now().await;
    // History inserted before the 019 views exist.
    let history = workload(0, 800, now);
    for chunk in history.chunks(90) {
        db.insert_sync(chunk).await;
    }
    db.insert_sync(&malformed(50_000, now)).await;

    db.apply_migration().await;
    // Let the boundary clear the view's creation by the plan's margin.
    tokio::time::sleep(Duration::from_secs(6)).await;
    // Live inserts captured by the views, for the same authors, kinds and days.
    let live_start = db.now().await;
    let live = workload(100_000, 300, now);
    for chunk in live.chunks(60) {
        db.insert_sync(chunk).await;
    }
    let first_live: u32 = db
        .client
        .query(
            "SELECT toUInt32(min(indexed_at)) FROM events_local WHERE indexed_at >= toDateTime(?)",
        )
        .bind(live_start)
        .fetch_one()
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut plans = BTreeMap::new();
    for target in Target::MIGRATION_019 {
        let plan = plan::build_plan(&db.client, &db.name, target, &plan_options())
            .await
            .unwrap();
        assert_eq!(plan.shards.len(), 16);
        let cp = CheckpointDir::new(dir.path(), target);
        cp.save_plan(&plan, false).unwrap();
        plans.insert(target, (plan, cp));
    }
    let bounds = plans[&Target::Publishers]
        .0
        .publisher_bounds
        .clone()
        .unwrap();
    assert_eq!(
        bounds.indexed_before, first_live,
        "boundary = first captured second"
    );

    // Before the backfill only the live rows are counted.
    let live_only = total_day_events(&db).await;
    for target in Target::MIGRATION_019 {
        let (plan, cp) = &plans[&target];
        let report = backfill::run_backfill(&db.client, plan, cp, &backfill_options())
            .await
            .unwrap();
        assert_eq!(report.stopped, StopReason::Complete);
        assert_eq!(report.shards_run, 16);
    }
    assert!(total_day_events(&db).await > live_only);
    // Every shard is exactly one part.
    db.exec("SYSTEM FLUSH LOGS").await;
    let shard_parts: Vec<u64> = db
        .client
        .query(
            "SELECT count() FROM system.part_log WHERE database = currentDatabase() \
             AND table = 'publisher_activity' AND event_type = 'NewPart' \
             AND startsWith(query_id, 'api-rollups-backfill-publishers') GROUP BY query_id",
        )
        .fetch_all()
        .await
        .unwrap();
    assert!(
        !shard_parts.is_empty() && shard_parts.iter().all(|&n| n == 1),
        "{shard_parts:?}"
    );

    assert_endpoints_match(&db, "after backfill").await;
    let total = total_day_events(&db).await;

    // The validator agrees, including pubkeys active around the boundary.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let options = ValidateOptions {
        targets: Target::MIGRATION_019.to_vec(),
        sample: 50,
        boundary_sample: 50,
        sample_permyriad: 10_000,
        cutoff_margin: Duration::from_secs(1),
        batch: 3,
        ..ValidateOptions::default()
    };
    let report = validate::run(
        &db.client,
        &db.name,
        &options,
        Some(&plans[&Target::Publishers].0),
    )
    .await
    .unwrap();
    assert!(report.passed(), "{}", report.render());
    for t in &report.targets {
        assert!(t.compared > 0, "{}", report.render());
        assert_eq!(t.rows.get(&Outcome::Excess), None, "{}", report.render());
    }
    let cross = report.targets[1]
        .cross_check
        .clone()
        .expect("cross-check ran");
    assert!(cross.matched > 0 && !cross.disagrees(), "{cross:?}");

    // Re-running is a no-op.
    for target in Target::MIGRATION_019 {
        let (plan, cp) = &plans[&target];
        let report = backfill::run_backfill(&db.client, plan, cp, &backfill_options())
            .await
            .unwrap();
        assert_eq!(report.shards_run, 0);
    }
    assert_eq!(total_day_events(&db).await, total);

    // A shard whose outcome was lost after it committed is recorded, not rerun.
    let (plan, cp) = &plans[&Target::Publishers];
    let done = cp.completed_shards().unwrap();
    let lost = &done[3];
    std::fs::remove_file(
        cp.root()
            .join("shards")
            .join(format!("{:06}.json", lost.index)),
    )
    .unwrap();
    cp.save_pending(&PendingShard {
        index: lost.index,
        range: lost.range.clone(),
        query_id: lost.query_id.clone(),
        started_at: 0,
    })
    .unwrap();
    let report = backfill::run_backfill(&db.client, plan, cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!((report.pending_resolved, report.shards_run), (1, 0));
    assert_eq!(total_day_events(&db).await, total);

    // An additive shard with no trace anywhere stops the run.
    let unknown = &done[5];
    std::fs::remove_file(
        cp.root()
            .join("shards")
            .join(format!("{:06}.json", unknown.index)),
    )
    .unwrap();
    cp.save_pending(&PendingShard {
        index: unknown.index,
        range: unknown.range.clone(),
        query_id: "never-sent".into(),
        started_at: 0,
    })
    .unwrap();
    let err = backfill::run_backfill(&db.client, plan, cp, &backfill_options())
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("cannot tell whether"),
        "{err:#}"
    );
    assert_eq!(total_day_events(&db).await, total);
    // The operator checked: it had committed.
    backfill::resolve_by_hand(cp, unknown.index, true, 0).unwrap();
    let report = backfill::run_backfill(&db.client, plan, cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!(report.shards_run, 0);
    assert_eq!(total_day_events(&db).await, total);

    // An idempotent (longform) shard with no trace simply runs again.
    let (plan, cp) = &plans[&Target::Longform];
    let done = cp.completed_shards().unwrap();
    let unknown = &done[2];
    std::fs::remove_file(
        cp.root()
            .join("shards")
            .join(format!("{:06}.json", unknown.index)),
    )
    .unwrap();
    cp.save_pending(&PendingShard {
        index: unknown.index,
        range: unknown.range.clone(),
        query_id: "never-sent-either".into(),
        started_at: 0,
    })
    .unwrap();
    let report = backfill::run_backfill(&db.client, plan, cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!((report.pending_resolved, report.shards_run), (1, 1));

    db.set_merges(false).await;
    assert_endpoints_match(&db, "after reruns").await;
    db.merge_all().await;
    assert_endpoints_match(&db, "after reruns and merges").await;
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn plan_refuses_a_boundary_racing_view_creation_and_recovery_works() {
    let db = TestDb::new().await;
    let now = db.now().await;
    db.insert_sync(&workload(0, 50, now)).await;
    db.apply_migration().await;
    // An insert right after creation: too close to tell captured from missed.
    db.insert_sync(&workload(1_000, 50, now)).await;
    let err = plan::build_plan(&db.client, &db.name, Target::Publishers, &plan_options())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("Recreate the view"), "{err:#}");

    // Documented recovery. Re-applying without truncating is refused.
    db.exec(&format!("DROP VIEW {}.publisher_activity_mv", db.name))
        .await;
    let refused = db.apply(MIGRATION_019).await.unwrap_err();
    assert!(refused.contains("TRUNCATE TABLE"), "{refused}");
    db.exec(&format!("TRUNCATE TABLE {}.publisher_activity", db.name))
        .await;
    db.apply_migration().await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    db.insert_sync(&workload(2_000, 50, now)).await;
    let plan = plan::build_plan(&db.client, &db.name, Target::Publishers, &plan_options())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cp = CheckpointDir::new(dir.path(), Target::Publishers);
    cp.save_plan(&plan, false).unwrap();
    backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap();
    let rows = db.stored().await;
    let now = db.now().await;
    assert_eq!(
        read_publishers(&db.client, 366, None, 1000).await,
        expected_publishers(&rows, now, 366, None, 1000)
    );

    // Recreating the view after planning invalidates the plan.
    db.exec(&format!("DROP VIEW {}.publisher_activity_mv", db.name))
        .await;
    db.exec(&format!("TRUNCATE TABLE {}.publisher_activity", db.name))
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    db.apply_migration().await;
    let err = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("re-run `plan`"), "{err:#}");
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn validator_reports_double_counts_and_duplicate_inflation() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    db.set_merges(false).await;
    let now = db.now().await;
    let events = workload(0, 200, now);
    db.insert_sync(&events).await;
    // A replay of one event: visible as a duplicate row in events_local.
    let replay = events
        .iter()
        .find(|e| {
            !EXCLUDED_PUBLISHER_KINDS.contains(&e.kind)
                && e.created_at > now - 300 * DAY
                && e.created_at < now
        })
        .unwrap()
        .clone();
    db.insert_sync(std::slice::from_ref(&replay)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let options = ValidateOptions {
        targets: vec![Target::Publishers],
        sample: 50,
        boundary_sample: 0,
        sample_permyriad: 10_000,
        cutoff_margin: Duration::from_secs(1),
        ..ValidateOptions::default()
    };
    let report = validate::run(&db.client, &db.name, &options, None)
        .await
        .unwrap();
    assert!(report.passed(), "{}", report.render());
    assert!(
        report.targets[0]
            .rows
            .contains_key(&Outcome::DuplicateInflation)
    );

    // The same double count without a visible duplicate is a failure.
    db.merge_all().await;
    let report = validate::run(&db.client, &db.name, &options, None)
        .await
        .unwrap();
    assert!(!report.passed(), "{}", report.render());
    assert!(
        report.targets[0].rows.contains_key(&Outcome::Excess),
        "{}",
        report.render()
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn missing_rollup_tables_are_reported_as_unknown_tables() {
    let db = TestDb::new().await;
    for sql in [publishers_sql(), longform_sql(false)] {
        let mut q = db.client.query(&sql);
        if sql.contains('?') {
            q = q.bind(30u32).bind(ALL_KINDS).bind(10u32);
        }
        let err = q.fetch_all::<u64>().await.unwrap_err();
        assert!(is_unknown_table(&err), "{err}");
    }
    db.apply_migration().await;
    db.apply(ROLLBACK_019).await.unwrap();
    let err = db
        .client
        .query(&longform_sql(false))
        .fetch_one::<(u64, u64, u64)>()
        .await
        .unwrap_err();
    assert!(is_unknown_table(&err), "{err}");
    db.drop().await;
}
