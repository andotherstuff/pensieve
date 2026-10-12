-- Prepared migration: 020_incremental_derived_views (phase 1 of 2)
-- Description: Insert-fed replacements for the three most expensive derived
--              views on events_local (~2.45B rows, no projections):
--                1. daily_user_stats_mv's per-insert IN subqueries (7.7 s of
--                   every ~8.4 s ingest insert, ~30.4M lookup rows read each);
--                2. relay_distribution_mv, a 6-hourly full rescan (1.32 TiB);
--                3. kinds_stats_mv, an hourly full rescan (180 GiB).
-- Date: 2026-10-12
--
-- Prepared for ClickHouse 24.8.14.39.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/020_incremental_derived_views.sql
--
-- Rollback (phase 1 only; undo the cutover first if it ran):
--   just ch-migrate docs/migrations/rollback/020_drop_incremental_derived_views.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it. It attaches three insert-triggered
--     materialized views to events_local, which run inside every ingest flush.
--   - Phase 1 changes no existing object's behaviour: the old views keep
--     running and the API keeps reading what it reads today. It creates empty
--     tables, starts capturing new inserts, and adds one column to
--     daily_user_stats that the current view leaves at its default.
--   - History is loaded with the bounded `api-rollups backfill --migration 020`
--     tool (crates/pensieve-serve/src/bin/api-rollups.rs), then validated.
--   - Phase 2 (020_incremental_derived_views_cutover.sql) switches the old
--     views over and drops the two rescans. Run it with `api-rollups cutover`,
--     only after backfill + validate and after the API that reads
--     kind_totals is deployed.
--   - Follow docs/api_rollups.md ("Migration 020") and the ops/RUNBOOK.md
--     section "Incremental derived views (migration 020)".
--   - Every expression below must be non-throwing for arbitrary rows. A
--     throwing materialized view fails the whole events_local insert.

-- =============================================================================
-- 1. USER FLAGS: when each pubkey first had a profile / follow list
-- =============================================================================
--
-- daily_user_stats_mv (migration 005) evaluates, for every inserted event,
--   pubkey IN (SELECT pubkey FROM pubkeys_with_profile_data)   (and follows),
-- which rebuilds a ~30M-row set inside every ingest insert. The flag it
-- stores means "the pubkey had published a kind 0 (kind 3) before this
-- event's insert". The same fact is recoverable later from two timestamps:
--
--   profile_since(p)       = first indexed_at of a kind 0 event of p
--   last_indexed_at(d, p)  = latest indexed_at of p's events dated d
--
-- An event inserted at t was flagged iff profile_since(p) < t (strictly: an
-- insert's own kind 0 is not visible to that insert's daily_user_stats_mv,
-- which runs first, and ingest inserts are sequential, so two inserts never
-- share an indexed_at second). So the cutover drops the IN subqueries from the
-- insert path, stores last_indexed_at instead, and the active-users refreshes
-- compute the flags with a join (see the cutover file). Published flags keep
-- their insert-time meaning: a pubkey that publishes its profile later is not
-- retro-flagged on earlier days.
--
-- Pubkeys already flagged before this migration are seeded by
-- `api-rollups backfill --target user-flags` from pubkeys_with_profile_data /
-- pubkeys_with_follows_data with since = 1970-01-01 (they had a profile before
-- any row the new path writes). The sentinel 2106-02-07 06:28:15 means
-- "never". All columns are min/max, so the seed and the view may overlap and
-- replays are harmless.

