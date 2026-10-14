//! Integration tests for migration 022 (`hourly_pubkeys`) and the
//! `/stats/events`, `/stats/throughput` and `/stats/activity/hourly` reads
//! from migration 021's `kind_activity`, against a real ClickHouse (24.8,
//! matching production).
//!
//! Run with a disposable server:
//!
//! ```text
//! just ch-test-up
//! just test-clickhouse
//! ```
//!
//! Each test creates a scratch database with a production-shaped
//! `events_local` (no projections), the pre-020 derived views, migration 020
//! phase 1 and migration 021, then applies migration 022. Results are
//! compared with independent Rust oracles over the stored rows (few authors,
//! so the sketches are exact) and with the queries the endpoints ran before.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use clickhouse::Client;
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

use pensieve_serve::rollups::Target;
use pensieve_serve::rollups::backfill::{self, BackfillOptions, StopReason};
use pensieve_serve::rollups::event_stats::{
    DayWindow, EventGrain, events_by_period_sql, events_per_hour, events_total_sql, throughput_sql,
};
use pensieve_serve::rollups::hourly_pubkeys::{
    backfill_select_sql, hourly_activity_sql, hourly_pubkeys_kind,
};
use pensieve_serve::rollups::plan::{self, CheckpointDir, PlanOptions};
use pensieve_serve::rollups::validate::{self, Outcome, ValidateInputs, ValidateOptions};
use pensieve_serve::{AppState, Config, router};

const IGNORE_REASON: &str = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL";
const BASE_SCHEMA: &str = include_str!("fixtures/events_local.sql");
const PRE_020: &str = include_str!("fixtures/pre020_derived_views.sql");
const MIGRATION_020: &str =
    include_str!("../../../docs/migrations/prepared/020_incremental_derived_views.sql");
const MIGRATION_021: &str = include_str!("../../../docs/migrations/prepared/021_kind_activity.sql");
const MIGRATION_022: &str =
    include_str!("../../../docs/migrations/prepared/022_hourly_pubkeys.sql");
const ROLLBACK_022: &str =
    include_str!("../../../docs/migrations/rollback/022_drop_hourly_pubkeys.sql");
