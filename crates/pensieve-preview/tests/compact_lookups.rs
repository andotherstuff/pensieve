//! Integration tests for the compact preview lookups against a real
//! ClickHouse (24.8, matching production).
//!
//! Run with a disposable server:
//!
//! ```text
//! just ch-test-up
//! just test-clickhouse
//! ```
//!
//! Each test creates its own scratch database containing a production-shaped
//! `events_local` (with the `events_by_kind` projection), the legacy
//! `zap_amounts_data` objects (migrations 003 + 009) and migration 017.
//! Assertions run twice: with merges stopped (so duplicate rows and states
//! are physically present) and after `OPTIMIZE ... FINAL`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clickhouse::Client;
use serde::Serialize;

use pensieve_preview::lookups::backfill::{self, BackfillOptions, StopReason};
use pensieve_preview::lookups::oracle::{self, SourceEvent};
use pensieve_preview::lookups::plan::{self, CheckpointDir, PlanOptions, ShardBy};
use pensieve_preview::lookups::target::Target;
use pensieve_preview::lookups::{CompactLookups, Lookups, bench, compact, validate};
use pensieve_preview::query::{self, ZapTotals};

const IGNORE_REASON: &str = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL";
const BASE_SCHEMA: &str = include_str!("fixtures/base_schema.sql");
const MIGRATION_017: &str =
    include_str!("../../../docs/migrations/prepared/017_preview_compact_lookups.sql");

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
            "pv_test_{nanos}_{}",
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
        db.apply(BASE_SCHEMA).await;
        db
    }

    async fn apply(&self, sql: &str) {
        for statement in statements(sql, &self.name) {
            self.admin
                .query(&statement)
                .execute()
                .await
                .unwrap_or_else(|e| panic!("{e}\n{statement}"));
        }
    }

    async fn apply_migration(&self) {
        self.apply(MIGRATION_017).await;
    }

    async fn exec(&self, sql: &str) {
        self.admin
            .query(sql)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    async fn stop_merges(&self) {
        for table in [
            "events_local",
            "zap_amounts_data",
            "profile_latest",
            "zap_receipts_by_target",
            "addressable_latest",
        ] {
            self.exec(&format!("SYSTEM STOP MERGES {}.{table}", self.name))
                .await;
        }
    }

    async fn merge_all(&self) {
        for table in [
            "events_local",
            "zap_amounts_data",
            "profile_latest",
            "zap_receipts_by_target",
            "addressable_latest",
        ] {
            self.exec(&format!("SYSTEM START MERGES {}.{table}", self.name))
                .await;
            self.exec(&format!("OPTIMIZE TABLE {}.{table} FINAL", self.name))
                .await;
        }
    }

    /// Insert with production async-insert settings (fire and forget), then
    /// flush the queue so the rows are visible.
    async fn insert_async(&self, events: &[SourceEvent]) {
        let client = self
            .client
            .clone()
            .with_option("async_insert", "1")
            .with_option("wait_for_async_insert", "0");
        insert_rows(&client, events).await;
        self.exec("SYSTEM FLUSH ASYNC INSERT QUEUE").await;
    }

    /// Insert synchronously; with merges stopped each call is its own part.
    async fn insert_sync(&self, events: &[SourceEvent]) {
        let client = self.client.clone().with_option("async_insert", "0");
        insert_rows(&client, events).await;
    }

    async fn count(&self, sql: &str) -> u64 {
        self.client
            .query(sql)
            .fetch_one::<u64>()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
    }

    async fn drop(self) {
        self.admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await
            .expect("drop database");
    }
}

/// Split a SQL file into statements, qualify `nostr.` with the scratch
/// database and drop comments.
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
            out.push(statement.replace("nostr.", &format!("{database}.")));
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

