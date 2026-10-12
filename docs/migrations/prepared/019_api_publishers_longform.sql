-- Prepared migration: 019_api_publishers_longform
-- Description: Small insert-fed rollup tables for pensieve-serve's
--              /api/v1/stats/publishers and /api/v1/stats/longform, which
--              otherwise full-scan events_local (~2.4B rows, no projections)
--              and hit the API's 20 s query cap.
-- Date: 2026-10-12
--
-- Prepared for ClickHouse 24.8.14.39.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/019_api_publishers_longform.sql
--
-- Rollback:
--   just ch-migrate docs/migrations/rollback/019_drop_api_publishers_longform.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it. It attaches two insert-triggered
--     materialized views to events_local, which run inside every ingest flush.
--   - It creates empty tables and starts capturing new inserts only. History
--     is loaded separately with the bounded `api-rollups backfill` tool
--     (crates/pensieve-serve/src/bin/api-rollups.rs). The API must not be
--     switched to these tables before that backfill is validated.
--   - Follow docs/api_rollups.md and the ops/RUNBOOK.md section
--     "API rollups (migration 019)" for preflight, backfill and rollout.
--   - Every expression below must be non-throwing for arbitrary rows. A
--     throwing materialized view fails the whole events_local insert.
--
-- Correctness model (see docs/api_rollups.md):
--   - longform_latest uses the migration 017 argMin pattern: idempotent and
--     order-independent, so replays, out-of-order arrival and backfill overlap
--     are harmless and reads are correct before merges.
--   - publisher_activity holds SUMS. Sums are NOT idempotent: a row counted by
--     both the view and the backfill, or inserted twice, is counted twice.
--     The backfill therefore only reads rows with indexed_at below the first
--     second the view captured (recorded in live_min_indexed_at), and runs each
--     shard exactly once. Reads still collapse unmerged rows with GROUP BY, so
--     they never depend on background merges.

