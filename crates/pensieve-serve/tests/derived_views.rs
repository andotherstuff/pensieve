//! Integration tests for migration 020 (incremental derived views) against a
//! real ClickHouse (24.8, matching production).
//!
//! Run with a disposable server:
//!
//! ```text
//! just ch-test-up
//! just test-clickhouse
//! ```
//!
//! Each test creates a scratch database with a production-shaped
//! `events_local` (no projections) and the pre-020 derived views
//! (`fixtures/pre020_derived_views.sql`: migrations 004, 005, 013, 014, 015),
//! then applies migration 020. Results are compared with the pre-020 views run
//! on the same inserts, and with independent Rust oracles.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{Datelike, NaiveDate};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use pensieve_serve::rollups::backfill::{self, BackfillOptions, StopReason};
use pensieve_serve::rollups::cutover::{self, CUTOVER_SQL, CutoverOptions};
use pensieve_serve::rollups::derived::{
    CUTOVER_JOIN_SETTINGS, FlagRule, Grain, KindsSort, NEVER, RelaySource, active_users_select_sql,
    daily_user_stats_cutover_query, earliest_event_sql, kinds_list_sql,
    relay_distribution_select_sql, total_kinds_sql,
};
use pensieve_serve::rollups::plan::{self, CheckpointDir, PendingShard, PlanOptions, utc_day};
use pensieve_serve::rollups::validate::{self, Outcome, ValidateInputs, ValidateOptions};
use pensieve_serve::rollups::{Target, is_unknown_table};

const IGNORE_REASON: &str = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL";
const BASE_SCHEMA: &str = include_str!("fixtures/events_local.sql");
const PRE_020: &str = include_str!("fixtures/pre020_derived_views.sql");
const MIGRATION_020: &str =
    include_str!("../../../docs/migrations/prepared/020_incremental_derived_views.sql");
const UNDO_CUTOVER: &str =
    include_str!("../../../docs/migrations/rollback/020_undo_derived_views_cutover.sql");
const DROP_020: &str =
    include_str!("../../../docs/migrations/rollback/020_drop_incremental_derived_views.sql");
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
    /// A scratch database with `events_local` and the pre-020 views.
    async fn new() -> Self {
        let url = std::env::var("PENSIEVE_TEST_CLICKHOUSE_URL")
            .unwrap_or_else(|_| panic!("{IGNORE_REASON}"));
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "derived_views_{nanos}_{}",
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
            name,
            admin,
            client,
        };
        db.apply(BASE_SCHEMA).await.expect("base schema");
        db.apply(PRE_020).await.expect("pre-020 views");
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

    async fn apply_020(&self) {
        self.apply(MIGRATION_020).await.expect("migration 020");
    }

    /// The cutover statements, without the tool's guards.
    async fn apply_cutover(&self) {
        for statement in cutover::statements(CUTOVER_SQL, &self.name) {
            self.admin
                .query(&statement)
                .execute()
                .await
                .unwrap_or_else(|e| panic!("{e}\n{statement}"));
        }
    }

    async fn exec(&self, sql: &str) {
        self.admin
            .query(sql)
            .execute()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    async fn set_merges(&self, tables: &[&str], start: bool) {
        let verb = if start { "START" } else { "STOP" };
        for table in tables {
            self.exec(&format!("SYSTEM {verb} MERGES {}.{table}", self.name))
                .await;
        }
    }

    async fn optimize(&self, tables: &[&str]) {
        self.set_merges(tables, true).await;
        for table in tables {
            self.exec(&format!("OPTIMIZE TABLE {}.{table} FINAL", self.name))
                .await;
        }
    }

    async fn insert(&self, events: &[Ev]) {
        let client = self.client.clone().with_option("async_insert", "0");
        let mut insert = client.insert("events_local").expect("insert");
        for e in events {
            insert.write(&e.row()).await.expect("write row");
        }
        insert.end().await.expect("end insert");
    }

    /// One insert with an explicit `indexed_at` (an ingest flush at `at`).
    async fn insert_at(&self, at: u32, events: &[Ev]) {
        let client = self.client.clone().with_option("async_insert", "0");
        let mut insert = client.insert("events_local").expect("insert");
        for e in events {
            insert.write(&e.row_at(at)).await.expect("write row");
        }
        insert.end().await.expect("end insert");
    }

    async fn now(&self) -> u32 {
        self.client
            .query("SELECT toUInt32(now())")
            .fetch_one()
            .await
            .unwrap()
    }

    async fn as_select(&self, view: &str) -> Option<String> {
        self.client
            .query(
                "SELECT as_select FROM system.tables WHERE database = currentDatabase() AND name = ?",
            )
            .bind(view)
            .fetch_optional()
            .await
            .unwrap()
    }

    /// Wait until `view` has refreshed successfully at or after `since`.
    async fn wait_refreshed(&self, view: &str, since: u32) {
        for _ in 0..120 {
            let done: u64 = self
                .client
                .query(
                    "SELECT count() FROM system.view_refreshes WHERE database = currentDatabase() \
                     AND view = ? AND status != 'Running' \
                     AND ifNull(toUInt32(last_success_time), 0) >= ?",
                )
                .bind(view)
                .bind(since)
                .fetch_one()
                .await
                .unwrap();
            if done > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("{view} did not refresh");
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
/// and drop comments (and session `SET`s, which the files repeat inline).
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
            content: "",
            sig: "",
            tags: &self.tags,
            relay_source: "",
        }
    }

    fn row_at(&self, indexed_at: u32) -> InsertRowAt<'_> {
        InsertRowAt {
            id: &self.id,
            pubkey: &self.pubkey,
            created_at: self.created_at,
            kind: self.kind,
            content: "",
            sig: "",
            tags: &self.tags,
            indexed_at,
            relay_source: "",
        }
    }
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