const DAY: u32 = 86_400;
const TOKEN: &str = "hourly-pubkeys-test-token";

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
    /// A scratch database with `events_local`, the pre-020 views, migration
    /// 020 phase 1 and migration 021 (so `kind_activity` sees every insert).
    async fn new() -> Self {
        let url = std::env::var("PENSIEVE_TEST_CLICKHOUSE_URL")
            .unwrap_or_else(|_| panic!("{IGNORE_REASON}"));
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "hourly_pubkeys_{nanos}_{}",
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
        db.apply(MIGRATION_021).await.expect("migration 021");
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
            .query("SELECT kind, pubkey, toUInt32(created_at) FROM events_local")
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
    created_at: u32,
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

fn validate_options() -> ValidateOptions {
    ValidateOptions {
        targets: vec![Target::HourlyPubkeys],
        cutoff_margin: Duration::from_secs(1),
        hour_days: Some(30),
        ..ValidateOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Oracles
// ---------------------------------------------------------------------------

fn utc(t: u32) -> DateTime<Utc> {
    DateTime::from_timestamp(i64::from(t), 0).unwrap()
}

fn day_of(t: u32) -> NaiveDate {
    utc(t).date_naive()
}

fn of_kind(r: &Stored, kind: Option<u16>) -> bool {
    kind.is_none_or(|k| r.kind == k)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
struct Totals {
    count: u64,
    unique_pubkeys: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clickhouse::Row)]
struct Period {
    period: String,
    count: u64,
    unique_pubkeys: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, clickhouse::Row)]
struct Hourly {
    hour: u8,
    event_count: u64,
    unique_pubkeys: u64,
    avg_per_day: f64,
}

/// `/stats/events` without `group_by`: rows created on a UTC day of `w`.
fn expected_totals(rows: &[Stored], w: DayWindow, kind: Option<u16>) -> Totals {
    let rows: Vec<&Stored> = rows
        .iter()
        .filter(|r| of_kind(r, kind) && (w.start..w.end).contains(&day_of(r.created_at)))
        .collect();
    Totals {
        count: rows.len() as u64,
        unique_pubkeys: rows
            .iter()
            .map(|r| r.pubkey.as_str())
            .collect::<BTreeSet<_>>()
            .len() as u64,
    }
}

fn period_of(day: NaiveDate, grain: EventGrain) -> NaiveDate {
    match grain {
        EventGrain::Day => day,
        EventGrain::Week => {
            day - chrono::Days::new(u64::from(day.weekday().num_days_from_monday()))
        }
        EventGrain::Month => day.with_day(1).unwrap(),
    }
}

/// `/stats/events?kind=K&group_by=…`: the `limit` newest periods with
/// events; a period cut by the window counts only its days inside it.
fn expected_periods(
    rows: &[Stored],
    w: DayWindow,
    kind: u16,
    grain: EventGrain,
    limit: u32,
) -> Vec<Period> {
    let mut periods: BTreeMap<NaiveDate, (u64, BTreeSet<&str>)> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.kind == kind) {
        let day = day_of(r.created_at);
        if (w.start..w.end).contains(&day) {
            let e = periods.entry(period_of(day, grain)).or_default();
            e.0 += 1;
            e.1.insert(&r.pubkey);
        }
    }
    periods
        .into_iter()
        .rev()
        .take(limit as usize)
        .map(|(p, (n, keys))| Period {
            period: p.to_string(),
            count: n,
            unique_pubkeys: keys.len() as u64,
        })
        .collect()
}

/// Rows from the UTC hour containing `now - days`, no upper bound.
fn recent(rows: &[Stored], kind: Option<u16>, days: u32, now: u32) -> Vec<&Stored> {
    let since = floor_hour(now - days * DAY);
    rows.iter()
        .filter(|r| of_kind(r, kind) && r.created_at >= since)
        .collect()
}

fn expected_hourly(rows: &[Stored], kind: Option<u16>, days: u32, now: u32) -> Vec<Hourly> {
    let mut hours: BTreeMap<u8, (u64, BTreeSet<&str>)> = BTreeMap::new();
    for r in recent(rows, kind, days, now) {
        let e = hours.entry((r.created_at / 3600 % 24) as u8).or_default();
        e.0 += 1;
        e.1.insert(&r.pubkey);
    }
    hours
        .into_iter()
        .map(|(hour, (n, keys))| Hourly {
            hour,
            event_count: n,
            unique_pubkeys: keys.len() as u64,
            avg_per_day: n as f64 / f64::from(days),
        })
        .collect()
}

/// Exact authors per (kind or -1, UTC hour) of hours newer than
/// `now - 35 days`.
fn expected_sketches(rows: &[Stored], now: u32) -> BTreeMap<(i32, u32), u64> {
    let mut out: BTreeMap<(i32, u32), BTreeSet<&str>> = BTreeMap::new();
    for r in rows {
        let hour = floor_hour(r.created_at);
        if hour > now - 35 * DAY {
            for scope in [i32::from(r.kind), -1] {
                out.entry((scope, hour)).or_default().insert(&r.pubkey);
            }
        }
    }
    out.into_iter().map(|(k, v)| (k, v.len() as u64)).collect()
}

async fn stored_sketches(c: &Client) -> BTreeMap<(i32, u32), u64> {
    c.query(
        "SELECT kind, toUInt32(period), uniqCombined64Merge(pubkeys) FROM hourly_pubkeys \
         GROUP BY kind, period",
    )
    .fetch_all::<(i32, u32, u64)>()
    .await
    .unwrap()
    .into_iter()
    .map(|(k, p, n)| ((k, p), n))
    .collect()
}

/// Run `read` until the server's UTC hour is the same before and after it
/// (the hour windows move with `now()`); returns the result and that time.
async fn at_stable_hour<T, F, Fut>(c: &Client, read: F) -> (T, u32)
where
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
{
    for _ in 0..3 {
        let before: u32 = c.query("SELECT toUInt32(now())").fetch_one().await.unwrap();
        let value = read().await;
        let after: u32 = c.query("SELECT toUInt32(now())").fetch_one().await.unwrap();
        if before / 3600 == after / 3600 {
            return (value, before);
        }
    }
    panic!("the hour kept changing while reading");
}

async fn read_hourly(c: &Client, kind: Option<u16>, days: u32) -> Vec<Hourly> {
    let query = c.query(&hourly_activity_sql(days, kind.is_some()));
    let query = match kind {
        Some(kind) => query.bind(kind),
        None => query,
    };
    query
        .bind(hourly_pubkeys_kind(kind))
        .fetch_all()
        .await
        .unwrap()
}

async fn read_throughput(c: &Client, kind: Option<u16>) -> u64 {
    let query = c.query(&throughput_sql(kind.is_some()));
    let query = match kind {
        Some(kind) => query.bind(kind),
        None => query,
    };
    query.fetch_one().await.unwrap()
}

async fn read_totals(c: &Client, w: DayWindow, kind: Option<u16>) -> Totals {
    let query = c.query(&events_total_sql(w, kind.is_some()));
    let query = match kind {
        Some(kind) => query.bind(kind),
        None => query,
    };
    query.fetch_one().await.unwrap()
}

async fn read_periods(
    c: &Client,
    w: DayWindow,
    kind: u16,
    grain: EventGrain,
    limit: u32,
) -> Vec<Period> {
    c.query(&events_by_period_sql(w, grain, limit))
        .bind(kind)
        .fetch_all()
        .await
        .unwrap()
}

const KINDS: [Option<u16>; 5] = [None, Some(1), Some(20001), Some(1059), Some(4242)];

/// Windows of `/stats/events` at `now`: rolling days (short and spanning
/// months and years), since/until with full months inside, open ends and an
/// inverted window.
fn windows(now: u32) -> Vec<DayWindow> {
    let at = utc(now);
    let today = at.date_naive();
    let month = today.with_day(1).unwrap();
    let mut out: Vec<DayWindow> = [0, 1, 7, 30, 45, 400, 3000, 30_000]
        .into_iter()
        .map(|d| DayWindow::new(at, Some(d), None, None))
        .collect();
    out.extend([
        DayWindow::new(
            at,
            None,
            Some(month - chrono::Months::new(3) + chrono::Days::new(9)),
            Some(month - chrono::Days::new(3)),
        ),
        DayWindow::new(at, None, Some(month - chrono::Months::new(14)), None),
        DayWindow::new(at, None, None, Some(month - chrono::Months::new(2))),
        DayWindow::new(at, None, Some(today), Some(today - chrono::Days::new(5))),
    ]);
    out
}

async fn assert_reads_match(db: &TestDb, phase: &str) {
    let rows = db.stored().await;
    let c = &db.client;
    for kind in KINDS {
        for days in [1, 7, 30] {
            let (got, now) = at_stable_hour(c, || read_hourly(c, kind, days)).await;
            assert_eq!(
                got,
                expected_hourly(&rows, kind, days, now),
                "{phase}: hourly kind {kind:?} days {days}"
            );
        }
        let (got, now) = at_stable_hour(c, || read_throughput(c, kind)).await;
        assert_eq!(
            got,
            recent(&rows, kind, 7, now).len() as u64,
            "{phase}: throughput kind {kind:?}"
        );
    }
    let now = db.now().await;
    for w in windows(now) {
        for kind in KINDS {
            assert_eq!(
                read_totals(c, w, kind).await,
                expected_totals(&rows, w, kind),
                "{phase}: totals kind {kind:?} {w:?}"
            );
        }
        for grain in [EventGrain::Day, EventGrain::Week, EventGrain::Month] {
            for limit in [1, 5, 1000] {
                for kind in [1, 20001, 4242] {
                    assert_eq!(
                        read_periods(c, w, kind, grain, limit).await,
                        expected_periods(&rows, w, kind, grain, limit),
                        "{phase}: kind {kind} {grain:?} limit {limit} {w:?}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn hourly_pubkeys_backfill_reads_and_validation() {
    let db = TestDb::new().await;
    let now = db.now().await;
    // History before migration 022: kind_activity (021) sees it, the new
    // table does not.
    for (i, chunk) in workload(0, 1_200, now).chunks(100).enumerate() {
        db.insert_at(now - 40 * DAY + i as u32 * 3 * DAY, chunk)
            .await;
    }
    db.apply(MIGRATION_022).await.expect("migration 022");
    db.apply(MIGRATION_022).await.expect("022 is idempotent");
    db.set_merges("hourly_pubkeys", false).await;
    for chunk in workload(5_000, 600, now).chunks(75) {
        db.insert(chunk).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let plan = backfill_target(&db, dir.path(), Target::HourlyPubkeys).await;
    assert_eq!(plan.view_boundary, None, "idempotent: no view boundary");

    // Every (kind, hour) and (all kinds, hour) of the last 35 days, exact
    // (few authors), view and backfill overlapping freely.
    let rows = db.stored().await;
    let expected = expected_sketches(&rows, now);
    assert!(expected.len() > 100, "{}", expected.len());
    assert_eq!(stored_sketches(&db.client).await, expected);

    // Reads before and after merges.
    assert_reads_match(&db, "before merges").await;
    db.optimize("hourly_pubkeys").await;
    db.optimize("kind_activity").await;
    assert_eq!(stored_sketches(&db.client).await, expected);
    assert_reads_match(&db, "after merges").await;

    // Against the queries the endpoints ran before: the new windows start
    // at whole UTC days (or the UTC hour), so they hold at least as much.
    let (old_count, old_unique): (u64, u64) = db
        .client
        .query(
            "SELECT count(), uniq(pubkey) FROM events_local \
             WHERE created_at <= now() AND kind = 1 AND created_at >= now() - INTERVAL 30 DAY",
        )
        .fetch_one()
        .await
        .unwrap();
    let new = read_totals(
        &db.client,
        DayWindow::new(utc(now), Some(30), None, None),
        Some(1),
    )
    .await;
    assert!(new.count >= old_count && new.unique_pubkeys >= old_unique);
    assert!(old_count > 0);
    let old_7d: u64 = db
        .client
        .query("SELECT count() FROM events_local WHERE created_at >= now() - INTERVAL 7 DAY")
        .fetch_one()
        .await
        .unwrap();
    let new_7d = read_throughput(&db.client, None).await;
    assert!(new_7d >= old_7d, "{new_7d} < {old_7d}");
    let future = rows.iter().filter(|r| r.created_at > now).count() as u64;
    assert!(
        future > 0 && new_7d >= future,
        "future-dated events stay counted"
    );

    // A busy hour: 20k authors put the hour and day sketches in
    // HyperLogLog mode (approximate from here on).
    db.exec(&format!(
        "INSERT INTO {}.events_local (id, pubkey, created_at, kind, content, sig, tags) \
         SELECT lower(hex(SHA256(concat('busy-', toString(number))))), \
         lower(hex(SHA256(concat('busy-author-', toString(number))))), \
         now() - number % 600, 20001, '', '', [] FROM numbers(20000)",
        db.name
    ))
    .await;
    let busy: u64 = db
        .client
        .query(
            "SELECT max(uniqCombined64Merge(pubkeys)) OVER () FROM hourly_pubkeys \
             WHERE kind = 20001 GROUP BY period LIMIT 1",
        )
        .fetch_one()
        .await
        .unwrap();
    assert!(busy > 4_096, "{busy}");

    // validate passes; the all-kinds day unions are compared too.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let inputs = ValidateInputs::default();
    let report = validate::run_with(&db.client, &db.name, &validate_options(), &inputs)
        .await
        .unwrap();
    eprintln!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    let target = &report.targets[0];
    assert!(target.compared > 100, "{}", report.render());
    assert!(
        target.notes.iter().any(|n| n.contains("day unions")),
        "{}",
        report.render()
    );

    // Re-running a shard changes nothing (sketches are idempotent).
    let before_rerun = stored_sketches(&db.client).await;
    db.exec(&format!(
        "INSERT INTO {}.hourly_pubkeys ({}) {}",
        db.name,
        Target::HourlyPubkeys.columns(),
        backfill_select_sql(&db.name, &plan.shard_predicate(&plan.shards[3]))
    ))
    .await;
    assert_eq!(stored_sketches(&db.client).await, before_rerun);
    let cp = CheckpointDir::new(dir.path(), Target::HourlyPubkeys);
    let rerun = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!(rerun.shards_run, 0);

    // Authors the hour never had are caught.
    db.exec(&format!(
        "INSERT INTO {}.hourly_pubkeys (kind, period, pubkeys, last_indexed_at) \
         SELECT -1, toStartOfHour(toDateTime({}), 'UTC'), \
         uniqCombined64State(concat('ghost', toString(number))), toDateTime(0) \
         FROM numbers(50)",
        db.name,
        now - DAY
    ))
    .await;
    let report = validate::run_with(&db.client, &db.name, &validate_options(), &inputs)
        .await
        .unwrap();
    assert!(!report.passed(), "{}", report.render());
    assert_eq!(report.targets[0].failures(), 1, "{}", report.render());
    assert!(report.targets[0].rows.contains_key(&Outcome::Excess));
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn validation_fails_while_the_backfill_is_missing() {
    let db = TestDb::new().await;
    let now = db.now().await;
    // History from 5 days ago, indexed before migration 022: only the
    // backfill writes it. Outside the old 2-day default window.
    let history: Vec<Ev> = (0..200)
        .map(|n| {
            ev(
                n,
                n % 17,
                [1, 7][n as usize % 2],
                now - 5 * DAY + n as u32 * 60,
                5,
            )
        })
        .collect();
    db.insert_at(now - 5 * DAY + 3 * 3600, &history).await;
    db.apply(MIGRATION_022).await.expect("migration 022");
    for chunk in workload(1_000, 100, now).chunks(50) {
        db.insert(chunk).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let plan = plan::build_plan(
        &db.client,
        &db.name,
        Target::HourlyPubkeys,
        &PlanOptions {
            id_prefix_len: 1,
            boundary_margin_secs: 5,
        },
    )
    .await
    .unwrap();
    let cp = CheckpointDir::new(dir.path(), Target::HourlyPubkeys);
    cp.save_plan(&plan, false).unwrap();
    assert!(!backfill::is_complete(&cp).unwrap());
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Default window: the view is younger than 35 days, so all 30 days are
    // compared and the history's hours are missing.
    let options = ValidateOptions {
        targets: vec![Target::HourlyPubkeys],
        cutoff_margin: Duration::from_secs(1),
        ..ValidateOptions::default()
    };
    let report = validate::run_with(&db.client, &db.name, &options, &ValidateInputs::default())
        .await
        .unwrap();
    eprintln!("{}", report.render());
    assert!(!report.passed(), "{}", report.render());
    assert!(
        report.targets[0].rows[&Outcome::Missing] > 5,
        "{}",
        report.render()
    );

    // A 2-day window does not reach the history, but the checkpoints say the
    // backfill is incomplete: still a failure.
    let two_days = ValidateOptions {
        hour_days: Some(2),
        ..options.clone()
    };
    let unknown = validate::run_with(&db.client, &db.name, &two_days, &ValidateInputs::default())
        .await
        .unwrap();
    assert!(unknown.passed(), "{}", unknown.render());
    let incomplete = ValidateInputs {
        hourly_pubkeys_backfill_complete: Some(false),
        ..ValidateInputs::default()
    };
    let report = validate::run_with(&db.client, &db.name, &two_days, &incomplete)
        .await
        .unwrap();
    assert!(!report.passed(), "{}", report.render());
    assert_eq!(report.targets[0].failures(), 1, "{}", report.render());
    assert_eq!(report.targets[0].examples[0].key, "hourly-pubkeys backfill");

    // After the backfill both pass.
    let done = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!(done.stopped, StopReason::Complete);
    let complete = ValidateInputs {
        hourly_pubkeys_backfill_complete: Some(backfill::is_complete(&cp).unwrap()),
        ..ValidateInputs::default()
    };
    let report = validate::run_with(&db.client, &db.name, &options, &complete)
        .await
        .unwrap();
    assert!(report.passed(), "{}", report.render());
    db.drop().await;
}

/// `value` as a response body parses: through the same JSON text and parser
/// (serde_json's default float parsing can be one ulp off the original f64).
fn as_parsed<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
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
async fn api_reads_the_rollups_and_keeps_the_response_shapes() {
    let db = TestDb::new().await;

    // Migration 022 not applied: the hourly pattern answers 503 at once.
    let app = db.api().await;
    for path in [
        "/api/v1/stats/activity/hourly",
        "/api/v1/stats/activity/hourly?days=30&kind=1",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
        assert_eq!(body["error"], "metric_unavailable", "{path}");
    }
    for path in [
        "/api/v1/stats/activity/hourly?days=0",
        "/api/v1/stats/activity/hourly?days=31",
        "/api/v1/stats/activity/hourly?days=90",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }

    // Applied on a database whose events all arrive after it (as on a fresh
    // install: nothing to backfill).
    db.apply(MIGRATION_022).await.expect("migration 022");
    let now = db.now().await;
    for chunk in workload(0, 400, now).chunks(80) {
        db.insert(chunk).await;
    }
    let rows = db.stored().await;
    let app = db.api().await;

    let (status, body) = get(&app, "/api/v1/stats/events").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let window = DayWindow::new(utc(now), Some(30), None, None);
    assert_eq!(
        body,
        serde_json::to_value(expected_totals(&rows, window, None)).unwrap(),
        "default: all kinds, 30 days"
    );
    let (status, body) = get(&app, "/api/v1/stats/events?kind=1&days=7").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let window = DayWindow::new(utc(now), Some(7), None, None);
    assert_eq!(
        body,
        serde_json::to_value(expected_totals(&rows, window, Some(1))).unwrap()
    );
    let (status, body) = get(
        &app,
        "/api/v1/stats/events?kind=20001&since=2000-01-01&group_by=month&limit=6",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let window = DayWindow::new(utc(now), None, NaiveDate::from_ymd_opt(2000, 1, 1), None);
    let want = expected_periods(&rows, window, 20001, EventGrain::Month, 6);
    assert!(!want.is_empty());
    assert_eq!(body, serde_json::to_value(&want).unwrap());
    // group_by without kind still reads daily_user_stats.
    let (status, body) = get(&app, "/api/v1/stats/events?group_by=day&days=7").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.is_array());
    let (status, _) = get(&app, "/api/v1/stats/events?kind=1&group_by=year").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for _ in 0..3 {
        let before = db.now().await;
        let app = db.api().await;
        let (status, throughput) = get(&app, "/api/v1/stats/throughput?kind=1").await;
        assert_eq!(status, StatusCode::OK, "{throughput}");
        let (status, hourly) = get(&app, "/api/v1/stats/activity/hourly?kind=1&days=30").await;
        assert_eq!(status, StatusCode::OK, "{hourly}");
        let (status, default_hourly) = get(&app, "/api/v1/stats/activity/hourly").await;
        assert_eq!(status, StatusCode::OK, "{default_hourly}");
        if before / 3600 != db.now().await / 3600 {
            continue;
        }
        let total = recent(&rows, Some(1), 7, before).len() as u64;
        assert_eq!(
            throughput,
            as_parsed(&serde_json::json!({
                "events_per_hour": events_per_hour(total),
                "total_events_7d": total,
            })),
            "throughput shape and value"
        );
        assert_eq!(
            hourly,
            as_parsed(&expected_hourly(&rows, Some(1), 30, before))
        );
        let want = expected_hourly(&rows, None, 7, before);
        assert!(!want.is_empty());
        assert_eq!(default_hourly, as_parsed(&want));
        break;
    }

    // Rolled back: the hourly pattern answers 503 again; the rest is
    // unaffected.
    db.apply(ROLLBACK_022).await.expect("rollback 022");
    let app = db.api().await;
    let (status, _) = get(&app, "/api/v1/stats/activity/hourly").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = get(&app, "/api/v1/stats/events?kind=1").await;
    assert_eq!(status, StatusCode::OK);
    // Ingest keeps working without the view.
    db.insert(&workload(10_000, 20, now)).await;
    db.drop().await;
}
