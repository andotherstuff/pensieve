//! SQL for migration 020's incremental derived views.
//!
//! Phase 1 (`docs/migrations/prepared/020_incremental_derived_views.sql`)
//! creates three insert-fed tables next to the views they replace; phase 2
//! (`020_incremental_derived_views_cutover.sql`, run by `api-rollups cutover`)
//! switches the old views over. The `SELECT`s here are the single Rust source
//! for the backfill, the API reads, the validator and the integration tests;
//! unit tests check that the migration files contain the same text.
//!
//! - **User flags** (`pubkey_flags`): `daily_user_stats_mv` used to evaluate
//!   `pubkey IN (SELECT pubkey FROM pubkeys_with_profile_data)` (and follows)
//!   for every inserted event. A flag meant "the pubkey had published a kind 0
//!   (kind 3) in an earlier insert". After the cutover the insert path writes
//!   `last_indexed_at` instead, and the active-users refreshes evaluate
//!   `profile_since < last_indexed_at` with one join per refresh
//!   ([`active_users_select_sql`]). Rows written before the cutover keep their
//!   stored flags.
//! - **Relay lists** (`relay_list_latest`): the latest kind 10002 list per
//!   pubkey; `relay_distribution` is recomputed from it with migration 014's
//!   normalisation ([`relay_distribution_select_sql`]).
//! - **Kinds** (`kind_totals`): per-kind sums with the exactly-once boundary
//!   model of migration 019, plus a `uniqCombined64` sketch of the pubkeys
//!   ([`kinds_list_sql`]).

use super::{BACKFILL_SENTINEL_INDEXED_AT, WINNER_KEY_EXPR};

/// User flags table.
pub const USER_FLAGS_TABLE: &str = "pubkey_flags";
/// View feeding [`USER_FLAGS_TABLE`].
pub const USER_FLAGS_VIEW: &str = "pubkey_flags_mv";
/// Latest relay list table.
pub const RELAY_LISTS_TABLE: &str = "relay_list_latest";
/// View feeding [`RELAY_LISTS_TABLE`].
pub const RELAY_LISTS_VIEW: &str = "relay_list_latest_mv";
/// Kind totals table.
pub const KINDS_TABLE: &str = "kind_totals";
/// View feeding [`KINDS_TABLE`].
pub const KINDS_VIEW: &str = "kind_totals_mv";

/// The pre-020 lookup tables (migration 004) the user flags are seeded from:
/// every pubkey with a kind 0, every pubkey with a kind 3.
pub const USER_FLAG_LOOKUPS: [&str; 2] = ["pubkeys_with_profile_data", "pubkeys_with_follows_data"];

/// `profile_since` / `follows_since` of a pubkey that never had one
/// (2106-02-07 06:28:15).
pub const NEVER: u32 = BACKFILL_SENTINEL_INDEXED_AT;

/// The pre-020 refreshable view that rescans `events_local` for kind stats
/// (migration 015). Dropped by the cutover; the API reads [`KINDS_TABLE`].
pub const OLD_KINDS_VIEW: &str = "kinds_stats_mv";

/// The refreshable view that writes `relay_distribution` (014, replaced by
/// the cutover).
pub const RELAY_DISTRIBUTION_VIEW: &str = "relay_distribution_mv";

/// `kind_totals.origin` type.
const ORIGIN_TYPE: &str = "Enum8(''backfill'' = 1, ''view'' = 2)";

/// The 'r' tags of a relay list, as migration 014 selected them.
pub const RELAY_TAGS_EXPR: &str = "arrayFilter(t -> t[1] = 'r' AND length(t) >= 2, tags)";

/// Payload kept for the latest relay list: id, `created_at`, 'r' tags.
const RELAY_PAYLOAD_TYPE: &str = "Tuple(String, DateTime, Array(Array(String)))";

/// Seed the user flags from the pre-020 lookup tables: those pubkeys had a
/// profile (follow list) before any row the new path writes, so their
/// `since` is 1970-01-01. The view captures newcomers with their real time.
pub fn user_flags_backfill_select_sql(database: &str, range_predicate: &str) -> String {
    let [profiles, follows] = USER_FLAG_LOOKUPS;
    format!(
        "SELECT pubkey, min(profile_since) AS profile_since, min(follows_since) AS follows_since, \
         toDateTime(0) AS last_indexed_at \
         FROM ( \
         SELECT pubkey, toDateTime(0) AS profile_since, toDateTime({NEVER}) AS follows_since \
         FROM {database}.{profiles} WHERE ({range_predicate}) \
         UNION ALL \
         SELECT pubkey, toDateTime({NEVER}) AS profile_since, toDateTime(0) AS follows_since \
         FROM {database}.{follows} WHERE ({range_predicate}) \
         ) \
         GROUP BY pubkey"
    )
}