/// A 64-hex pubkey whose prefix is spread over the keyspace.
fn pubkey(n: u64) -> String {
    format!("{:016x}{n:048x}", splitmix(n ^ 0xfeed))
}

fn ev(n: u64, author: u64, kind: u16, created_at: u32) -> Ev {
    Ev {
        id: event_id(n),
        pubkey: pubkey(author),
        created_at,
        kind,
        tags: Vec::new(),
    }
}

fn relay_list(n: u64, author: u64, created_at: u32, tags: &[&[&str]]) -> Ev {
    let mut e = ev(n, author, 10002, created_at);
    e.tags = tags
        .iter()
        .map(|t| t.iter().map(|s| s.to_string()).collect())
        .collect();
    e
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

/// Plan and run a complete backfill of `target`.
async fn backfill_target(db: &TestDb, dir: &std::path::Path, target: Target) -> plan::Plan {
    let plan = plan::build_plan(&db.client, &db.name, target, &plan_options())
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
        sample: 50,
        boundary_sample: 50,
        sample_permyriad: 10_000,
        cutoff_margin: Duration::from_secs(1),
        batch: 7,
        flag_days: 400,
        user_days: 400,
        ..ValidateOptions::default()
    }
}

// ---------------------------------------------------------------------------
// User flags
// ---------------------------------------------------------------------------

/// One period of an active-users refresh (`updated_at` left out).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
struct ActiveRow {
    period: u16,
    active_users: u64,
    has_profile: u64,
    has_follows: u64,
    has_both: u64,
    total_events: u64,
}

async fn active_users(c: &Client, grain: Grain, rule: FlagRule, db: &str) -> Vec<ActiveRow> {
    let select = active_users_select_sql(grain, rule, db);
    let select = select.trim_end_matches(CUTOVER_JOIN_SETTINGS);
    c.query(&format!(
        "SELECT toUInt16({col}) AS period, active_users, has_profile, has_follows, has_both, \
         total_events FROM ({select}) ORDER BY period {CUTOVER_JOIN_SETTINGS}",
        col = grain.column()
    ))
    .fetch_all()
    .await
    .unwrap()
}

/// What the pre-020 views mean, computed from the inserts: an event is
/// flagged when its pubkey published a kind 0 (kind 3) in an EARLIER insert;
/// a (date, pubkey) is flagged when any of its events is.
fn expected_active_users(batches: &[Vec<Ev>], today: NaiveDate, grain: Grain) -> Vec<ActiveRow> {
    let first_batch = |kind: u16| -> BTreeMap<&str, usize> {
        let mut first = BTreeMap::new();
        for (i, batch) in batches.iter().enumerate() {
            for e in batch.iter().filter(|e| e.kind == kind) {
                first.entry(e.pubkey.as_str()).or_insert(i);
            }
        }
        first
    };
    let (profiles, follows) = (first_batch(0), first_batch(3));
    let start = match grain {
        Grain::Day => today - chrono::Days::new(30),
        Grain::Week => monday(today) - chrono::Days::new(182),
        Grain::Month => today.with_day(1).unwrap() - chrono::Days::new(365),
    };
    // (period, pubkey) -> (events, profile, follows), from (date, pubkey) groups.
    let mut groups: BTreeMap<(NaiveDate, &str), (u64, bool, bool)> = BTreeMap::new();
    for (i, batch) in batches.iter().enumerate() {
        for e in batch.iter().filter(|e| ![445, 1059].contains(&e.kind)) {
            let day = utc_day(e.created_at);
            if day < start || day > today {
                continue;
            }
            let g = groups.entry((day, e.pubkey.as_str())).or_default();
            g.0 += 1;
            g.1 |= profiles.get(e.pubkey.as_str()).is_some_and(|&b| b < i);
            g.2 |= follows.get(e.pubkey.as_str()).is_some_and(|&b| b < i);
        }
    }
    let period = |day: NaiveDate| match grain {
        Grain::Day => day,
        Grain::Week => monday(day),
        Grain::Month => day.with_day(1).unwrap(),
    };
    #[derive(Default)]
    struct Acc<'a> {
        all: BTreeSet<&'a str>,
        profile: BTreeSet<&'a str>,
        follows: BTreeSet<&'a str>,
        both: BTreeSet<&'a str>,
        events: u64,
    }
    let mut out: BTreeMap<NaiveDate, Acc> = BTreeMap::new();
    for ((day, pubkey), (events, profile, follow)) in groups {
        let acc = out.entry(period(day)).or_default();
        acc.all.insert(pubkey);
        acc.events += events;
        if profile {
            acc.profile.insert(pubkey);
        }
        if follow {
            acc.follows.insert(pubkey);
        }
        if profile && follow {
            acc.both.insert(pubkey);
        }
    }
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    out.into_iter()
        .map(|(p, a)| ActiveRow {
            period: (p - epoch).num_days() as u16,
            active_users: a.all.len() as u64,
            has_profile: a.profile.len() as u64,
            has_follows: a.follows.len() as u64,
            has_both: a.both.len() as u64,
            total_events: a.events,
        })
        .collect()
}

