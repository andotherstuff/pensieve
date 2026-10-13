//! Integration tests for migration 021 (`kind_activity`) and the exact
//! distinct-pubkeys validation of `kind_totals`, against a real ClickHouse
//! (24.8, matching production).
//!
//! Run with a disposable server:
//!
//! ```text
//! just ch-test-up
//! just test-clickhouse
//! ```
//!
//! Each test creates a scratch database with a production-shaped
//! `events_local` (no projections), the pre-020 derived views and migration
//! 020 phase 1 (`kind_totals`), then applies migration 021. Results are
//! compared with independent Rust oracles over the stored rows and with the
//! queries the endpoints ran before.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Datelike, NaiveDate};
use clickhouse::Client;
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

use pensieve_serve::rollups::backfill::{self, BackfillOptions, StopReason};
use pensieve_serve::rollups::kind_activity::{
    HOUR_ROWS_DAYS, SeriesGrain, average_content_length, backfill_select_sql, kind_detail_sql,
    kind_series_sql,
};
use pensieve_serve::rollups::plan::{self, CheckpointDir, PlanOptions};
use pensieve_serve::rollups::validate::{self, Outcome, ValidateInputs, ValidateOptions};
use pensieve_serve::rollups::{Target, is_unknown_table};
use pensieve_serve::{AppState, Config, router};

const IGNORE_REASON: &str = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL";
const BASE_SCHEMA: &str = include_str!("fixtures/events_local.sql");
const PRE_020: &str = include_str!("fixtures/pre020_derived_views.sql");
const MIGRATION_020: &str =
    include_str!("../../../docs/migrations/prepared/020_incremental_derived_views.sql");
const MIGRATION_021: &str = include_str!("../../../docs/migrations/prepared/021_kind_activity.sql");
const ROLLBACK_021: &str =
    include_str!("../../../docs/migrations/rollback/021_drop_kind_activity.sql");
const DAY: u32 = 86_400;
const TOKEN: &str = "kind-activity-test-token";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestDb {
    url: String,
    name: String,
    admin: Client,
    client: Client,
}