/// Latest kind 10002 relay list per pubkey (same expressions as the view).
pub fn relay_lists_backfill_select_sql(database: &str, range_predicate: &str) -> String {
    format!(
        "SELECT pubkey, \
         argMinState(CAST((id, created_at, {RELAY_TAGS_EXPR}), '{RELAY_PAYLOAD_TYPE}'), \
         {WINNER_KEY_EXPR}) AS latest, \
         max(indexed_at) AS last_indexed_at \
         FROM {database}.events_local \
         WHERE kind = 10002 AND ({range_predicate}) \
         GROUP BY pubkey"
    )
}

/// Per-kind totals of rows indexed before the view boundary.
pub fn kinds_backfill_select_sql(
    database: &str,
    range_predicate: &str,
    indexed_before: u32,
) -> String {
    format!(
        "SELECT kind, CAST('backfill', '{ORIGIN_TYPE}') AS origin, \
         count() AS event_count, uniqCombined64State(pubkey) AS pubkeys, \
         min(created_at) AS first_created_at, max(created_at) AS last_created_at, \
         toDateTime({BACKFILL_SENTINEL_INDEXED_AT}) AS live_min_indexed_at, \
         max(indexed_at) AS last_indexed_at \
         FROM {database}.events_local \
         WHERE indexed_at < toDateTime({indexed_before}) AND ({range_predicate}) \
         GROUP BY kind"
    )
}

// ---------------------------------------------------------------------------
// Active users (migration 013's refreshes)
// ---------------------------------------------------------------------------

/// Time grain of an active-users refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grain {
    /// `active_users_daily`: last 30 days, hourly.
    Day,
    /// `active_users_weekly`: last 26 weeks, every 12 hours.
    Week,
    /// `active_users_monthly`: last 12 months, daily.
    Month,
}

impl Grain {
    /// Every grain.
    pub const ALL: [Self; 3] = [Self::Day, Self::Week, Self::Month];

    /// (period expression, period column, window predicate on `date`).
    fn parts(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Day => (
                "s.date",
                "date",
                "date >= today() - 30 AND {d}date <= today()",
            ),
            Self::Week => (
                "toMonday(s.date)",
                "week",
                "date >= toMonday(today()) - 182 AND {d}date <= today()",
            ),
            Self::Month => (
                "toStartOfMonth(s.date)",
                "month",
                "date >= toStartOfMonth(today()) - 365 AND {d}date <= today()",
            ),
        }
    }

    /// Target table of the refresh.
    pub fn table(self) -> &'static str {
        match self {
            Self::Day => "active_users_daily",
            Self::Week => "active_users_weekly",
            Self::Month => "active_users_monthly",
        }
    }

    /// Period column of [`Self::table`].
    pub fn column(self) -> &'static str {
        self.parts().1
    }
}

/// How an active-users refresh decides `has_profile` / `has_follows`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagRule {
    /// Migration 013 as deployed: stored flag `= 1`. Wrong once
    /// SummingMergeTree has summed a row's flags (any pubkey flagged on more
    /// than one event in a merged row drops out); kept for comparisons.
    Legacy,
    /// Stored flags only, `> 0`: the intended pre-020 meaning (insert-time
    /// lookups, any flagged event of the row).
    Stored,
    /// The cutover: stored flags `> 0`, or `profile_since < last_indexed_at`
    /// from `pubkey_flags` for rows written after the cutover.
    Cutover,
}

/// Settings of the cutover's flag join: no NULLs for unmatched rows, and a
/// grace hash join that keeps at most 512 MiB of the ~30M-row right side in
/// memory and spills the rest.
pub const CUTOVER_JOIN_SETTINGS: &str = "SETTINGS join_use_nulls = 0, \
     join_algorithm = 'grace_hash', max_bytes_in_join = 536870912";