fn monday(day: NaiveDate) -> NaiveDate {
    day - chrono::Days::new(u64::from(day.weekday().num_days_from_monday()))
}

/// Six ingest flushes. Pubkeys: 1 profile in flush 0 (with own events), 2
/// follows in flush 0 and profile in 2, 3 profile+follows in 3 (after 020
/// phase 1, before the seed), 4 profile in 4 together with events, again in 5;
/// 5 never; 6 posts many flagged events a day (summed flags); 7 profile in 5.
/// Events are spread over the last 200 days so every refresh window sees
/// them; flushes 4 and 5 come after the cutover.
fn flag_batches(now: u32) -> Vec<Vec<Ev>> {
    let mut n = 0u64;
    let mut next = |author: u64, kind: u16, days_ago: u32| {
        n += 1;
        ev(n, author, kind, now - days_ago * DAY - (n as u32 % 1000))
    };
    let mut batches = vec![Vec::new(); 6];
    for (batch, author, kind, days) in [
        (0, 1, 0, 2),
        (0, 1, 1, 2),
        (0, 1, 1, 40),
        (0, 2, 3, 3),
        (0, 2, 1, 3),
        (0, 5, 1, 3),
        (0, 6, 0, 150),
        (1, 1, 1, 2),
        (1, 2, 1, 2),
        (1, 3, 1, 2),
        (1, 6, 1, 2),
        (1, 6, 1, 2),
        (1, 6, 7, 2),
        (2, 2, 0, 1),
        (2, 2, 1, 1),
        (2, 3, 1, 100),
        (2, 6, 1, 2),
        (3, 3, 0, 1),
        (3, 3, 3, 1),
        (3, 1, 1, 1),
        (3, 6, 1, 1),
        (3, 6, 1, 1),
        (4, 3, 1, 1),
        (4, 4, 0, 1),
        (4, 4, 1, 1),
        (4, 4, 1, 60),
        (4, 2, 1, 2),
        (4, 6, 1, 1),
        (4, 6, 1, 2),
        (4, 5, 1059, 1),
        (5, 4, 1, 1),
        (5, 3, 1, 150),
        (5, 7, 0, 0),
        (5, 7, 1, 0),
        (5, 5, 1, 0),
        (5, 1, 1, 2),
        (5, 6, 1, 1),
    ] {
        batches[batch].push(next(author, kind, days));
    }
    batches
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn user_flags_keep_insert_time_semantics_across_the_cutover() {
    // `old` keeps the pre-020 views; `new` goes through migration 020.
    let old = TestDb::new().await;
    let new = TestDb::new().await;
    let now = new.now().await;
    let batches = flag_batches(now);
    // Flushes in the past, ten seconds apart (sequential ingest inserts).
    let flush = |i: usize| now - 1000 + 10 * i as u32;
    for db in [&old, &new] {
        db.set_merges(&["daily_user_stats"], false).await;
    }
    for (i, batch) in batches.iter().enumerate().take(3) {
        for db in [&old, &new] {
            db.insert_at(flush(i), batch).await;
        }
    }
    new.apply_020().await;
    new.apply_020().await; // idempotent
    for db in [&old, &new] {
        db.insert_at(flush(3), &batches[3]).await;
    }
    // Seed pubkey_flags from the lookup tables.
    let dir = tempfile::tempdir().unwrap();
    backfill_target(&new, dir.path(), Target::UserFlags).await;
    new.apply_cutover().await;
    assert_eq!(
        new.as_select("daily_user_stats_mv").await.unwrap(),
        daily_user_stats_cutover_query(&new.name),
        "the insert path has no lookups after the cutover"
    );
    for (i, batch) in batches.iter().enumerate().skip(4) {
        for db in [&old, &new] {
            db.insert_at(flush(i), batch).await;
        }
    }

    // pubkey_flags: seeded pubkeys from 1970, newcomers at their first flush.
    let flags: BTreeMap<String, (u32, u32)> = new
        .client
        .query(
            "SELECT pubkey, toUInt32(min(profile_since)), toUInt32(min(follows_since)) \
             FROM pubkey_flags GROUP BY pubkey",
        )
        .fetch_all::<(String, u32, u32)>()
        .await
        .unwrap()
        .into_iter()
        .map(|(p, a, b)| (p, (a, b)))
        .collect();
    assert_eq!(flags[&pubkey(1)], (0, NEVER));
    assert_eq!(flags[&pubkey(2)], (0, 0));
    assert_eq!(flags[&pubkey(3)], (0, 0), "flagged before the seed: seeded");
    assert_eq!(flags[&pubkey(4)], (flush(4), NEVER), "captured by the view");
    assert_eq!(flags[&pubkey(7)], (flush(5), NEVER));
    assert!(!flags.contains_key(&pubkey(5)));

    let today = utc_day(new.now().await);
    for phase in ["before merges", "after merges"] {
        // Every (date, pubkey) counted exactly once across the switch.
        let counts = |db: &TestDb| {
            let c = db.client.clone();
            async move {
                c.query(
                    "SELECT pubkey, toUInt16(date), toUInt64(sum(event_count)) \
                     FROM daily_user_stats GROUP BY pubkey, date ORDER BY pubkey, date",
                )
                .fetch_all::<(String, u16, u64)>()
                .await
                .unwrap()
            }
        };
        assert_eq!(counts(&old).await, counts(&new).await, "{phase}");
        for grain in Grain::ALL {
            let stored = active_users(&old.client, grain, FlagRule::Stored, &old.name).await;
            let cutover = active_users(&new.client, grain, FlagRule::Cutover, &new.name).await;
            assert_eq!(
                stored, cutover,
                "{phase} {grain:?}: 020 equals the pre-020 meaning"
            );
            if phase == "after merges" {
                // Merged rows are compared with the meaning computed from the
                // inserts (before merges, unmerged rows of one (date, pubkey)
                // are judged one by one, as before 020).
                assert_eq!(
                    cutover,
                    expected_active_users(&batches, today, grain),
                    "{phase} {grain:?}: oracle"
                );
            }
        }
        if phase == "before merges" {
            for db in [&old, &new] {
                db.optimize(&["daily_user_stats"]).await;
            }
        }
    }

    // Migration 013's `= 1` loses pubkeys whose summed flag exceeds 1:
    // pubkey 6 posted several flagged events on one day.
    let legacy = active_users(&old.client, Grain::Day, FlagRule::Legacy, &old.name).await;
    let stored = active_users(&old.client, Grain::Day, FlagRule::Stored, &old.name).await;
    let profiles = |rows: &[ActiveRow]| rows.iter().map(|r| r.has_profile).sum::<u64>();
    assert!(
        profiles(&legacy) < profiles(&stored),
        "{legacy:?} vs {stored:?}"
    );

    // The refreshable views the cutover created write the same numbers.
    for (grain, view) in [
        (Grain::Day, "active_users_daily_mv"),
        (Grain::Week, "active_users_weekly_mv"),
        (Grain::Month, "active_users_monthly_mv"),
    ] {
        let since = new.now().await;
        new.exec(&format!("SYSTEM REFRESH VIEW {}.{view}", new.name))
            .await;
        new.wait_refreshed(view, since).await;
        let table: Vec<ActiveRow> = new
            .client
            .query(&format!(
                "SELECT toUInt16({col}) AS period, active_users, has_profile, has_follows, \
                 has_both, total_events FROM {table} FINAL ORDER BY period",
                col = grain.column(),
                table = grain.table()
            ))
            .fetch_all()
            .await
            .unwrap();
        assert_eq!(
            table,
            expected_active_users(&batches, today, grain),
            "{view} after the cutover"
        );
    }

    // validate: flags backed, counts exact, last_indexed_at written.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let options = ValidateOptions {
        boundary_sample: 0,
        ..validate_options(&[Target::UserFlags])
    };
    let inputs = ValidateInputs {
        cutover_at: Some(flush(3) + 5),
        ..ValidateInputs::default()
    };
    let report = validate::run_with(&new.client, &new.name, &options, &inputs)
        .await
        .unwrap();
    assert!(report.passed(), "{}", report.render());
    assert!(report.targets[0].compared > 0, "{}", report.render());
    old.drop().await;
    new.drop().await;
}