impl TestDb {
    /// A scratch database with `events_local`, the pre-020 views and
    /// migration 020 phase 1 (so `kind_totals` sees every insert).
    async fn new() -> Self {
        let url = std::env::var("PENSIEVE_TEST_CLICKHOUSE_URL")
            .unwrap_or_else(|_| panic!("{IGNORE_REASON}"));
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "kind_activity_{nanos}_{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = Client::default()
            .with_url(&url)
            .with_option("allow_experimental_refreshable_materialized_view", "1");
        admin
            .query(&format!("CREATE DATABASE {name}"))
            .execute()
            .await
            .expect("create database");
        let client = Client::default().with_url(&url).with_database(&name);
        let db = Self {
            url,
            name,
            admin,
            client,
        };
        db.apply(BASE_SCHEMA).await.expect("base schema");
        db.apply(PRE_020).await.expect("pre-020 views");
        db.apply(MIGRATION_020).await.expect("migration 020");
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

    async fn exec(&self, sql: &str) {
        self.admin
            .query(sql)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    async fn set_merges(&self, table: &str, start: bool) {
        let verb = if start { "START" } else { "STOP" };
        self.exec(&format!("SYSTEM {verb} MERGES {}.{table}", self.name))
            .await;
    }

    async fn optimize(&self, table: &str) {
        self.set_merges(table, true).await;
        self.exec(&format!("OPTIMIZE TABLE {}.{table} FINAL", self.name))
            .await;
    }

    /// One insert with the server's `indexed_at` (an ingest flush now).
    async fn insert(&self, events: &[Ev]) {
        insert_into(&self.client, events, None).await;
    }

    /// One insert with an explicit `indexed_at` (history).
    async fn insert_at(&self, at: u32, events: &[Ev]) {
        insert_into(&self.client, events, Some(at)).await;
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
                "SELECT kind, pubkey, length(content), toUInt32(created_at), \
                 toUInt32(indexed_at) FROM events_local",
            )
            .fetch_all()
            .await
            .unwrap()
    }

    /// The API, with an empty response cache, over this database.
    async fn api(&self) -> axum::Router {
        let config = Config {
            bind_addr: "127.0.0.1:0".into(),
            clickhouse_url: self.url.clone(),
            clickhouse_database: self.name.clone(),
            latest_event_watermark_path: None,
            latest_event_watermark_max_age_secs: 0,
            api_tokens: [TOKEN.to_string()].into_iter().collect(),
            relay_db_path: None,
        };
        router(AppState::new(config).await.unwrap())
    }

    async fn drop(self) {
        self.admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await
            .expect("drop database");
    }
}

async fn insert_into(client: &Client, events: &[Ev], at: Option<u32>) {
    let client = client.clone().with_option("async_insert", "0");
    match at {
        Some(at) => {
            let mut insert = client.insert("events_local").expect("insert");
            for e in events {
                insert.write(&e.row_at(at)).await.expect("write row");
            }
            insert.end().await.expect("end insert");
        }
        None => {
            let mut insert = client.insert("events_local").expect("insert");
            for e in events {
                insert.write(&e.row()).await.expect("write row");
            }
            insert.end().await.expect("end insert");
        }
    }
}

/// Split a SQL file into statements, point `nostr` at the scratch database
/// and drop comments.
fn statements(sql: &str, database: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in sql.lines() {
        let code = line.find("--").map_or(line, |i| &line[..i]);
        if code.trim().is_empty() {
            continue;
        }
        current.push_str(code);
        current.push('\n');
        if code.trim_end().ends_with(';') {
            let statement = current.trim().trim_end_matches(';').to_string();
            if !statement.starts_with("SET ") {
                out.push(
                    statement
                        .replace("nostr.", &format!("{database}."))
                        .replace("database = 'nostr'", &format!("database = '{database}'")),
                );
            }
            current.clear();
        }
    }
    assert!(
        current.trim().is_empty(),
        "unterminated statement: {current}"
    );
    out
}

#[derive(Debug, Clone)]
struct Ev {
    id: String,
    pubkey: String,
    created_at: u32,
    kind: u16,
    content: String,
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

#[derive(Serialize, clickhouse::Row)]
struct InsertRowAt<'a> {
    id: &'a str,
    pubkey: &'a str,
    created_at: u32,
    kind: u16,
    content: &'a str,
    sig: &'a str,
    tags: &'a [Vec<String>],
    indexed_at: u32,
    relay_source: &'a str,
}

impl Ev {
    fn row(&self) -> InsertRow<'_> {
        InsertRow {
            id: &self.id,
            pubkey: &self.pubkey,
            created_at: self.created_at,
            kind: self.kind,
            content: &self.content,
            sig: "",
            tags: &[],
            relay_source: "",
        }
    }

    fn row_at(&self, indexed_at: u32) -> InsertRowAt<'_> {
        InsertRowAt {
            id: &self.id,
            pubkey: &self.pubkey,
            created_at: self.created_at,
            kind: self.kind,
            content: &self.content,
            sig: "",
            tags: &[],
            indexed_at,
            relay_source: "",
        }
    }
}

#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct Stored {
    kind: u16,
    pubkey: String,
    content_length: u64,
    created_at: u32,
    indexed_at: u32,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn event_id(n: u64) -> String {
    format!("{:016x}{n:048x}", splitmix(n))
}

fn pubkey(n: u64) -> String {
    format!("{:016x}{n:048x}", splitmix(n ^ 0xfeed))
}

fn ev(n: u64, author: u64, kind: u16, created_at: u32, content_len: usize) -> Ev {
    Ev {
        id: event_id(n),
        pubkey: pubkey(author),
        created_at,
        kind,
        content: "x".repeat(content_len),
    }
}