/// The `SELECT` of an active-users refresh over `database`.
pub fn active_users_select_sql(grain: Grain, rule: FlagRule, database: &str) -> String {
    let (period, column, window) = grain.parts();
    let group = if grain == Grain::Day {
        "s.date"
    } else {
        column
    };
    let outer = |profile: &str, follows: &str| {
        format!(
            "SELECT {period} AS {column}, \
             uniq(s.pubkey) AS active_users, \
             uniqIf(s.pubkey, {profile}) AS has_profile, \
             uniqIf(s.pubkey, {follows}) AS has_follows, \
             uniqIf(s.pubkey, {profile} AND {follows}) AS has_both, \
             sum(s.event_count) AS total_events, \
             now() AS updated_at"
        )
    };
    match rule {
        FlagRule::Legacy | FlagRule::Stored => {
            let op = if rule == FlagRule::Legacy {
                "= 1"
            } else {
                "> 0"
            };
            format!(
                "{} FROM {database}.daily_user_stats s WHERE s.{} GROUP BY {group}",
                outer(
                    &format!("s.has_profile {op}"),
                    &format!("s.has_follows {op}")
                ),
                window.replace("{d}", "s.")
            )
        }
        FlagRule::Cutover => format!(
            "{} FROM ( \
             SELECT d.date AS date, d.pubkey AS pubkey, d.event_count AS event_count, \
             d.has_profile > 0 OR (f.known = 1 AND d.last_indexed_at > f.profile_since) AS profile, \
             d.has_follows > 0 OR (f.known = 1 AND d.last_indexed_at > f.follows_since) AS follows \
             FROM {database}.daily_user_stats AS d \
             LEFT ANY JOIN ( \
             SELECT sipHash128(pubkey) AS key, profile_since, follows_since, \
             toUInt8(1) AS known \
             FROM {database}.{USER_FLAGS_TABLE} FINAL \
             ) AS f ON sipHash128(d.pubkey) = f.key \
             WHERE d.{} \
             ) AS s \
             GROUP BY {group} \
             {CUTOVER_JOIN_SETTINGS}",
            outer("s.profile", "s.follows"),
            window.replace("{d}", "d.")
        ),
    }
}

/// `daily_user_stats_mv`'s query before the cutover (migration 005).
pub fn daily_user_stats_lookup_query(database: &str) -> String {
    format!(
        "SELECT toDate(created_at) AS date, pubkey, \
         toUInt8(pubkey IN (SELECT pubkey FROM {database}.pubkeys_with_profile_data)) AS has_profile, \
         toUInt8(pubkey IN (SELECT pubkey FROM {database}.pubkeys_with_follows_data)) AS has_follows, \
         toUInt32(1) AS event_count \
         FROM {database}.events_local WHERE kind NOT IN (445, 1059)"
    )
}

/// `daily_user_stats_mv`'s query after the cutover: no lookups.
pub fn daily_user_stats_cutover_query(database: &str) -> String {
    format!(
        "SELECT toDate(created_at) AS date, pubkey, toUInt8(0) AS has_profile, \
         toUInt8(0) AS has_follows, toUInt32(1) AS event_count, \
         indexed_at AS last_indexed_at \
         FROM {database}.events_local WHERE kind NOT IN (445, 1059)"
    )
}

// ---------------------------------------------------------------------------
// Relay distribution (migration 014's output)
// ---------------------------------------------------------------------------

/// Where [`relay_distribution_select_sql`] reads the latest lists from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaySource {
    /// Migration 014: `argMax(tags, created_at)` over every kind 10002 row
    /// of `events_local` (a full rescan).
    EventsLocal,
    /// Migration 020: the stored winners in `relay_list_latest`.
    LatestLists,
}