// ---------------------------------------------------------------------------
// Relay lists
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, clickhouse::Row)]
struct RelayRow {
    relay_url: String,
    user_count: u64,
    read_count: u64,
    write_count: u64,
}

#[derive(Debug, Deserialize, clickhouse::Row)]
struct Winner {
    id: String,
    relays: Vec<Vec<String>>,
}

async fn distribution(c: &Client, source: RelaySource, db: &str) -> Vec<RelayRow> {
    c.query(&format!(
        "SELECT relay_url, user_count, read_count, write_count FROM ({}) ORDER BY relay_url",
        relay_distribution_select_sql(source, db)
    ))
    .fetch_all()
    .await
    .unwrap()
}

/// Relay lists of 40 authors with every URL shape 014 normalises or filters,
/// older versions (some arriving after the newer one) and malformed tags.
/// Versions of one author never share a `created_at`.
fn relay_workload(first_id: u64, now: u32) -> Vec<Ev> {
    const URLS: [&[&str]; 12] = [
        &["r", "wss://relay.damus.io"],
        &["r", "wss://relay.damus.io/", "read"],
        &["r", " WSS://Relay.Damus.io// ", "write"],
        &["r", "wss://nos.lol:443"],
        &["r", "wss://nos.lol", "read"],
        &["r", "ws://insecure.example"],
        &["r", "wss://localhost:7777"],
        &["r", "wss://relay.primal.net", "write"],
        &["r", "wss://bad url.example"],
        &["r", "wss://relay.nostr.band", "read"],
        &["r"],
        &["p", "wss://not-a-relay"],
    ];
    let mut out = Vec::new();
    let mut n = first_id;
    for author in 0..40u64 {
        let r = splitmix(author);
        let versions = 1 + r % 3;
        for v in 0..versions {
            n += 1;
            let picks: Vec<&[&str]> = (0..1 + (r >> (8 + 4 * v)) % 6)
                .map(|i| URLS[((r >> (3 * i + v)) % URLS.len() as u64) as usize])
                .collect();
            out.push(relay_list(
                n,
                100 + author,
                now - (versions - v) as u32 * 1000 - author as u32,
                &picks,
            ));
        }
    }
    // Arrival order is not creation order.
    out.reverse();
    out.push(relay_list(
        n + 1,
        500,
        now - 10,
        &[&[], &["r", "wss://relay.damus.io"], &["r", ""]],
    ));
    out
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn relay_lists_reproduce_the_014_distribution() {
    let db = TestDb::new().await;
    let now = db.now().await;
    // History before 020, then live lists after it, then a backfill.
    let history = relay_workload(0, now - 10 * DAY);
    for chunk in history.chunks(17) {
        db.insert(chunk).await;
    }
    db.apply_020().await;
    db.set_merges(&["relay_list_latest"], false).await;
    let live = relay_workload(10_000, now);
    for chunk in live.chunks(23) {
        db.insert(chunk).await;
    }
    let dir = tempfile::tempdir().unwrap();
    backfill_target(&db, dir.path(), Target::RelayLists).await;

    let old = distribution(&db.client, RelaySource::EventsLocal, &db.name).await;
    assert!(
        old.len() >= 2,
        "the fixture must clear HAVING user_count >= 10: {old:?}"
    );
    for phase in ["before merges", "after merges"] {
        assert_eq!(
            distribution(&db.client, RelaySource::LatestLists, &db.name).await,
            old,
            "{phase}"
        );
        if phase == "before merges" {
            db.optimize(&["relay_list_latest"]).await;
        }
    }

    // A created_at tie: the lowest id wins, whatever the arrival order.
    db.set_merges(&["relay_list_latest"], false).await;
    let mut lo = relay_list(90_001, 900, now, &[&["r", "wss://lowest.example"]]);
    lo.id = format!("0{}", &lo.id[1..]);
    let hi = relay_list(90_002, 900, now, &[&["r", "wss://higher.example"]]);
    let older = relay_list(90_003, 900, now - 5, &[&["r", "wss://older.example"]]);
    db.insert(std::slice::from_ref(&hi)).await;
    db.insert(&[older, lo.clone()]).await;
    db.insert(std::slice::from_ref(&hi)).await;
    for phase in ["before merges", "after merges"] {
        let winner: Winner = db
            .client
            .query(
                "SELECT tupleElement(l, 1) AS id, tupleElement(l, 3) AS relays FROM \
                 (SELECT argMinMerge(latest) AS l FROM relay_list_latest WHERE pubkey = ?)",
            )
            .bind(pubkey(900))
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(winner.id, lo.id, "{phase}");
        assert_eq!(
            winner.relays,
            vec![vec!["r".to_string(), "wss://lowest.example".into()]]
        );
        if phase == "before merges" {
            db.optimize(&["relay_list_latest"]).await;
        }
    }

    tokio::time::sleep(Duration::from_secs(2)).await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &validate_options(&[Target::RelayLists]),
        &ValidateInputs::default(),
    )
    .await
    .unwrap();
    assert!(report.passed(), "{}", report.render());
    assert!(report.targets[0].compared >= 40, "{}", report.render());
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, clickhouse::Row)]
struct KindRow {
    kind: u16,
    event_count: u64,
    unique_pubkeys: u64,
    first_seen: u32,
    last_seen: u32,
}