fn floor_hour(t: u32) -> u32 {
    t / 3600 * 3600
}

/// Mixed kinds, authors, content lengths and timestamps: recent hours on
/// both sides of the 30-day edge, months and years old, future-dated, and
/// near the DateTime maximum. Nothing is dated later today (the activity
/// series counts a period once it starts, the old query only once
/// `created_at <= now()`).
fn workload(first_id: u64, count: u64, now: u32) -> Vec<Ev> {
    const KINDS: [u16; 8] = [1, 1, 7, 3, 0, 20001, 30023, 1059];
    let today = now / DAY * DAY;
    (first_id..first_id + count)
        .map(|n| {
            let r = splitmix(n ^ 0x21);
            let created_at = match r % 13 {
                0 => now + 50 * DAY + (r >> 40) as u32 % DAY,
                1 => now - 2000 * DAY + (r >> 40) as u32 % DAY,
                2 => u32::MAX - (r >> 40) as u32 % 1000,
                3 => now - 400 * DAY + (r >> 40) as u32 % (300 * DAY),
                // Earlier today (never in the future).
                4 => today + (r >> 40) as u32 % (now - today + 1),
                _ => now - (r >> 20) as u32 % (32 * DAY),
            };
            ev(
                n,
                (r >> 8) % 23,
                KINDS[(r >> 16) as usize % KINDS.len()],
                created_at,
                (r >> 32) as usize % 300,
            )
        })
        .collect()
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

/// Plan and run a complete backfill of `target`.
async fn backfill_target(db: &TestDb, dir: &std::path::Path, target: Target) -> plan::Plan {
    let options = PlanOptions {
        id_prefix_len: 1,
        boundary_margin_secs: 5,
    };
    let plan = plan::build_plan(&db.client, &db.name, target, &options)
        .await
        .unwrap();
    let cp = CheckpointDir::new(dir, target);
    cp.save_plan(&plan, false).unwrap();
    let report = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!(report.stopped, StopReason::Complete, "{target}");
    plan
}

fn validate_options(targets: &[Target]) -> ValidateOptions {
    ValidateOptions {
        targets: targets.to_vec(),
        cutoff_margin: Duration::from_secs(1),
        pubkeys_kinds: 6,
        content_budget_events: 1_000_000,
        ..ValidateOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Oracles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize, clickhouse::Row)]
struct DetailRow {
    kind: u16,
    event_count: u64,
    unique_pubkeys: u64,
    first_seen: u32,
    last_seen: u32,
    events_all: u64,
    content_bytes: u64,
    events_24h: u64,
    events_7d: u64,
    events_30d: u64,
}

/// `/kinds/{kind}` at server time `now`: windows from the UTC hour
/// containing `now - N days`, no upper bound.
fn expected_detail(rows: &[Stored], kind: u16, now: u32) -> Option<DetailRow> {
    let rows: Vec<&Stored> = rows.iter().filter(|r| r.kind == kind).collect();
    if rows.is_empty() {
        return None;
    }
    let since = |days: u32| floor_hour(now - days * DAY);
    let count_since =
        |days: u32| rows.iter().filter(|r| r.created_at >= since(days)).count() as u64;
    Some(DetailRow {
        kind,
        event_count: rows.len() as u64,
        unique_pubkeys: rows
            .iter()
            .map(|r| r.pubkey.as_str())
            .collect::<BTreeSet<_>>()
            .len() as u64,
        first_seen: rows.iter().map(|r| r.created_at).min().unwrap(),
        last_seen: rows.iter().map(|r| r.created_at).max().unwrap(),
        events_all: rows.len() as u64,
        content_bytes: rows.iter().map(|r| r.content_length).sum(),
        events_24h: count_since(1),
        events_7d: count_since(7),
        events_30d: count_since(30),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
struct SeriesRow {
    period: String,
    event_count: u64,
    unique_pubkeys: u64,
}

fn period_of(created_at: u32, grain: SeriesGrain) -> NaiveDate {
    let day = DateTime::from_timestamp(i64::from(created_at), 0)
        .unwrap()
        .date_naive();
    match grain {
        SeriesGrain::Day => day,
        SeriesGrain::Week => {
            day - chrono::Days::new(u64::from(day.weekday().num_days_from_monday()))
        }
        SeriesGrain::Month => day.with_day(1).unwrap(),
    }
}

/// `/kinds/{kind}/activity`: the `limit` most recent periods up to the
/// current one, exact distinct pubkeys.
fn expected_series(
    rows: &[Stored],
    kind: u16,
    grain: SeriesGrain,
    limit: u32,
    now: u32,
) -> Vec<SeriesRow> {
    let current = period_of(now, grain);
    let first = match grain {
        SeriesGrain::Day => current - chrono::Days::new(u64::from(limit.max(1) - 1)),
        SeriesGrain::Week => current - chrono::Days::new(7 * u64::from(limit.max(1) - 1)),
        SeriesGrain::Month => current - chrono::Months::new(limit.max(1) - 1),
    };
    let mut periods: BTreeMap<NaiveDate, (u64, BTreeSet<&str>)> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.kind == kind) {
        let p = period_of(r.created_at, grain);
        if p >= first && p <= current {
            let e = periods.entry(p).or_default();
            e.0 += 1;
            e.1.insert(&r.pubkey);
        }
    }
    periods
        .into_iter()
        .rev()
        .take(limit as usize)
        .map(|(p, (n, keys))| SeriesRow {
            period: p.to_string(),
            event_count: n,
            unique_pubkeys: keys.len() as u64,
        })
        .collect()
}

/// Read `/kinds/{kind}` rows for `kinds`, retrying when the UTC hour of a
/// window start changes during the read. Returns the rows and the server
/// time they correspond to.
async fn read_details(c: &Client, kinds: &[u16]) -> (Vec<Option<DetailRow>>, u32) {
    for _ in 0..3 {
        let before: u32 = c.query("SELECT toUInt32(now())").fetch_one().await.unwrap();
        let mut rows = Vec::new();
        for kind in kinds {
            rows.push(
                c.query(&kind_detail_sql())
                    .bind(*kind)
                    .bind(*kind)
                    .fetch_optional::<DetailRow>()
                    .await
                    .unwrap(),
            );
        }
        let after: u32 = c.query("SELECT toUInt32(now())").fetch_one().await.unwrap();
        if [1, 7, 30]
            .iter()
            .all(|d| floor_hour(before - d * DAY) == floor_hour(after - d * DAY))
            && before / DAY == after / DAY
        {
            return (rows, before);
        }
    }
    panic!("the hour kept changing while reading");
}

async fn read_series(c: &Client, kind: u16, grain: SeriesGrain, limit: u32) -> Vec<SeriesRow> {
    c.query(&kind_series_sql(grain, limit))
        .bind(kind)
        .fetch_all()
        .await
        .unwrap()
}

const KINDS: [u16; 9] = [1, 7, 3, 0, 20001, 30023, 1059, 4242, 65535];

async fn assert_reads_match(db: &TestDb, phase: &str) {
    let rows = db.stored().await;
    let (details, now) = read_details(&db.client, &KINDS).await;
    for (kind, got) in KINDS.iter().zip(details) {
        assert_eq!(
            got,
            expected_detail(&rows, *kind, now),
            "{phase}: kind {kind} detail"
        );
    }
    for grain in [SeriesGrain::Day, SeriesGrain::Week, SeriesGrain::Month] {
        for limit in [1, 7, 30, grain.max_limit()] {
            for kind in [1, 20001, 4242] {
                assert_eq!(
                    read_series(&db.client, kind, grain, limit).await,
                    expected_series(&rows, kind, grain, limit, now),
                    "{phase}: kind {kind} {grain:?} limit {limit}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn kind_activity_counts_each_row_exactly_once_around_the_view_boundary() {
    let db = TestDb::new().await;
    let now = db.now().await;
    // History: flushes over the last 40 days, before migration 021.
    for (i, chunk) in workload(0, 1_200, now).chunks(100).enumerate() {
        db.insert_at(now - 40 * DAY + i as u32 * 3 * DAY, chunk)
            .await;
    }
    // An event at the start of the hour containing now - 1 day: inside the
    // new 24 h window, outside the old rolling one unless now is on the hour.
    let edge = ev(9_000, 5, 4242, floor_hour(now - DAY), 10);
    db.insert_at(now - 100, std::slice::from_ref(&edge)).await;
    db.apply(MIGRATION_021).await.expect("migration 021");
    db.apply(MIGRATION_021).await.expect("021 is idempotent");
    db.set_merges("kind_activity", false).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let live_start = db.now().await;
    for chunk in workload(5_000, 600, now).chunks(75) {
        db.insert(chunk).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let plan = backfill_target(&db, dir.path(), Target::KindActivity).await;
    let boundary = plan
        .view_boundary
        .expect("kind activity plans fix a boundary");
    let rows = db.stored().await;
    let first_live = rows
        .iter()
        .filter(|r| r.indexed_at >= live_start)
        .map(|r| r.indexed_at)
        .min()
        .unwrap();
    assert_eq!(boundary, first_live, "boundary = first captured second");

    // Day and month rows hold every row once, split exactly at the boundary.
    let shares: Vec<(String, String, u64, u64)> = db
        .client
        .query(
            "SELECT toString(grain), toString(origin), toUInt64(sum(event_count)), \
             toUInt64(sum(content_bytes)) FROM kind_activity WHERE grain != 'hour' \
             GROUP BY grain, origin ORDER BY grain, origin",
        )
        .fetch_all()
        .await
        .unwrap();
    let before: Vec<&Stored> = rows.iter().filter(|r| r.indexed_at < boundary).collect();
    let bytes = |rs: &[&Stored]| rs.iter().map(|r| r.content_length).sum::<u64>();
    let after: Vec<&Stored> = rows.iter().filter(|r| r.indexed_at >= boundary).collect();
    for grain in ["day", "month"] {
        assert_eq!(
            shares
                .iter()
                .filter(|s| s.0 == grain)
                .map(|s| (s.1.as_str(), s.2, s.3))
                .collect::<Vec<_>>(),
            vec![
                ("backfill", before.len() as u64, bytes(&before)),
                ("view", after.len() as u64, bytes(&after)),
            ],
            "{grain}"
        );
    }
    // Hour rows only for the last 35 days.
    let (oldest_hour, hour_events): (u32, u64) = db
        .client
        .query(
            "SELECT toUInt32(min(period)), toUInt64(sum(event_count)) FROM kind_activity \
             WHERE grain = 'hour'",
        )
        .fetch_one()
        .await
        .unwrap();
    assert!(oldest_hour > now - HOUR_ROWS_DAYS * DAY, "{oldest_hour}");
    let recent = |r: &&Stored, floor: u32| floor_hour(r.created_at) > floor - HOUR_ROWS_DAYS * DAY;
    let expected_hours = before.iter().filter(|r| recent(r, boundary)).count()
        + after.iter().filter(|r| recent(r, r.indexed_at)).count();
    assert_eq!(hour_events, expected_hours as u64);

    // API reads, before and after merges.
    assert_reads_match(&db, "before merges").await;
    db.optimize("kind_activity").await;
    db.optimize("kind_totals").await;
    assert_reads_match(&db, "after merges").await;

    // The window semantics against the queries the endpoint ran before.
    let (details, at) = read_details(&db.client, &[4242, 1]).await;
    let edge_kind = details[0].clone().unwrap();
    let (old_24h, old_avg): (u64, f64) = db
        .client
        .query(
            "SELECT countIf(created_at >= now() - INTERVAL 1 DAY), avg(length(content)) \
             FROM events_local WHERE kind = 4242",
        )
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(edge_kind.events_24h, 1, "the edge event is counted");
    if !(at - DAY).is_multiple_of(3600) {
        assert_eq!(old_24h, 0, "the old rolling window started after it");
    }
    assert_eq!(
        average_content_length(edge_kind.content_bytes, edge_kind.events_all),
        old_avg
    );
    let kind1 = details[1].clone().unwrap();
    let (old_7d, old_30d): (u64, u64) = db
        .client
        .query(
            "SELECT countIf(created_at >= now() - INTERVAL 7 DAY), \
             countIf(created_at >= now() - INTERVAL 30 DAY) FROM events_local WHERE kind = 1",
        )
        .fetch_one()
        .await
        .unwrap();
    // The new windows add at most the partial first hour.
    assert!(kind1.events_7d >= old_7d && kind1.events_30d >= old_30d);
    let future: u64 = rows
        .iter()
        .filter(|r| r.kind == 1 && r.created_at > at)
        .count() as u64;
    assert!(future > 0, "the fixture has future-dated kind 1 events");
    assert!(
        kind1.events_24h >= future,
        "future-dated events stay counted"
    );

    // validate: counts, content bytes and sketches, and kind_totals' exact
    // distinct pubkeys.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let inputs = ValidateInputs {
        kind_activity_plan: Some(&plan),
        ..ValidateInputs::default()
    };
    let report = validate::run_with(
        &db.client,
        &db.name,
        &validate_options(&[Target::Kinds, Target::KindActivity]),
        &inputs,
    )
    .await
    .unwrap();
    eprintln!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    let activity = &report.targets[1];
    assert!(activity.compared > 100, "{}", report.render());
    assert!(
        activity.notes.iter().any(|n| n.contains("content bytes")),
        "{}",
        report.render()
    );

    // A shard counted twice is caught.
    db.exec(&format!(
        "INSERT INTO {}.kind_activity ({}) {}",
        db.name,
        Target::KindActivity.columns(),
        backfill_select_sql(&db.name, &plan.shard_predicate(&plan.shards[3]), boundary)
    ))
    .await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &validate_options(&[Target::KindActivity]),
        &inputs,
    )
    .await
    .unwrap();
    assert!(!report.passed(), "{}", report.render());
    assert!(report.targets[0].rows.contains_key(&Outcome::Excess));

    // Re-running the backfill is a no-op.
    let cp = CheckpointDir::new(dir.path(), Target::KindActivity);
    let rerun = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!(rerun.shards_run, 0);
    db.drop().await;
}

/// Exact distinct pubkeys of every kind.
async fn exact_pubkeys(c: &Client) -> BTreeMap<u16, u64> {
    c.query("SELECT kind, uniqExact(pubkey) FROM events_local GROUP BY kind")
        .fetch_all::<(u16, u64)>()
        .await
        .unwrap()
        .into_iter()
        .collect()
}

async fn approximate_pubkeys(c: &Client) -> BTreeMap<u16, u64> {
    c.query("SELECT kind, uniqCombined64Merge(pubkeys) FROM kind_totals GROUP BY kind")
        .fetch_all::<(u16, u64)>()
        .await
        .unwrap()
        .into_iter()
        .collect()
}

/// `count` events of `kind` by pubkeys numbered from `first`; `batch` keeps
/// event ids of different calls apart.
fn bulk(db: &TestDb, batch: &str, kind: u16, first: u64, count: u64) -> String {
    format!(
        "INSERT INTO {}.events_local (id, pubkey, created_at, kind, content, sig, tags) \
         SELECT lower(hex(SHA256(concat('e{batch}-', toString({kind}), '-', toString(number))))), \
         lower(hex(SHA256(concat('p', toString({kind}), '-', toString(number))))), \
         now() - number % 1000, {kind}, '', '', [] FROM numbers({first}, {count})",
        db.name
    )
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn kinds_unique_pubkeys_are_compared_exactly_for_fast_moving_kinds() {
    // TestDb::new applies 020 phase 1; recreate kind_totals' view after some
    // history so the kinds backfill and its boundary are exercised too.
    let db = TestDb::new().await;
    db.exec(&format!("DROP VIEW {}.kind_totals_mv", db.name))
        .await;
    db.exec(&format!("TRUNCATE TABLE {}.kind_totals", db.name))
        .await;
    // History: a large ephemeral kind (HyperLogLog mode), a mid-size kind
    // (exact mode) and a small one.
    db.exec(&bulk(&db, "a", 20001, 0, 40_000)).await;
    db.exec(&bulk(&db, "a", 7, 0, 3_000)).await;
    db.exec(&bulk(&db, "a", 1, 0, 50)).await;
    db.apply(MIGRATION_020).await.expect("re-apply 020");
    tokio::time::sleep(Duration::from_secs(6)).await;
    db.exec(&bulk(&db, "b", 20001, 40_000, 20_000)).await;
    // Half of these pubkeys already posted kind 7 before the boundary.
    db.exec(&bulk(&db, "b", 7, 2_500, 1_000)).await;
    let dir = tempfile::tempdir().unwrap();
    let plan = backfill_target(&db, dir.path(), Target::Kinds).await;

    // What the hourly kinds_stats_mv snapshot would have held now.
    let snapshot = exact_pubkeys(&db.client).await;
    // The fast-moving kind keeps getting new pubkeys after the snapshot.
    for start in (60_000..90_000).step_by(10_000) {
        db.exec(&bulk(&db, "c", 20001, start, 10_000)).await;
    }
    let approx = approximate_pubkeys(&db.client).await;
    let stale = approx[&20001].abs_diff(snapshot[&20001]) as f64 / snapshot[&20001] as f64;
    assert!(
        stale > 0.02,
        "against the stale snapshot the old 2% check fails ({stale})"
    );

    // Keep inserting new pubkeys of the fast kind while validate runs.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (client, stop, name) = (db.admin.clone(), stop.clone(), db.name.clone());
        tokio::spawn(async move {
            let mut start = 1_000_000u64;
            while !stop.load(Ordering::Relaxed) {
                let sql = format!(
                    "INSERT INTO {name}.events_local (id, pubkey, created_at, kind, content, sig, \
                     tags) SELECT lower(hex(SHA256(concat('e20001-', toString(number))))), \
                     lower(hex(SHA256(concat('p20001-', toString(number))))), now(), 20001, '', \
                     '', [] FROM numbers({start}, 500)"
                );
                client.query(&sql).execute().await.unwrap();
                start += 500;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            start - 1_000_000
        })
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    let options = ValidateOptions {
        pubkeys_kinds: 3,
        ..validate_options(&[Target::Kinds])
    };
    let inputs = ValidateInputs {
        kinds_plan: Some(&plan),
        ..ValidateInputs::default()
    };
    let report = validate::run_with(&db.client, &db.name, &options, &inputs)
        .await
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    let inserted = writer.await.unwrap();
    assert!(inserted > 0);
    eprintln!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    let kinds = &report.targets[0];
    let note = kinds
        .notes
        .iter()
        .find(|n| n.contains("unique_pubkeys of"))
        .unwrap_or_else(|| panic!("{}", report.render()));
    assert!(
        note.contains("20001") && note.contains("largest error"),
        "{note}"
    );
    assert!(note.contains("backfill share largest error"), "{note}");
    // Total and backfill share of each of the three kinds.
    let pubkey_rows: u64 = kinds
        .examples
        .iter()
        .filter(|d| d.key.contains("unique_pubkeys"))
        .count() as u64;
    assert_eq!(pubkey_rows, 0, "{}", report.render());
    assert_eq!(
        kinds.rows[&Outcome::Match],
        3 + 6,
        "3 kind counts + 3 x 2 pubkey comparisons: {}",
        report.render()
    );

    // An estimate off by more than the sketch error is caught: 500 pubkeys
    // that kind 7 never had (counts and first/last unchanged).
    db.exec(&format!(
        "INSERT INTO {}.kind_totals (kind, origin, event_count, pubkeys, first_created_at, \
         last_created_at, live_min_indexed_at, last_indexed_at) \
         SELECT 7, 'view', 0, uniqCombined64State(concat('ghost', toString(number))), \
         toDateTime(4294967295), toDateTime(0), toDateTime(4294967295), toDateTime(0) \
         FROM numbers(500)",
        db.name
    ))
    .await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &ValidateOptions {
            pubkeys_kinds: 0,
            pubkeys_extra_kinds: vec![7],
            ..validate_options(&[Target::Kinds])
        },
        &inputs,
    )
    .await
    .unwrap();
    assert!(!report.passed(), "{}", report.render());
    let failures: Vec<&str> = report.targets[0]
        .examples
        .iter()
        .map(|d| d.key.as_str())
        .collect();
    assert_eq!(failures, ["kind 7 unique_pubkeys"], "{}", report.render());
    db.drop().await;
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn api_reads_kind_activity_and_answers_503_without_it() {
    let db = TestDb::new().await;
    let now = db.now().await;
    db.insert(&workload(0, 300, now)).await;

    // Migration 021 not applied: no scan of events_local, 503 at once.
    let app = db.api().await;
    for path in [
        "/api/v1/kinds/1",
        "/api/v1/kinds/1/activity",
        "/api/v1/kinds/1/activity?group_by=month&limit=12",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
        assert_eq!(body["error"], "metric_unavailable", "{path}");
    }
    let err = db
        .client
        .query(&kind_detail_sql())
        .bind(1u16)
        .bind(1u16)
        .fetch_optional::<DetailRow>()
        .await
        .unwrap_err();
    assert!(is_unknown_table(&err), "{err}");

    // Applied on a database whose events all arrive after it (as on a fresh
    // install: nothing to backfill).
    db.exec(&format!("TRUNCATE TABLE {}.events_local", db.name))
        .await;
    db.exec(&format!("TRUNCATE TABLE {}.kind_totals", db.name))
        .await;
    db.apply(MIGRATION_021).await.expect("migration 021");
    for chunk in workload(0, 300, now).chunks(60) {
        db.insert(chunk).await;
    }
    let app = db.api().await;
    let rows = db.stored().await;
    let (status, body) = get(&app, "/api/v1/kinds/1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (details, at) = read_details(&db.client, &[1]).await;
    let want = expected_detail(&rows, 1, at).unwrap();
    assert_eq!(details[0].as_ref(), Some(&want));
    assert_eq!(
        body,
        serde_json::json!({
            "kind": 1,
            "event_count": want.event_count,
            "unique_pubkeys": want.unique_pubkeys,
            "first_seen": want.first_seen,
            "last_seen": want.last_seen,
            "avg_content_length": average_content_length(want.content_bytes, want.events_all),
            "events_24h": want.events_24h,
            "events_7d": want.events_7d,
            "events_30d": want.events_30d,
        }),
        "response shape unchanged"
    );
    let (status, body) = get(&app, "/api/v1/kinds/4242").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = get(&app, "/api/v1/kinds/20001/activity?group_by=week&limit=12").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let want = expected_series(&rows, 20001, SeriesGrain::Week, 12, at);
    assert!(!want.is_empty());
    assert_eq!(body, serde_json::to_value(&want).unwrap());
    let (status, _) = get(&app, "/api/v1/kinds/1/activity?group_by=year").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Rolled back: 503 again (fresh cache).
    db.apply(ROLLBACK_021).await.expect("rollback 021");
    let app = db.api().await;
    let (status, _) = get(&app, "/api/v1/kinds/1").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Ingest keeps working without the view.
    db.insert(&workload(10_000, 20, now)).await;
    db.drop().await;
}