/// The `SELECT` that computes `relay_distribution`: migration 014's
/// normalisation and filters over each pubkey's latest relay list.
pub fn relay_distribution_select_sql(source: RelaySource, database: &str) -> String {
    let lists = match source {
        RelaySource::EventsLocal => format!(
            "SELECT pubkey, argMax(tags, created_at) AS tags \
             FROM {database}.events_local WHERE kind = 10002 GROUP BY pubkey ) \
             ARRAY JOIN {RELAY_TAGS_EXPR} AS tag"
        ),
        RelaySource::LatestLists => format!(
            "SELECT pubkey, tupleElement(argMinMerge(latest), 3) AS relays \
             FROM {database}.{RELAY_LISTS_TABLE} GROUP BY pubkey ) \
             ARRAY JOIN relays AS tag"
        ),
    };
    format!(
        "WITH normalized AS ( \
         SELECT pubkey, \
         replaceRegexpOne(replaceRegexpOne(lower(trim(tag[2])), '/+$', ''), ':443$', '') AS relay_url, \
         tag[3] AS marker \
         FROM ( {lists} \
         ) \
         SELECT relay_url, count() AS user_count, \
         countIf(marker = '' OR marker = 'read') AS read_count, \
         countIf(marker = '' OR marker = 'write') AS write_count, \
         now() AS updated_at \
         FROM normalized \
         WHERE relay_url LIKE 'wss://%' \
         AND length(relay_url) > 6 \
         AND NOT match(relay_url, '[ \\n\\r\\t,]') \
         AND relay_url NOT LIKE '%.local%' \
         AND relay_url NOT LIKE '%localhost%' \
         AND relay_url NOT LIKE '%127.0.0.1%' \
         AND relay_url NOT LIKE '%0.0.0.0%' \
         AND relay_url NOT LIKE '%wss://%wss://%' \
         AND relay_url NOT LIKE '%http%' \
         GROUP BY relay_url \
         HAVING user_count >= 10"
    )
}

// ---------------------------------------------------------------------------
// API reads of kind_totals
// ---------------------------------------------------------------------------

/// Sort order of `GET /api/v1/kinds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindsSort {
    /// Most events first (ties: lowest kind first).
    Count,
    /// Lowest kind first.
    Kind,
}

/// Per-kind totals for `GET /api/v1/kinds`, in the `KindSummary` row shape.
/// Binds one `limit` (u32). Reads collapse unmerged rows and both origins
/// with `GROUP BY`, so they never depend on merges.
pub fn kinds_list_sql(sort: KindsSort) -> String {
    let order = match sort {
        KindsSort::Count => "event_count DESC, kind ASC",
        KindsSort::Kind => "kind ASC",
    };
    format!(
        "SELECT t.kind AS kind, \
         sum(t.event_count) AS event_count, \
         uniqCombined64Merge(t.pubkeys) AS unique_pubkeys, \
         toUInt32(min(t.first_created_at)) AS first_seen, \
         toUInt32(max(t.last_created_at)) AS last_seen \
         FROM {KINDS_TABLE} AS t \
         GROUP BY t.kind \
         ORDER BY {order} \
         LIMIT ?"
    )
}

/// Kinds with at least one event created in the last 30 days (what
/// `uniq(kind) ... WHERE created_at >= now() - INTERVAL 30 DAY` over
/// `events_local` counted).
pub fn total_kinds_sql() -> String {
    format!(
        "SELECT count() FROM ( \
         SELECT t.kind FROM {KINDS_TABLE} AS t GROUP BY t.kind \
         HAVING max(t.last_created_at) >= now() - INTERVAL 30 DAY)"
    )
}