#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
struct StoredEv {
    pubkey: String,
    kind: u16,
    created_at: u32,
    indexed_at: u32,
}

/// Mixed kinds, authors and timestamps (some in the future, some years old).
fn kinds_workload(first_id: u64, count: u64, now: u32) -> Vec<Ev> {
    const KINDS: [u16; 8] = [1, 1, 7, 3, 0, 1059, 30023, 10002];
    (first_id..first_id + count)
        .map(|n| {
            let r = splitmix(n ^ 0x6b);
            let created_at = match r % 11 {
                0 => now + 50 * DAY,
                1 => now - 2000 * DAY,
                _ => now - (r >> 20) as u32 % (90 * DAY),
            };
            ev(
                n,
                (r >> 8) % 23,
                KINDS[(r >> 16) as usize % KINDS.len()],
                created_at,
            )
        })
        .collect()
}

fn expected_kinds(rows: &[StoredEv]) -> Vec<KindRow> {
    let mut by_kind: BTreeMap<u16, (u64, BTreeSet<&str>, u32, u32)> = BTreeMap::new();
    for r in rows {
        let k = by_kind
            .entry(r.kind)
            .or_insert((0, BTreeSet::new(), u32::MAX, 0));
        k.0 += 1;
        k.1.insert(&r.pubkey);
        k.2 = k.2.min(r.created_at);
        k.3 = k.3.max(r.created_at);
    }
    let mut out: Vec<KindRow> = by_kind
        .into_iter()
        .map(|(kind, (n, p, first, last))| KindRow {
            kind,
            event_count: n,
            unique_pubkeys: p.len() as u64,
            first_seen: first,
            last_seen: last,
        })
        .collect();
    out.sort_by(|a, b| b.event_count.cmp(&a.event_count).then(a.kind.cmp(&b.kind)));
    out
}