async fn insert_rows(client: &Client, events: &[SourceEvent]) {
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

fn hex(n: u64) -> String {
    format!("{n:064x}")
}

fn tags(raw: &[&[&str]]) -> Vec<Vec<String>> {
    raw.iter()
        .map(|t| t.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn event(
    id: u64,
    pubkey: u64,
    created_at: u32,
    kind: u16,
    content: &str,
    t: &[&[&str]],
) -> SourceEvent {
    SourceEvent {
        id: hex(id),
        pubkey: hex(pubkey),
        created_at,
        kind,
        content: content.to_string(),
        tags: tags(t),
    }
}

fn profile(id: u64, pubkey: u64, created_at: u32, name: &str) -> SourceEvent {
    event(
        id,
        pubkey,
        created_at,
        0,
        &format!("{{\"name\":\"{name}\"}}"),
        &[],
    )
}

fn receipt(id: u64, t: &[&[&str]]) -> SourceEvent {
    event(id, 0xbeef, 1_700_000_000, 9735, "", t)
}

fn backfill_options() -> BackfillOptions {
    BackfillOptions {
        delay_between_shards: Duration::ZERO,
        grace: Duration::ZERO,
        min_free_bytes: 0,
        ..BackfillOptions::default()
    }
}

/// Assert every compact lookup matches the oracle over `events`.
async fn assert_matches_oracle(db: &TestDb, events: &[SourceEvent], phase: &str) {
    let c = &db.client;

    let profiles = oracle::latest_profiles(events);
    let pubkeys: Vec<String> = profiles.keys().cloned().collect();
    let batch = compact::fetch_profiles(c, &pubkeys).await.unwrap();
    assert_eq!(batch.len(), profiles.len(), "{phase}: profile batch size");
    for (pubkey, expected) in &profiles {
        let single = compact::fetch_profile(c, pubkey).await.unwrap().unwrap();
        assert_eq!(
            single.event_id, expected.id,
            "{phase}: profile id for {pubkey}"
        );
        assert_eq!(single.content, expected.content, "{phase}: profile content");
        assert_eq!(
            batch[pubkey].event_id, expected.id,
            "{phase}: batch profile id"
        );
        assert_eq!(
            batch[pubkey].content, expected.content,
            "{phase}: batch content"
        );
    }

    let zaps = oracle::zap_totals_by_target(events);
    for (target, expected) in &zaps {
        let got = compact::fetch_zap_totals(c, target).await.unwrap();
        assert_eq!(&got, expected, "{phase}: zap totals for {target}");
    }

    let addressable = oracle::latest_addressable(events);
    for ((pubkey, kind, d), expected) in &addressable {
        let got = compact::fetch_replaceable(c, *kind, pubkey, d)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{phase}: missing {kind}:{pubkey}:{d}"));
        assert_eq!(got.id, expected.id, "{phase}: naddr {kind}:{pubkey}:{d}");
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn migration_creates_pinned_schema_and_is_idempotent() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    // Re-applying is a no-op.
    db.apply_migration().await;

    let columns = db
        .client
        .query(
            "SELECT table, name, type FROM system.columns WHERE database = currentDatabase() \
             AND table IN ('profile_latest', 'zap_receipts_by_target', 'addressable_latest') \
             ORDER BY table, position",
        )
        .fetch_all::<(String, String, String)>()
        .await
        .unwrap();
    let get = |table: &str, name: &str| {
        columns
            .iter()
            .find(|(t, n, _)| t == table && n == name)
            .map(|(_, _, ty)| ty.clone())
            .unwrap_or_else(|| panic!("missing {table}.{name}"))
    };
    assert_eq!(
        get("profile_latest", "latest"),
        "AggregateFunction(argMin, Tuple(String, String, DateTime), Tuple(Int64, String))"
    );
    assert_eq!(
        get("addressable_latest", "latest"),
        "AggregateFunction(argMin, String, Tuple(Int64, String))"
    );
    assert_eq!(get("zap_receipts_by_target", "parser_version"), "UInt32");

    let views = db
        .count(
            "SELECT count() FROM system.tables WHERE database = currentDatabase() \
             AND engine = 'MaterializedView' AND name LIKE '%latest_mv' OR name = 'zap_receipts_by_target_mv'",
        )
        .await;
    assert!(views >= 3);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn profiles_handle_duplicates_out_of_order_and_ties() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    db.stop_merges().await;

    let newer = profile(0x50, 1, 200, "newer");
    let older = profile(0x10, 1, 100, "older");
    let tie_high = profile(0x90, 2, 300, "tie-high-id");
    let tie_low = profile(0x30, 2, 300, "tie-low-id");
    let tie2_low = profile(0x31, 3, 300, "tie2-low-id");
    let tie2_high = profile(0x91, 3, 300, "tie2-high-id");

    // Out of order: newest first. Ties inserted in both orders. Replays.
    let inserts = [
        vec![newer.clone()],
        vec![older.clone()],
        vec![newer.clone()],
        vec![tie_high.clone()],
        vec![tie_low.clone()],
        vec![tie2_low.clone()],
        vec![tie2_high.clone()],
        vec![newer.clone(), older.clone()],
    ];
    for batch in &inserts {
        db.insert_sync(batch).await;
    }
    let all: Vec<SourceEvent> = inserts.concat();

    // Duplicate states are physically present before merges.
    let raw_states = db.count("SELECT count() FROM profile_latest").await;
    assert!(
        raw_states > 3,
        "expected unmerged duplicate states, got {raw_states}"
    );

    for phase in ["before merges", "after merges"] {
        let c = &db.client;
        let p1 = compact::fetch_profile(c, &hex(1)).await.unwrap().unwrap();
        assert_eq!(
            (p1.event_id.as_str(), p1.content.as_str()),
            (newer.id.as_str(), newer.content.as_str()),
            "{phase}"
        );
        let p2 = compact::fetch_profile(c, &hex(2)).await.unwrap().unwrap();
        assert_eq!(p2.event_id, tie_low.id, "{phase}: lowest id wins a tie");
        assert_eq!(
            p2.content, tie_low.content,
            "{phase}: content from the same event"
        );
        let p3 = compact::fetch_profile(c, &hex(3)).await.unwrap().unwrap();
        assert_eq!(p3.event_id, tie2_low.id, "{phase}: tie inserted low-first");
        assert!(compact::fetch_profile(c, &hex(4)).await.unwrap().is_none());

        let batch = compact::fetch_profiles(c, &[hex(1), hex(2), hex(3), hex(4)])
            .await
            .unwrap();
        assert_eq!(batch.len(), 3, "{phase}: missing pubkeys are omitted");
        assert_eq!(batch[&hex(2)].event_id, tie_low.id);

        let names = compact::fetch_display_names(c, &[hex(1), hex(2), hex(4)])
            .await
            .unwrap();
        assert_eq!(names[&hex(1)], "newer");
        assert_eq!(names[&hex(2)], "tie-low-id");
        assert!(!names.contains_key(&hex(4)));

        assert_matches_oracle(&db, &all, phase).await;
        db.merge_all().await;
    }

    assert_eq!(db.count("SELECT count() FROM profile_latest").await, 3);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Zaps
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn zaps_dedupe_replays_and_match_legacy_amount_rules() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    db.stop_merges().await;

    let t1 = hex(0x7001);
    let t2 = hex(0x7002);
    let receipts = vec![
        // 21 sats, duplicate e tag.
        receipt(
            1,
            &[
                &["bolt11", "lnbc210n1abc"],
                &["e", &t1],
                &["e", &t1],
                &["p", "x"],
            ],
        ),
        // 500 sats to two targets.
        receipt(2, &[&["bolt11", "lnbc5u1abc"], &["e", &t1], &["e", &t2]]),
        // Unparseable invoice: not counted.
        receipt(3, &[&["bolt11", "lnbc100x"], &["e", &t1]]),
        // Above 1M sats: counted with amount 0.
        receipt(4, &[&["bolt11", "lnbc11m1abc"], &["e", &t1]]),
        // First bolt11 invalid, second valid: counted, amount 0.
        receipt(
            5,
            &[&["bolt11", "nope"], &["bolt11", "lnbc5u1abc"], &["e", &t2]],
        ),
        // Pico amount.
        receipt(6, &[&["bolt11", "lnbc12345p1abc"], &["e", &t2]]),
        // No targets at all.
        receipt(7, &[&["bolt11", "lnbc5u1abc"], &["e"], &["e", ""]]),
    ];
    // Original inserts, then a full replay and a partial replay in separate parts.
    db.insert_sync(&receipts).await;
    db.insert_sync(&receipts).await;
    db.insert_sync(&receipts[..2]).await;
    let mut all = receipts.clone();
    all.extend_from_slice(&receipts);
    all.extend_from_slice(&receipts[..2]);

    let raw_t1 = db
        .count(&format!(
            "SELECT count() FROM zap_receipts_by_target WHERE target_event_id = '{t1}'"
        ))
        .await;
    assert!(raw_t1 > 4, "expected unmerged replayed rows, got {raw_t1}");

    let expected = oracle::zap_totals_by_target(&all);
    assert_eq!(
        expected[&t1],
        ZapTotals {
            total_msats: 21_000 + 500_000,
            zap_count: 3
        }
    );
    assert_eq!(
        expected[&t2],
        ZapTotals {
            total_msats: 500_000 + 1_234,
            zap_count: 3
        }
    );

    // Legacy double counts replays before merges.
    let legacy_before = query::fetch_zap_totals(&db.client, &t1).await.unwrap();
    assert!(legacy_before.zap_count > expected[&t1].zap_count);

    for phase in ["before merges", "after merges"] {
        assert_matches_oracle(&db, &all, phase).await;
        let none = compact::fetch_zap_totals(&db.client, &hex(0x7003))
            .await
            .unwrap();
        assert_eq!(none, ZapTotals::default());
        db.merge_all().await;
    }

    // Once legacy inputs are merged, legacy and compact agree exactly:
    // same inclusion and amount rules as migration 009.
    for target in [&t1, &t2] {
        let legacy = query::fetch_zap_totals(&db.client, target).await.unwrap();
        let compact = compact::fetch_zap_totals(&db.client, target).await.unwrap();
        assert_eq!(legacy, compact, "legacy parity for {target}");
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn zap_corrections_win_regardless_of_arrival_order() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    db.stop_merges().await;
    let target = hex(0x8001);

    // A corrected parser (version 2) writes rows directly. Receipt 1's v2 row
    // arrives after its v1 row; receipt 2's v2 row arrives before the receipt.
    let r1 = receipt(1, &[&["bolt11", "lnbc210n1abc"], &["e", &target]]);
    let r2 = receipt(2, &[&["bolt11", "lnbc5u1abc"], &["e", &target]]);
    let correction = |receipt_id: &str, amount: u64, parsed: u8| {
        format!(
            "INSERT INTO {}.zap_receipts_by_target VALUES \
             ('{target}', '{receipt_id}', {amount}, {parsed}, 2, toDateTime(1700000000), now())",
            db.name
        )
    };

    db.insert_sync(std::slice::from_ref(&r1)).await;
    assert_eq!(
        compact::fetch_zap_totals(&db.client, &target)
            .await
            .unwrap(),
        ZapTotals {
            total_msats: 21_000,
            zap_count: 1
        }
    );
    db.exec(&correction(&r1.id, 42_000, 1)).await;
    db.exec(&correction(&r2.id, 0, 0)).await;
    db.insert_sync(std::slice::from_ref(&r2)).await;
    // A late amount change in the legacy table has no effect on compact rows.
    db.exec(&format!(
        "INSERT INTO {}.zap_amounts_data VALUES ('{}', '', toDateTime(1800000000), 999999, '', '')",
        db.name, r1.id
    ))
    .await;

    for phase in ["before merges", "after merges"] {
        let got = compact::fetch_zap_totals(&db.client, &target)
            .await
            .unwrap();
        assert_eq!(
            got,
            ZapTotals {
                total_msats: 42_000,
                zap_count: 1
            },
            "{phase}: v2 rows win; v2 marks receipt 2 unparsed"
        );
        db.merge_all().await;
    }
    db.drop().await;
}

// ---------------------------------------------------------------------------
// naddr
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn addressable_latest_handles_ties_and_falls_back_for_uncovered_kinds() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    db.stop_merges().await;

    let author = 0xa11ce;
    let events = vec![
        event(
            0x40,
            author,
            20,
            30023,
            "v2-high",
            &[&["d", "slug"], &["title", "T"]],
        ),
        event(0x10, author, 10, 30023, "v1", &[&["d", "slug"]]),
        event(0x20, author, 20, 30023, "v2-low", &[&["d", "slug"]]),
        event(0x30, author, 5, 30023, "other", &[&["d", "other"]]),
        event(0x50, author, 7, 10002, "relays", &[&["r", "wss://x"]]),
        event(0x60, author, 9, 30023, "no-d", &[&["d"]]),
        // Kinds 1 and 10002 are not covered by addressable_latest.
        event(0x70, author, 9, 1, "note", &[&["d", "n"]]),
    ];
    for e in &events {
        db.insert_sync(std::slice::from_ref(e)).await;
    }
    db.insert_sync(&events).await;

    for phase in ["before merges", "after merges"] {
        let c = &db.client;
        let pk = hex(author);
        let slug = compact::fetch_replaceable(c, 30023, &pk, "slug")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slug.id, hex(0x20), "{phase}: lowest id among newest");
        assert_eq!(slug.content, "v2-low");
        assert_eq!(slug.d_tag, "slug");
        let other = compact::fetch_replaceable(c, 30023, &pk, "other")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(other.id, hex(0x30));
        // Relay lists are not a covered kind: no compact row is stored.
        assert!(
            compact::fetch_replaceable(c, 10002, &pk, "")
                .await
                .unwrap()
                .is_none()
        );
        let no_d = compact::fetch_replaceable(c, 30023, &pk, "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(no_d.id, hex(0x60));
        assert!(
            compact::fetch_replaceable(c, 30023, &pk, "missing")
                .await
                .unwrap()
                .is_none()
        );

        // The dispatcher uses the legacy query for uncovered kinds.
        let lookups = Lookups::new(c.clone(), CompactLookups::ALL);
        let note = lookups
            .fetch_replaceable(1, &pk, "n")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.id, hex(0x70));
        let relays = lookups
            .fetch_replaceable(10002, &pk, "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(relays.id, hex(0x50));
        let via_dispatch = lookups
            .fetch_replaceable(30023, &pk, "slug")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(via_dispatch.id, hex(0x20));

        assert_matches_oracle(&db, &events, phase).await;
        db.merge_all().await;
    }
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Ingest safety
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn async_insert_flush_survives_malformed_events() {
    let db = TestDb::new().await;
    db.apply_migration().await;

    let malformed: Vec<SourceEvent> = vec![
        event(1, 1, 0, 0, "", &[]),
        event(2, 1, u32::MAX, 0, "not json \u{0}", &[&[]]),
        event(
            3,
            2,
            5,
            9735,
            "",
            &[&[], &[""], &["e"], &["e", ""], &["bolt11"]],
        ),
        event(
            4,
            2,
            5,
            9735,
            "",
            &[
                &["bolt11", "lnbc99999999999999999999999999m1x"],
                &["e", "t"],
            ],
        ),
        event(
            5,
            2,
            5,
            9735,
            "",
            &[&["bolt11", "lnbc"], &["bolt11", "LNBC10U1"], &["e", "t"]],
        ),
        event(
            6,
            2,
            5,
            9735,
            "",
            &[&["bolt11", "lnbc18446744073709551615u1x"], &["e", "t"]],
        ),
        event(7, 3, 5, 30023, "", &[&["d"]]),
        event(8, 3, 5, 30023, "", &[]),
        event(9, 3, 5, 10002, "", &[&["d", "\u{1F600}"]]),
        event(10, 3, 5, 39999, "", &[&["d", ""], &["d", "second"]]),
    ];
    // Production inserts synchronously; also cover fire-and-forget async
    // inserts, where a throwing view would drop rows silently.
    db.insert_sync(&malformed).await;
    db.insert_async(&malformed).await;
    db.exec("SYSTEM FLUSH LOGS").await;

    assert_eq!(
        db.count("SELECT uniqExact(id) FROM events_local").await,
        malformed.len() as u64,
        "every row must land despite malformed tags"
    );
    let failed = db
        .count(&format!(
            "SELECT count() FROM system.asynchronous_insert_log \
             WHERE database = '{}' AND status != 'Ok'",
            db.name
        ))
        .await;
    assert_eq!(failed, 0, "no failed async-insert flushes");
    assert_matches_oracle(&db, &malformed, "malformed").await;
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Backfill
// ---------------------------------------------------------------------------

/// A deterministic mixed workload: profiles with ties, receipts with
/// replays, addressable versions.
fn workload(seed: u64, created_base: u32) -> Vec<SourceEvent> {
    let mut events = Vec::new();
    for i in 0..40u64 {
        let pubkey = 0x1000 + i % 9;
        let at = created_base + ((i / 2) as u32) * 3600;
        events.push(profile(
            seed * 1000 + i,
            pubkey,
            at,
            &format!("p{seed}-{i}"),
        ));
        let target = hex(0x2000 + i % 5);
        let invoice = format!("lnbc{}n1x", (i % 7) + 1);
        events.push(event(
            seed * 1000 + 500 + i,
            0xbeef,
            at,
            9735,
            "",
            &[&["bolt11", &invoice], &["e", &target]],
        ));
        let d = format!("d{}", i % 4);
        events.push(event(
            seed * 1000 + 800 + i,
            0x3000 + i % 3,
            at,
            30023,
            "",
            &[&["d", &d]],
        ));
    }
    events
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn backfill_and_live_ingest_overlap_without_gaps_or_double_counts() {
    let db = TestDb::new().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();

    // A: history that predates the views.
    let a = workload(1, 1_600_000_000);
    db.insert_sync(&a).await;

    db.apply_migration().await;
    db.stop_merges().await;

    // B: live traffic after the views exist, including replays of history and
    // events whose created_at falls inside shards the backfill will cover.
    let mut b = workload(2, 1_600_000_000);
    b.extend_from_slice(&a[..30]);
    db.insert_async(&b).await;

    let options = PlanOptions {
        shard_by: ShardBy::CreatedAt,
        max_rows_per_shard: 10,
        id_prefix_len: 2,
        require_projection: false,
    };
    let mut plans = Vec::new();
    for target in Target::ALL {
        let p = plan::build_plan(&db.client, &db.name, target, &options)
            .await
            .unwrap();
        assert!(p.shards.len() > 2, "{target}: expected several shards");
        let dir = CheckpointDir::new(checkpoint_dir.path(), target);
        dir.save_plan(&p, false).unwrap();
        plans.push((p, dir));
    }

    // Run one shard at a time, inserting more live traffic (C) between shards:
    // new events, replays, and events landing in already-completed shards.
    let mut c_events = Vec::new();
    let one_shard = BackfillOptions {
        max_shards: Some(1),
        ..backfill_options()
    };
    let mut round = 0u64;
    loop {
        let mut complete = true;
        for (p, dir) in &plans {
            let report = backfill::run_backfill(&db.client, p, dir, &one_shard)
                .await
                .unwrap();
            if report.stopped != StopReason::Complete {
                complete = false;
            }
        }
        round += 1;
        let mut live = workload(10 + round, 1_600_000_000 + (round as u32 % 3) * 7200);
        live.truncate(9);
        live.extend_from_slice(&a[(round as usize % 20)..(round as usize % 20) + 3]);
        db.insert_async(&live).await;
        c_events.extend(live);
        if complete {
            break;
        }
        assert!(round < 200, "backfill did not finish");
    }

    let mut all = a.clone();
    all.extend(b.clone());
    all.extend(c_events.clone());
    assert_matches_oracle(&db, &all, "after interleaved backfill (before merges)").await;

    // Re-running is a no-op; a full second backfill (replan) is idempotent;
    // the gap-closure pass is harmless.
    for (p, dir) in &plans {
        let again = backfill::run_backfill(&db.client, p, dir, &backfill_options())
            .await
            .unwrap();
        assert_eq!(again.shards_run, 0);
        assert_eq!(again.stopped, StopReason::Complete);
        dir.save_plan(p, true).unwrap();
        let full = backfill::run_backfill(&db.client, p, dir, &backfill_options())
            .await
            .unwrap();
        assert_eq!(full.shards_run, p.shards.len());
        let gap = backfill::run_gap_pass(
            &db.client,
            p,
            dir,
            Duration::from_secs(3600),
            &backfill_options(),
        )
        .await
        .unwrap();
        assert_eq!(gap, StopReason::Complete);
    }
    assert_matches_oracle(&db, &all, "after repeated backfill (before merges)").await;

    db.merge_all().await;
    assert_matches_oracle(&db, &all, "after merges").await;
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn backfill_and_materialized_views_produce_identical_rows() {
    let db = TestDb::new().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();
    let mut events = workload(3, 1_650_000_000);
    events.extend(workload(4, 1_650_000_000));
    events.push(receipt(
        9_001,
        &[
            &["bolt11", "nope"],
            &["bolt11", "lnbc7u1x"],
            &["e", &hex(0x2001)],
        ],
    ));
    events.push(event(
        9_002,
        0x3001,
        1_650_000_000,
        30023,
        "",
        &[&["d"], &["d", "x"]],
    ));

    // Path 1: backfill only.
    db.insert_sync(&events).await;
    db.apply_migration().await;
    for target in Target::ALL {
        let options = PlanOptions {
            shard_by: ShardBy::CreatedAt,
            max_rows_per_shard: 25,
            id_prefix_len: 2,
            require_projection: false,
        };
        let p = plan::build_plan(&db.client, &db.name, target, &options)
            .await
            .unwrap();
        let dir = CheckpointDir::new(checkpoint_dir.path(), target);
        dir.save_plan(&p, false).unwrap();
        let report = backfill::run_backfill(&db.client, &p, &dir, &backfill_options())
            .await
            .unwrap();
        assert_eq!(report.stopped, StopReason::Complete);
    }
    assert_matches_oracle(&db, &events, "backfill path").await;
    let snapshot = |db: &TestDb| {
        let client = db.client.clone();
        async move {
            let profiles = client
                .query("SELECT pubkey, toString(argMinMerge(latest)) FROM profile_latest GROUP BY pubkey ORDER BY pubkey")
                .fetch_all::<(String, String)>()
                .await
                .unwrap();
            let zaps = client
                .query(
                    "SELECT target_event_id, receipt_id, argMax(amount_msats, parser_version), \
                     argMax(amount_parsed, parser_version) FROM zap_receipts_by_target \
                     GROUP BY target_event_id, receipt_id ORDER BY 1, 2",
                )
                .fetch_all::<(String, String, u64, u8)>()
                .await
                .unwrap();
            let addressable = client
                .query(
                    "SELECT pubkey, kind, d_tag, argMinMerge(latest) FROM addressable_latest \
                     GROUP BY pubkey, kind, d_tag ORDER BY 1, 2, 3",
                )
                .fetch_all::<(String, u16, String, String)>()
                .await
                .unwrap();
            (profiles, zaps, addressable)
        }
    };
    let via_backfill = snapshot(&db).await;

    // Path 2: the same events through the materialized views.
    for target in Target::ALL {
        db.exec(&format!("TRUNCATE TABLE {}.{}", db.name, target.table()))
            .await;
    }
    db.insert_async(&events).await;
    let via_views = snapshot(&db).await;

    assert_eq!(via_backfill, via_views);
    db.drop().await;
}

/// Bulk synthetic history large enough for the optimizer to choose
/// `events_by_kind`, as in production.
async fn insert_bulk(db: &TestDb, rows: u64) {
    db.exec(&format!(
        "INSERT INTO {}.events_local (id, pubkey, created_at, kind, content, sig, tags) \
         SELECT lower(hex(SHA256(toString(number)))) AS id, \
         lower(hex(SHA256(toString(number % 3000)))) AS pubkey, \
         toDateTime(1600000000 + intDiv(number, 6000) * 600) AS created_at, \
         [0, 0, 1, 1, 7, 3, 9735, 9735, 30023, 10002, 6, 1][number % 12 + 1] AS kind, \
         concat('{{\"name\":\"u', toString(number), '\"}}') AS content, \
         '' AS sig, \
         multiIf( \
           kind = 9735, [['e', lower(hex(SHA256(toString(number % 400))))], \
                         ['bolt11', concat('lnbc', toString(number % 900 + 1), 'n1x')]], \
           kind = 30023, [['d', concat('slug', toString(number % 7))]], \
           [['e', lower(hex(SHA256(toString(number % 777))))]]) AS tags \
         FROM numbers({rows}) SETTINGS async_insert = 0",
        db.name
    ))
    .await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn projection_backfill_validate_and_bench_end_to_end() {
    let db = TestDb::new().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();
    insert_bulk(&db, 600_000).await;
    db.apply_migration().await;

    let options = PlanOptions {
        shard_by: ShardBy::CreatedAt,
        max_rows_per_shard: 20_000,
        id_prefix_len: 2,
        require_projection: true,
    };
    for target in Target::ALL {
        let p = plan::build_plan(&db.client, &db.name, target, &options)
            .await
            .unwrap();
        assert!(p.shards.len() > 1);
        assert!(p.estimate.peak_bytes >= p.estimate.pre_merge_bytes);
        let dir = CheckpointDir::new(checkpoint_dir.path(), target);
        dir.save_plan(&p, false).unwrap();

        // Interrupted run, then resume.
        let first = backfill::run_backfill(
            &db.client,
            &p,
            &dir,
            &BackfillOptions {
                max_shards: Some(1),
                ..backfill_options()
            },
        )
        .await
        .unwrap();
        assert_eq!(first.stopped, StopReason::MaxShards);
        let rest = backfill::run_backfill(&db.client, &p, &dir, &backfill_options())
            .await
            .unwrap();
        assert_eq!(rest.stopped, StopReason::Complete);

        let projection = format!("{}.events_local.events_by_kind", db.name);
        for checkpoint in dir.completed_shards().unwrap() {
            let log = checkpoint.query_log.expect("query_log entry");
            assert!(
                log.projections.contains(&projection),
                "{target}: {:?}",
                log.projections
            );
        }
    }

    // Profiles: compact equals an independent SQL reference over all rows.
    let mismatches = db
        .count(
            "SELECT count() FROM ( \
                 SELECT pubkey, argMin((id, content), (-toInt64(created_at), id)) AS r \
                 FROM events_local WHERE kind = 0 GROUP BY pubkey \
             ) AS ref \
             FULL OUTER JOIN ( \
                 SELECT pubkey, argMinMerge(latest) AS w FROM profile_latest GROUP BY pubkey \
             ) AS c USING pubkey \
             WHERE tupleElement(ref.r, 1) != tupleElement(c.w, 1) \
             OR tupleElement(ref.r, 2) != tupleElement(c.w, 2)",
        )
        .await;
    assert_eq!(mismatches, 0);

    let report = validate::run(
        &db.client,
        &db.name,
        &validate::ValidateOptions {
            sample: 40,
            hot: 5,
            sample_permyriad: 2_000,
            cutoff_margin: Duration::ZERO,
            batch: 25,
            legacy_sample: 10,
            ..validate::ValidateOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(report.unexplained(), 0, "{}", report.render());
    assert!(report.passed(), "{}", report.render());
    for t in &report.targets {
        assert!(t.sampled > 0, "{}: no keys sampled", t.target);
    }
    // The synthetic data has created_at ties per pubkey, which legacy breaks
    // arbitrarily.
    let profiles = &report.targets[0];
    assert!(
        profiles
            .compact
            .get(&validate::Outcome::Match)
            .copied()
            .unwrap_or(0)
            > 0
    );

    let bench = bench::run(
        &db.client,
        &db.name,
        &bench::BenchOptions {
            samples: 3,
            batch_size: 5,
            include_legacy: true,
            legacy_samples: 1,
            ..bench::BenchOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(
        bench.measurements.iter().all(|m| m.error.is_none()),
        "{}",
        bench.render()
    );
    assert!(bench.measurements.iter().any(|m| m.server_ms.is_some()));
    assert!(
        bench
            .storage
            .projections
            .iter()
            .any(|p| p.name == "events_by_kind")
    );
    let lookups: HashMap<&str, usize> =
        bench.summaries.iter().map(|s| (s.lookup, s.runs)).collect();
    for name in [
        "profile",
        "profiles_batch",
        "display_names",
        "zaps",
        "naddr",
    ] {
        assert!(lookups.contains_key(name), "bench missing {name}");
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn gap_pass_honours_runtime_disk_and_backlog_guards() {
    let db = TestDb::new().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();
    db.insert_sync(&workload(5, 1_600_000_000)).await;
    db.apply_migration().await;

    let options = PlanOptions {
        shard_by: ShardBy::CreatedAt,
        max_rows_per_shard: 1_000,
        id_prefix_len: 2,
        require_projection: false,
    };
    let p = plan::build_plan(&db.client, &db.name, Target::Profiles, &options)
        .await
        .unwrap();
    let dir = CheckpointDir::new(checkpoint_dir.path(), Target::Profiles);
    dir.save_plan(&p, false).unwrap();
    let margin = Duration::from_secs(3600);

    let no_runtime = BackfillOptions {
        max_runtime: Some(Duration::ZERO),
        ..backfill_options()
    };
    let stopped = backfill::run_gap_pass(&db.client, &p, &dir, margin, &no_runtime)
        .await
        .unwrap();
    assert_eq!(stopped, StopReason::MaxRuntime);

    let no_disk = BackfillOptions {
        min_free_bytes: u64::MAX,
        ..backfill_options()
    };
    let stopped = backfill::run_gap_pass(&db.client, &p, &dir, margin, &no_disk)
        .await
        .unwrap();
    assert!(matches!(stopped, StopReason::DiskGuard(_)), "{stopped:?}");

    assert_eq!(db.count("SELECT count() FROM profile_latest").await, 0);

    // A live insert gives the target one part, which exceeds a zero-part limit.
    db.insert_sync(&[profile(0x9999, 0x9999, 1, "live")]).await;
    let backlog = BackfillOptions {
        max_target_parts: 0,
        parts_wait_timeout: Duration::ZERO,
        ..backfill_options()
    };
    let stopped = backfill::run_gap_pass(&db.client, &p, &dir, margin, &backlog)
        .await
        .unwrap();
    assert!(
        matches!(stopped, StopReason::PartsBacklog(_)),
        "{stopped:?}"
    );

    assert!(
        !dir.gap_pass_path().exists(),
        "refused passes must not checkpoint"
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn validation_rechecks_keys_changed_during_reads() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    let author = 0x5150;
    let target = hex(0x6001);
    let quiet_target = hex(0x6002);
    db.insert_sync(&[
        profile(0x10, author, 100, "v1"),
        profile(0x11, author + 1, 100, "quiet"),
        receipt(0x20, &[&["bolt11", "lnbc1u1x"], &["e", &target]]),
        receipt(0x21, &[&["bolt11", "lnbc1u1x"], &["e", &quiet_target]]),
        event(0x30, author, 100, 30023, "", &[&["d", "slug"]]),
        event(0x31, author, 100, 30023, "", &[&["d", "quiet"]]),
    ])
    .await;

    // The validator's cutoff and source read happen here...
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let cutoff: u32 = db
        .client
        .query("SELECT toUInt32(now())")
        .fetch_one()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    // ...then new winners land before the compact reads.
    db.insert_sync(&[
        profile(0x12, author, 200, "v2"),
        receipt(0x22, &[&["bolt11", "lnbc2u1x"], &["e", &target]]),
        event(0x32, author, 200, 30023, "", &[&["d", "slug"]]),
    ])
    .await;

    let pubkeys = [hex(author), hex(author + 1)];
    let changed = validate::profiles_changed_after(&db.client, &db.name, &pubkeys, cutoff)
        .await
        .unwrap();
    assert_eq!(changed.into_iter().collect::<Vec<_>>(), vec![hex(author)]);

    let targets = [target.clone(), quiet_target.clone()];
    let changed = validate::zap_targets_changed_after(&db.client, &db.name, &targets, cutoff)
        .await
        .unwrap();
    assert_eq!(changed.into_iter().collect::<Vec<_>>(), vec![target]);

    let coordinates = [
        (hex(author), 30023u16, "slug".to_string()),
        (hex(author), 30023u16, "quiet".to_string()),
    ];
    let changed = validate::coordinates_changed_after(&db.client, &db.name, &coordinates, cutoff)
        .await
        .unwrap();
    assert_eq!(
        changed.into_iter().collect::<Vec<_>>(),
        vec![(hex(author), 30023u16, "slug".to_string())]
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn validation_with_every_key_skipped_is_inconclusive() {
    let db = TestDb::new().await;
    db.apply_migration().await;
    // Everything was just indexed, so a 5-minute cutoff excludes every key.
    db.insert_async(&workload(6, 1_600_000_000)).await;

    let report = validate::run(
        &db.client,
        &db.name,
        &validate::ValidateOptions {
            sample: 20,
            hot: 5,
            sample_permyriad: 10_000,
            require_projection: false,
            ..validate::ValidateOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(report.unexplained(), 0);
    for t in &report.targets {
        assert!(t.sampled > 0 && t.compared() == 0, "{}", report.render());
    }
    assert_eq!(report.inconclusive_targets().len(), 3);
    assert!(!report.passed(), "{}", report.render());
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn validation_with_empty_sample_of_nonempty_source_is_inconclusive() {
    let db = TestDb::new().await;
    db.insert_sync(&workload(7, 1_600_000_000)).await;
    db.apply_migration().await;

    // Sampling disabled entirely is rejected.
    let none = validate::ValidateOptions {
        sample: 0,
        hot: 0,
        require_projection: false,
        ..validate::ValidateOptions::default()
    };
    assert!(validate::run(&db.client, &db.name, &none).await.is_err());

    // A random-only sample that selects no keys cannot pass on a non-empty
    // source, even though the compact tables were never backfilled.
    let empty_fraction = validate::ValidateOptions {
        sample: 20,
        hot: 0,
        sample_permyriad: 0,
        cutoff_margin: Duration::ZERO,
        require_projection: false,
        ..validate::ValidateOptions::default()
    };
    let report = validate::run(&db.client, &db.name, &empty_fraction)
        .await
        .unwrap();
    for t in &report.targets {
        assert_eq!(t.sampled, 0);
        assert!(t.source_has_keys, "{}", report.render());
    }
    assert_eq!(report.inconclusive_targets().len(), 3);
    assert!(!report.passed(), "{}", report.render());
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn session_deadline_expiring_during_merge_wait_starts_no_insert() {
    let db = TestDb::new().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();
    db.insert_sync(&workload(8, 1_600_000_000)).await;
    db.apply_migration().await;
    // One live insert gives the target a part, so a zero-part limit waits.
    db.insert_sync(&[profile(0x9998, 0x9998, 1, "live")]).await;

    let options = PlanOptions {
        shard_by: ShardBy::CreatedAt,
        max_rows_per_shard: 1_000,
        id_prefix_len: 2,
        require_projection: false,
    };
    let p = plan::build_plan(&db.client, &db.name, Target::Profiles, &options)
        .await
        .unwrap();
    let dir = CheckpointDir::new(checkpoint_dir.path(), Target::Profiles);
    dir.save_plan(&p, false).unwrap();
    let rows_before = db.count("SELECT count() FROM profile_latest").await;

    // A positive budget that runs out while waiting for merges (the parts
    // wait alone would allow an hour).
    let waiting = BackfillOptions {
        max_runtime: Some(Duration::from_secs(2)),
        max_target_parts: 0,
        parts_wait_timeout: Duration::from_secs(3600),
        ..backfill_options()
    };
    let started = std::time::Instant::now();
    let report = backfill::run_backfill(&db.client, &p, &dir, &waiting)
        .await
        .unwrap();
    assert_eq!(report.stopped, StopReason::MaxRuntime);
    assert_eq!(report.shards_run, 0);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "wait was not bounded by the deadline"
    );

    let stopped = backfill::run_gap_pass(&db.client, &p, &dir, Duration::from_secs(3600), &waiting)
        .await
        .unwrap();
    assert_eq!(stopped, StopReason::MaxRuntime);

    assert!(dir.completed_shards().unwrap().is_empty());
    assert!(!dir.gap_pass_path().exists());
    assert_eq!(
        db.count("SELECT count() FROM profile_latest").await,
        rows_before,
        "no INSERT may start after the deadline"
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn projections_off_rehearsal_applies_to_every_preview_query() {
    use pensieve_preview::{AppState, Config};

    let db = TestDb::new().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();
    insert_bulk(&db, 200_000).await;
    db.apply_migration().await;
    for target in Target::ALL {
        let options = PlanOptions {
            shard_by: ShardBy::CreatedAt,
            max_rows_per_shard: 50_000,
            id_prefix_len: 2,
            require_projection: false,
        };
        let p = plan::build_plan(&db.client, &db.name, target, &options)
            .await
            .unwrap();
        let dir = CheckpointDir::new(checkpoint_dir.path(), target);
        dir.save_plan(&p, false).unwrap();
        let report = backfill::run_backfill(&db.client, &p, &dir, &backfill_options())
            .await
            .unwrap();
        assert_eq!(report.stopped, StopReason::Complete);
    }

    let state_for = |compact_lookups| {
        AppState::new(Config {
            bind_addr: String::new(),
            clickhouse_url: std::env::var("PENSIEVE_TEST_CLICKHOUSE_URL").unwrap(),
            clickhouse_database: db.name.clone(),
            base_url: String::new(),
            site_name: String::new(),
            vip_pubkeys: Default::default(),
            compact_lookups,
            disable_projections: true,
        })
    };

    let pubkey: String = db
        .client
        .query("SELECT pubkey FROM profile_latest LIMIT 1")
        .fetch_one()
        .await
        .unwrap();
    let zap_target: String = db
        .client
        .query("SELECT target_event_id FROM zap_receipts_by_target LIMIT 1")
        .fetch_one()
        .await
        .unwrap();
    let (author, kind, d): (String, u16, String) = db
        .client
        .query("SELECT pubkey, kind, d_tag FROM addressable_latest LIMIT 1")
        .fetch_one()
        .await
        .unwrap();

    // Full rehearsal: every lookup on the compact path, projections off.
    let state = state_for(CompactLookups::ALL);
    assert!(
        state
            .lookups
            .fetch_profile(&pubkey)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        state
            .lookups
            .fetch_profiles(std::slice::from_ref(&pubkey))
            .await
            .unwrap()
            .len(),
        1
    );
    state
        .lookups
        .fetch_display_names(std::slice::from_ref(&pubkey))
        .await
        .unwrap();
    // Engagement also reads the reaction/comment/repost tables, which this
    // fixture lacks; zap totals are covered by the compact zap read.
    let naddr = state
        .lookups
        .fetch_replaceable(kind, &author, &d)
        .await
        .unwrap()
        .expect("naddr row");
    assert!(
        state
            .lookups
            .fetch_event_by_id(&naddr.id)
            .await
            .unwrap()
            .is_some()
    );
    let zaps = compact::fetch_zap_totals(&state.clickhouse, &zap_target)
        .await
        .unwrap();
    assert!(zaps.zap_count > 0);

    // A legacy lookup under the rehearsal is bounded rather than scanning
    // with the projection.
    let legacy = state_for(CompactLookups::NONE);
    legacy.lookups.fetch_profile(&pubkey).await.unwrap();

    #[derive(Debug, serde::Deserialize, clickhouse::Row)]
    struct LoggedQuery {
        log_comment: String,
        projections: Vec<String>,
        use_projections: String,
        max_time: String,
    }
    db.exec("SYSTEM FLUSH LOGS").await;
    let rows = db
        .admin
        .query(
            "SELECT log_comment, projections, \
             Settings['optimize_use_projections'] AS use_projections, \
             Settings['max_execution_time'] AS max_time \
             FROM system.query_log \
             WHERE event_date >= yesterday() AND type = 'QueryFinish' \
             AND current_database = ? \
             AND (startsWith(log_comment, 'pensieve-preview:') \
                  OR query LIKE '%FROM zap_receipts_by_target%WHERE target_event_id%')",
        )
        .bind(&db.name)
        .fetch_all::<LoggedQuery>()
        .await
        .unwrap();
    let comments: Vec<&str> = rows.iter().map(|r| r.log_comment.as_str()).collect();
    for expected in [
        "pensieve-preview:profile:compact",
        "pensieve-preview:profiles:compact",
        "pensieve-preview:display_names:compact",
        "pensieve-preview:replaceable:compact",
        "pensieve-preview:event:legacy",
        "pensieve-preview:profile:legacy",
    ] {
        assert!(
            comments.contains(&expected),
            "missing {expected}: {comments:?}"
        );
    }
    assert!(
        rows.iter().any(|r| r.log_comment.is_empty()),
        "compact zap read not logged"
    );
    for r in &rows {
        let comment = &r.log_comment;
        assert_eq!(
            r.use_projections, "0",
            "{comment} ran with projections enabled"
        );
        assert!(
            r.projections.is_empty(),
            "{comment} used {:?}",
            r.projections
        );
        assert!(!r.max_time.is_empty(), "{comment} has no execution cap");
    }
    db.drop().await;
}