/// Earliest `created_at` of any event (what `min(created_at)` over
/// `events_local` returned; 0 when empty).
pub fn earliest_event_sql() -> String {
    format!("SELECT toUInt32(min(t.first_created_at)) FROM {KINDS_TABLE} AS t")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHASE1: &str =
        include_str!("../../../../docs/migrations/prepared/020_incremental_derived_views.sql");
    const CUTOVER: &str = include_str!(
        "../../../../docs/migrations/prepared/020_incremental_derived_views_cutover.sql"
    );
    const UNDO: &str =
        include_str!("../../../../docs/migrations/rollback/020_undo_derived_views_cutover.sql");

    /// SQL without comments and whitespace, for containment checks that do
    /// not depend on formatting.
    fn squash(sql: &str) -> String {
        sql.lines()
            .map(|l| l.find("--").map_or(l, |i| &l[..i]))
            .collect::<String>()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn cutover_file_runs_the_rust_queries() {
        let cutover = squash(CUTOVER);
        for grain in Grain::ALL {
            let sql = active_users_select_sql(grain, FlagRule::Cutover, "nostr");
            let sql = sql.replace(
                CUTOVER_JOIN_SETTINGS,
                &format!(
                    "{CUTOVER_JOIN_SETTINGS}, allow_experimental_refreshable_materialized_view = 1"
                ),
            );
            assert!(cutover.contains(&squash(&sql)), "{grain:?}: {sql}");
        }
        let relay = relay_distribution_select_sql(RelaySource::LatestLists, "nostr");
        assert!(cutover.contains(&squash(&relay)), "{relay}");
        assert!(cutover.contains("REFRESHEVERY1HOUROFFSET20MINUTE"));
        let insert = daily_user_stats_cutover_query("nostr");
        assert!(cutover.contains(&squash(&insert)), "{insert}");
        assert!(cutover.contains(&squash("DROP VIEW IF EXISTS nostr.kinds_stats_mv")));
    }

    #[test]
    fn rollback_restores_the_pre_020_queries() {
        let undo = squash(UNDO);
        for grain in Grain::ALL {
            let sql = active_users_select_sql(grain, FlagRule::Legacy, "nostr");
            assert!(undo.contains(&squash(&sql)), "{grain:?}: {sql}");
        }
        let relay = relay_distribution_select_sql(RelaySource::EventsLocal, "nostr");
        assert!(undo.contains(&squash(&relay)), "{relay}");
        let insert = daily_user_stats_lookup_query("nostr");
        assert!(undo.contains(&squash(&insert)), "{insert}");
        assert!(undo.contains("uniqExact(pubkey)ASunique_pubkeys"));
    }

    #[test]
    fn backfills_match_the_view_expressions() {
        let phase1 = squash(PHASE1);
        for fragment in [
            RELAY_TAGS_EXPR,
            RELAY_PAYLOAD_TYPE,
            WINNER_KEY_EXPR,
            "WHERE kind = 10002",
            "min(if(kind = 0, indexed_at, toDateTime(4294967295))) AS profile_since",
            "min(if(kind = 3, indexed_at, toDateTime(4294967295))) AS follows_since",
            "WHERE kind IN (0, 3)",
            "uniqCombined64State(pubkey) AS pubkeys",
            "pubkeys AggregateFunction(uniqCombined64, String)",
            "origin Enum8('backfill' = 1, 'view' = 2)",
            "ADD COLUMN IF NOT EXISTS last_indexed_at SimpleAggregateFunction(max, DateTime)",
        ] {
            assert!(
                phase1.contains(&squash(fragment)),
                "migration lacks {fragment}"
            );
        }
        let kinds = kinds_backfill_select_sql("db", "1", 7);
        assert!(kinds.contains("uniqCombined64State(pubkey) AS pubkeys"));
        assert!(kinds.contains("CAST('backfill', 'Enum8(''backfill'' = 1, ''view'' = 2)')"));
        assert!(kinds.contains("indexed_at < toDateTime(7)"));
        let flags = user_flags_backfill_select_sql("db", "pubkey < '8'");
        assert_eq!(flags.matches("(pubkey < '8')").count(), 2);
        assert!(flags.contains("FROM db.pubkeys_with_profile_data"));
        assert!(flags.contains("FROM db.pubkeys_with_follows_data"));
    }

    #[test]
    fn active_users_rules_differ_only_in_the_flags() {
        for grain in Grain::ALL {
            let legacy = active_users_select_sql(grain, FlagRule::Legacy, "db");
            let stored = active_users_select_sql(grain, FlagRule::Stored, "db");
            assert_eq!(legacy.replace("= 1", "> 0"), stored);
            assert!(legacy.contains(&format!("AS {}", grain.column())));
            let cutover = active_users_select_sql(grain, FlagRule::Cutover, "db");
            assert!(cutover.contains("FROM db.pubkey_flags FINAL"));
            assert!(cutover.ends_with(CUTOVER_JOIN_SETTINGS));
            assert!(cutover.contains("f.known = 1 AND d.last_indexed_at > f.profile_since"));
            assert!(!cutover.contains('?'));
        }
    }

    #[test]
    fn kinds_reads_bind_only_the_limit() {
        assert_eq!(kinds_list_sql(KindsSort::Count).matches('?').count(), 1);
        assert!(kinds_list_sql(KindsSort::Count).contains("ORDER BY event_count DESC, kind ASC"));
        assert!(kinds_list_sql(KindsSort::Kind).contains("ORDER BY kind ASC"));
        assert!(!total_kinds_sql().contains('?'));
        assert!(!earliest_event_sql().contains('?'));
    }
}