CREATE TABLE IF NOT EXISTS nostr.pubkey_flags (
    pubkey String,
    profile_since SimpleAggregateFunction(min, DateTime),
    follows_since SimpleAggregateFunction(min, DateTime),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY pubkey;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.pubkey_flags_mv TO nostr.pubkey_flags
AS SELECT
    pubkey,
    min(if(kind = 0, indexed_at, toDateTime(4294967295))) AS profile_since,
    min(if(kind = 3, indexed_at, toDateTime(4294967295))) AS follows_since,
    max(indexed_at) AS last_indexed_at
FROM nostr.events_local
WHERE kind IN (0, 3)
GROUP BY pubkey;

-- daily_user_stats is a SummingMergeTree ORDER BY (date, pubkey). A
-- SimpleAggregateFunction column is merged with its own function (max), not
-- summed. Metadata-only; the current view does not write it, so its rows keep
-- the default 1970-01-01 00:00:00, which the cutover's flag expression reads as
-- "flags stored at insert time".
ALTER TABLE nostr.daily_user_stats
    ADD COLUMN IF NOT EXISTS last_indexed_at SimpleAggregateFunction(max, DateTime);

-- =============================================================================
-- 2. RELAY LISTS: latest kind 10002 relay list per pubkey
-- =============================================================================
--
-- Migration 017's argMin pattern: newest created_at wins, lowest event id
-- breaks ties (014 used argMax(tags, created_at), whose ties were arbitrary).
-- The payload is the winner's id, created_at and its 'r' tags exactly as 014
-- selected them, so relay_distribution is recomputed from this table with
-- 014's normalisation and filters unchanged. Idempotent and order-independent:
-- replays, out-of-order arrival and backfill overlap are harmless.

CREATE TABLE IF NOT EXISTS nostr.relay_list_latest (
    pubkey String,
    latest AggregateFunction(
        argMin,
        Tuple(String, DateTime, Array(Array(String))),
        Tuple(Int64, String)
    ),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY pubkey;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.relay_list_latest_mv TO nostr.relay_list_latest
AS SELECT
    pubkey,
    argMinState(
        CAST(
            (id, created_at, arrayFilter(t -> t[1] = 'r' AND length(t) >= 2, tags)),
            'Tuple(String, DateTime, Array(Array(String)))'
        ),
        CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')
    ) AS latest,
    max(indexed_at) AS last_indexed_at
FROM nostr.events_local
WHERE kind = 10002
GROUP BY pubkey;

-- =============================================================================
-- 3. KIND TOTALS: per kind event count, approximate pubkeys, first/last seen
-- =============================================================================
--
-- event_count is a SUM: not idempotent. Same exactly-once model as migration
-- 019's publisher_activity: the backfill reads only rows with indexed_at below
-- the first second the view captured (min(live_min_indexed_at) over view rows;
-- backfill rows write the sentinel 4294967295) and runs each shard exactly
-- once. `origin` keeps the backfill's and the view's rows apart forever, so
-- `api-rollups validate` can compare the backfill share with
-- countIf(indexed_at < boundary) on events_local exactly.
--
-- pubkeys is a uniqCombined64 sketch (HyperLogLog, 2^17 registers, 64-bit
-- hash): standard error ~0.3%, at most ~96 KiB per row, exact below a few
-- thousand values. The 64-bit hash matters: kinds 1059 and 445 have hundreds
-- of millions of throwaway pubkeys, where a 32-bit hash collides by several
-- percent. Sketches merge losslessly, so view + backfill + merges give the
-- same estimate as one pass. min/max are idempotent.
--
-- Unpartitioned: every backfill shard is exactly one part, committed
-- atomically.

CREATE TABLE IF NOT EXISTS nostr.kind_totals (
    kind UInt16,
    origin Enum8('backfill' = 1, 'view' = 2),
    event_count SimpleAggregateFunction(sum, UInt64),
    pubkeys AggregateFunction(uniqCombined64, String),
    first_created_at SimpleAggregateFunction(min, DateTime),
    last_created_at SimpleAggregateFunction(max, DateTime),
    live_min_indexed_at SimpleAggregateFunction(min, DateTime),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY (kind, origin);

-- Guard: the boundary rule needs the view to start on an EMPTY table. If a
-- previous view was dropped, truncate kind_totals before re-applying.
SELECT throwIf(
    (SELECT count() FROM system.tables
     WHERE database = 'nostr' AND name = 'kind_totals_mv') = 0
    AND (SELECT count() FROM nostr.kind_totals) > 0,
    'nostr.kind_totals has rows but kind_totals_mv does not exist: TRUNCATE TABLE nostr.kind_totals first (see docs/api_rollups.md)'
) AS guard_kind_totals_empty
FORMAT Null;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.kind_totals_mv TO nostr.kind_totals
AS SELECT
    kind,
    CAST('view', 'Enum8(''backfill'' = 1, ''view'' = 2)') AS origin,
    count() AS event_count,
    uniqCombined64State(pubkey) AS pubkeys,
    min(created_at) AS first_created_at,
    max(created_at) AS last_created_at,
    min(indexed_at) AS live_min_indexed_at,
    max(indexed_at) AS last_indexed_at
FROM nostr.events_local
GROUP BY kind;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 020 objects' AS status, name, engine, metadata_modification_time
FROM system.tables
WHERE database = 'nostr'
  AND name IN (
      'pubkey_flags', 'pubkey_flags_mv',
      'relay_list_latest', 'relay_list_latest_mv',
      'kind_totals', 'kind_totals_mv'
  )
ORDER BY name;

SELECT 'daily_user_stats.last_indexed_at' AS status, type
FROM system.columns
WHERE database = 'nostr' AND table = 'daily_user_stats' AND name = 'last_indexed_at';