-- =============================================================================
-- PUBLISHER ACTIVITY: event counts per (grain, kind, period, pubkey)
-- =============================================================================
--
-- One table, two time grains and two kind scopes, written by ONE view so a
-- single insert pipeline (one creation time) feeds every rollup:
--   grain = 'day'   period = UTC day of the event
--   grain = 'month' period = first day of the UTC month of the event
--   kind  = the event kind, or -1 for the all-kinds rollup of that pubkey
-- A window [start_day, +inf) reads 'day' rows from start_day up to the next
-- month boundary and 'month' rows from there on, so a 365-day ranking reads at
-- most ~30 daily and ~12 monthly rows per active pubkey instead of 365 daily.
--
-- Event time: activity_at = least(created_at, indexed_at + 1 day). Events
-- dated more than a day after they were indexed (future-dated) count on the
-- day after ingestion instead of their claimed date. This bounds the periods
-- one insert can touch, keeps TTL meaningful and stops far-future timestamps
-- from staying in every window forever. first/last_created_at keep the real
-- created_at.
--
-- Kinds 445 and 1059 are excluded: they are signed by throwaway keys (one
-- pubkey per event), would dominate the table's cardinality, and carry no
-- publisher signal. daily_user_stats (migration 005) excludes the same kinds.
--
-- TTL 400 days: the API accepts windows of at most 366 days, and a window
-- only reads month rows from the first month boundary on or after its start,
-- so no read needs a period older than today - 366 days. A month row only
-- holds its whole month if the backfill window (today - 400 at planning)
-- starts before that month: 366 + 31 days of month rounding + 3 days slack.
-- The view drops events whose activity day is already outside the TTL, so
-- late-arriving old events do not create rows that only expire.
-- Unpartitioned on purpose: every backfill shard is then exactly one part,
-- committed atomically (see the backfill's exactly-once rule), and a burst of
-- odd timestamps can never exceed max_partitions_per_insert_block and fail an
-- ingest insert. TTL deletes are applied by at most one rewrite per day.
--
-- live_min_indexed_at is the earliest indexed_at the VIEW wrote for a row. The
-- backfill writes the sentinel 4294967295 (2106-02-07 06:28:15) instead, so
-- min(live_min_indexed_at) over the table is the first second the view
-- captured. The backfill reads only rows indexed before that second.

CREATE TABLE IF NOT EXISTS nostr.publisher_activity (
    grain Enum8('day' = 1, 'month' = 2),
    kind Int32,
    period Date,
    pubkey String,
    event_count SimpleAggregateFunction(sum, UInt64),
    kinds SimpleAggregateFunction(groupUniqArrayArray, Array(UInt16)),
    first_created_at SimpleAggregateFunction(min, DateTime),
    last_created_at SimpleAggregateFunction(max, DateTime),
    live_min_indexed_at SimpleAggregateFunction(min, DateTime),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY (grain, kind, period, pubkey)
TTL period + INTERVAL 400 DAY
SETTINGS merge_with_ttl_timeout = 86400;

-- Guard: the boundary rule needs the view to start on an EMPTY table. If a
-- previous view was dropped, truncate publisher_activity before re-applying.
SELECT throwIf(
    (SELECT count() FROM system.tables
     WHERE database = 'nostr' AND name = 'publisher_activity_mv') = 0
    AND (SELECT count() FROM nostr.publisher_activity) > 0,
    'nostr.publisher_activity has rows but publisher_activity_mv does not exist: TRUNCATE TABLE nostr.publisher_activity first (see docs/api_rollups.md)'
) AS guard_publisher_activity_empty
FORMAT Null;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.publisher_activity_mv TO nostr.publisher_activity
AS SELECT
    CAST(slot.1, 'Enum8(''day'' = 1, ''month'' = 2)') AS grain,
    slot.2 AS kind,
    if(slot.1 = 1, toDate(activity_at, 'UTC'), toStartOfMonth(activity_at, 'UTC')) AS period,
    pubkey,
    count() AS event_count,
    groupUniqArray(event_kind) AS kinds,
    min(created_at) AS first_created_at,
    max(created_at) AS last_created_at,
    min(indexed_at) AS live_min_indexed_at,
    max(indexed_at) AS last_indexed_at
FROM (
    SELECT
        pubkey,
        kind AS event_kind,
        created_at,
        indexed_at,
        least(created_at, indexed_at + toIntervalDay(1)) AS activity_at
    FROM nostr.events_local
    WHERE kind NOT IN (445, 1059)
      AND toDate(activity_at, 'UTC') > toDate(now(), 'UTC') - 400
)
ARRAY JOIN [(1, -1), (1, toInt32(event_kind)), (2, -1), (2, toInt32(event_kind))] AS slot
GROUP BY grain, kind, period, pubkey;

-- =============================================================================
-- LONGFORM: latest version per (pubkey, d_tag) of kind 30023
-- =============================================================================
--
-- Same winner rule and key encoding as migration 017's addressable_latest:
-- highest created_at wins, lowest event id breaks ties. The payload keeps the
-- winning version's id, created_at and content length in bytes (what the old
-- query's length(content) measured). d_tag uses the events_local.d_tag
-- MATERIALIZED expression, so a missing d tag is the '' coordinate.

CREATE TABLE IF NOT EXISTS nostr.longform_latest (
    pubkey String,
    d_tag String,
    latest AggregateFunction(argMin, Tuple(String, DateTime, UInt64), Tuple(Int64, String)),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY (pubkey, d_tag);

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.longform_latest_mv TO nostr.longform_latest
AS SELECT
    pubkey,
    arrayElement(arrayFilter(t -> t[1] = 'd', tags), 1)[2] AS d_tag,
    argMinState(
        CAST((id, created_at, toUInt64(length(content))), 'Tuple(String, DateTime, UInt64)'),
        CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')
    ) AS latest,
    max(indexed_at) AS last_indexed_at
FROM nostr.events_local
WHERE kind = 30023
GROUP BY pubkey, d_tag;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 019 objects' AS status, name, engine, metadata_modification_time
FROM system.tables
WHERE database = 'nostr'
  AND name IN (
      'publisher_activity', 'publisher_activity_mv',
      'longform_latest', 'longform_latest_mv'
  )
ORDER BY name;