async fn stored(db: &TestDb) -> Vec<StoredEv> {
    db.client
        .query("SELECT pubkey, kind, toUInt32(created_at), toUInt32(indexed_at) FROM events_local")
        .fetch_all()
        .await
        .unwrap()
}

async fn read_kinds(c: &Client, sort: KindsSort) -> Vec<KindRow> {
    c.query(&kinds_list_sql(sort))
        .bind(1000u32)
        .fetch_all()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn kinds_backfill_counts_each_row_exactly_once_around_the_view_boundary() {
    let db = TestDb::new().await;
    let now = db.now().await;
    for chunk in kinds_workload(0, 900, now).chunks(100) {
        db.insert(chunk).await;
    }
    db.apply_020().await;
    db.set_merges(&["kind_totals"], false).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let live_start = db.now().await;
    for chunk in kinds_workload(5_000, 400, now).chunks(80) {
        db.insert(chunk).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let plan = backfill_target(&db, dir.path(), Target::Kinds).await;
    let boundary = plan.view_boundary.expect("kinds plans fix a boundary");
    let rows = stored(&db).await;
    let first_live = rows
        .iter()
        .filter(|r| r.indexed_at >= live_start)
        .map(|r| r.indexed_at)
        .min()
        .unwrap();
    assert_eq!(boundary, first_live, "boundary = first captured second");

    // The backfill share is exactly the rows indexed before the boundary.
    let shares: Vec<(String, u64)> = db
        .client
        .query("SELECT toString(origin), toUInt64(sum(event_count)) FROM kind_totals GROUP BY origin ORDER BY origin")
        .fetch_all()
        .await
        .unwrap();
    let before = rows.iter().filter(|r| r.indexed_at < boundary).count() as u64;
    assert_eq!(
        shares,
        vec![
            ("backfill".to_string(), before),
            ("view".to_string(), rows.len() as u64 - before)
        ]
    );

    // API reads, before and after merges.
    for phase in ["before merges", "after merges"] {
        let want = expected_kinds(&rows);
        assert_eq!(
            read_kinds(&db.client, KindsSort::Count).await,
            want,
            "{phase}"
        );
        let mut by_kind = want.clone();
        by_kind.sort_by_key(|k| k.kind);
        assert_eq!(
            read_kinds(&db.client, KindsSort::Kind).await,
            by_kind,
            "{phase}"
        );
        let recent = want
            .iter()
            .filter(|k| k.last_seen >= now - 30 * DAY)
            .count() as u64;
        let total: u64 = db
            .client
            .query(&total_kinds_sql())
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(total, recent, "{phase}: /stats/kinds/total");
        let old_total: u64 = db
            .client
            .query(
                "SELECT uniq(kind) FROM events_local WHERE created_at >= now() - INTERVAL 30 DAY",
            )
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(total, old_total, "{phase}: same as the pre-020 query");
        let earliest: u32 = db
            .client
            .query(&earliest_event_sql())
            .fetch_one()
            .await
            .unwrap();
        let old_earliest: u32 = db
            .client
            .query("SELECT toUInt32(min(created_at)) FROM events_local")
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(earliest, old_earliest, "{phase}: /stats/events/earliest");
        if phase == "before merges" {
            db.optimize(&["kind_totals", "events_local"]).await;
        }
    }

    // Re-running is a no-op; an additive shard with no trace stops the run.
    let cp = CheckpointDir::new(dir.path(), Target::Kinds);
    let report = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap();
    assert_eq!(report.shards_run, 0);
    let done = cp.completed_shards().unwrap();
    std::fs::remove_file(
        cp.root()
            .join("shards")
            .join(format!("{:06}.json", done[4].index)),
    )
    .unwrap();
    cp.save_pending(&PendingShard {
        index: done[4].index,
        range: done[4].range.clone(),
        query_id: "never-sent".into(),
        started_at: 0,
    })
    .unwrap();
    let err = backfill::run_backfill(&db.client, &plan, &cp, &backfill_options())
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("cannot tell whether"),
        "{err:#}"
    );
    backfill::resolve_by_hand(&cp, done[4].index, true, 0).unwrap();
    assert_eq!(
        read_kinds(&db.client, KindsSort::Count).await,
        expected_kinds(&rows)
    );

    // validate: exact shares, and unique pubkeys against kinds_stats_mv.
    let since = db.now().await;
    db.exec(&format!("SYSTEM REFRESH VIEW {}.kinds_stats_mv", db.name))
        .await;
    db.wait_refreshed("kinds_stats_mv", since).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &validate_options(&[Target::Kinds]),
        &ValidateInputs {
            kinds_plan: Some(&plan),
            ..ValidateInputs::default()
        },
    )
    .await
    .unwrap();
    assert!(report.passed(), "{}", report.render());
    assert_eq!(report.targets[0].compared, expected_kinds(&rows).len());

    // A shard counted twice is caught.
    db.exec(&format!(
        "INSERT INTO {}.kind_totals ({}) {}",
        db.name,
        Target::Kinds.columns(),
        pensieve_serve::rollups::derived::kinds_backfill_select_sql(
            &db.name,
            &plan.shard_predicate(&plan.shards[3]),
            boundary
        )
    ))
    .await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &validate_options(&[Target::Kinds]),
        &ValidateInputs {
            kinds_plan: Some(&plan),
            ..ValidateInputs::default()
        },
    )
    .await
    .unwrap();
    assert!(!report.passed(), "{}", report.render());
    assert!(report.targets[0].rows.contains_key(&Outcome::Excess));
    db.drop().await;
}

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn kinds_unique_pubkeys_are_within_the_sketch_error() {
    let db = TestDb::new().await;
    // 300k events of 250k pubkeys: half before the migration (backfill),
    // half after (view, several inserts), sharing 50k pubkeys.
    let bulk = |from: u64, to: u64| {
        format!(
            "INSERT INTO {}.events_local (id, pubkey, created_at, kind, content, sig, tags) \
             SELECT lower(hex(SHA256(concat('e', toString(number))))), \
             lower(hex(SHA256(concat('p', toString(number % 250000))))), \
             now() - number, 1, '', '', [] FROM numbers({from}, {})",
            db.name,
            to - from
        )
    };
    db.exec(&bulk(0, 150_000)).await;
    db.apply_020().await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    for start in (150_000..300_000).step_by(50_000) {
        db.exec(&bulk(start, start + 50_000)).await;
    }
    let dir = tempfile::tempdir().unwrap();
    backfill_target(&db, dir.path(), Target::Kinds).await;
    let exact: u64 = db
        .client
        .query("SELECT uniqExact(pubkey) FROM events_local WHERE kind = 1")
        .fetch_one()
        .await
        .unwrap();
    let row = read_kinds(&db.client, KindsSort::Count).await;
    assert_eq!(row[0].kind, 1);
    assert_eq!(row[0].event_count, 300_000);
    let error = row[0].unique_pubkeys.abs_diff(exact) as f64 / exact as f64;
    // uniqCombined64 at 2^17 registers: ~0.3% standard error; 1% is >3 sigma.
    assert!(
        error < 0.01,
        "approximate {} vs exact {exact}",
        row[0].unique_pubkeys
    );
    assert_eq!(exact, 250_000);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Cutover end to end, and rollback
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable ClickHouse at PENSIEVE_TEST_CLICKHOUSE_URL"]
async fn cutover_is_guarded_switches_every_view_and_rolls_back() {
    let db = TestDb::new().await;
    let now = db.now().await;
    for chunk in kinds_workload(0, 300, now).chunks(50) {
        db.insert(chunk).await;
    }
    for chunk in relay_workload(20_000, now).chunks(30) {
        db.insert(chunk).await;
    }
    db.apply_020().await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    db.insert(&kinds_workload(1_000, 100, now)).await;
    let dir = tempfile::tempdir().unwrap();
    let options = CutoverOptions {
        quiet_minutes: None,
        ..CutoverOptions::default()
    };

    // Refused until every 020 backfill is complete.
    let err = cutover::run(&db.client, &db.name, dir.path(), &options)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("plan and backfill first"),
        "{err:#}"
    );
    let mut plans = BTreeMap::new();
    for target in Target::MIGRATION_020 {
        plans.insert(target, backfill_target(&db, dir.path(), target).await);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &validate_options(&Target::MIGRATION_020),
        &ValidateInputs {
            kinds_plan: plans.get(&Target::Kinds),
            ..ValidateInputs::default()
        },
    )
    .await
    .unwrap();
    assert!(report.passed(), "{}", report.render());

    // Refused while something still reads kinds_stats_mv (the old API).
    db.client
        .query("SELECT count() FROM kinds_stats_mv")
        .fetch_one::<u64>()
        .await
        .unwrap();
    let err = cutover::run(&db.client, &db.name, dir.path(), &options)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("pre-020 API is still live"),
        "{err:#}"
    );
    // The phase 1 rollback refuses nothing yet; the drop guard only fires
    // after the cutover (checked below).
    let options = CutoverOptions {
        reader_window_mins: 0,
        ..options
    };
    let dry = CutoverOptions {
        dry_run: true,
        ..options.clone()
    };
    assert!(
        cutover::run(&db.client, &db.name, dir.path(), &dry)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.as_select("kinds_stats_mv").await.is_some(),
        "dry run changes nothing"
    );

    let started = db.now().await;
    let record = cutover::run(&db.client, &db.name, dir.path(), &options)
        .await
        .unwrap()
        .expect("cutover ran");
    assert_eq!(
        cutover::load_record(dir.path()).unwrap(),
        Some(record.clone())
    );
    assert!(db.as_select("kinds_stats_mv").await.is_none());
    assert!(
        !db.as_select("daily_user_stats_mv")
            .await
            .unwrap()
            .contains("pubkeys_with_profile_data")
    );
    assert!(
        db.as_select("relay_distribution_mv")
            .await
            .unwrap()
            .contains("relay_list_latest")
    );
    // Running it again is harmless.
    cutover::run(&db.client, &db.name, dir.path(), &options)
        .await
        .unwrap();

    // relay_distribution now comes from relay_list_latest, with 014's output.
    db.wait_refreshed("relay_distribution_mv", started).await;
    let table: Vec<RelayRow> = db
        .client
        .query(
            "SELECT relay_url, user_count, read_count, write_count FROM relay_distribution FINAL \
             ORDER BY relay_url",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(
        table,
        distribution(&db.client, RelaySource::EventsLocal, &db.name).await
    );
    assert!(!table.is_empty());

    // New inserts go through the lookup-free daily_user_stats_mv.
    let later = kinds_workload(3_000, 120, now);
    db.insert(&later).await;
    let written: u64 = db
        .client
        .query("SELECT count() FROM daily_user_stats WHERE last_indexed_at > toDateTime(0)")
        .fetch_one()
        .await
        .unwrap();
    assert!(written > 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let report = validate::run_with(
        &db.client,
        &db.name,
        &ValidateOptions {
            boundary_window_secs: 3600,
            ..validate_options(&Target::MIGRATION_020)
        },
        &ValidateInputs {
            kinds_plan: plans.get(&Target::Kinds),
            cutover_at: Some(record.finished_at),
            ..ValidateInputs::default()
        },
    )
    .await
    .unwrap();
    assert!(report.passed(), "{}", report.render());

    // Phase 1 cannot be dropped while the switched views read it.
    let refused = db.apply(DROP_020).await.unwrap_err();
    assert!(refused.contains("undo_derived_views_cutover"), "{refused}");

    // Undo the cutover: the pre-020 definitions are back.
    let since = db.now().await;
    db.apply(UNDO_CUTOVER).await.expect("undo cutover");
    assert!(
        db.as_select("daily_user_stats_mv")
            .await
            .unwrap()
            .contains("pubkeys_with_profile_data")
    );
    assert!(
        db.as_select("relay_distribution_mv")
            .await
            .unwrap()
            .contains("events_local")
    );
    db.wait_refreshed("kinds_stats_mv", since).await;
    let restored: u64 = db
        .client
        .query("SELECT count() FROM kinds_stats_mv")
        .fetch_one()
        .await
        .unwrap();
    assert!(restored > 0, "kinds_stats_mv refreshes when recreated");
    db.insert(&kinds_workload(4_000, 30, now)).await;

    // Then phase 1 can be dropped; the API's kind reads report missing tables.
    db.apply(DROP_020).await.expect("drop 020");
    let columns: Vec<String> = db
        .client
        .query(
            "SELECT name FROM system.columns WHERE database = currentDatabase() \
             AND table = 'daily_user_stats' ORDER BY position",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(
        columns,
        [
            "date",
            "pubkey",
            "has_profile",
            "has_follows",
            "event_count"
        ]
    );
    let err = db
        .client
        .query(&kinds_list_sql(KindsSort::Count))
        .bind(10u32)
        .fetch_all::<KindRow>()
        .await
        .unwrap_err();
    assert!(is_unknown_table(&err), "{err}");
    db.insert(&kinds_workload(5_000, 30, now)).await;
    db.drop().await;
}
