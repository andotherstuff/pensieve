-- Prepared migration: 021_kind_activity
-- Description: Small insert-fed rollup for pensieve-serve's
--              /api/v1/kinds/{kind} (content length, 24 h / 7 d / 30 d
--              counts) and /api/v1/kinds/{kind}/activity (per day / week /
--              month counts and distinct pubkeys), which otherwise scan
--              events_local (~2.46B rows, no projections) by kind and time
--              out at the API's 20 s cap for large kinds (kind 1: 500
--              database_error after 20 s on 2026-10-12).
-- Date: 2026-10-13
--
-- Prepared for ClickHouse 24.8.14.39. Applied after 017, 019 and 020.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/021_kind_activity.sql
--
-- Rollback:
--   just ch-migrate docs/migrations/rollback/021_drop_kind_activity.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it. It attaches one insert-triggered
--     materialized view to events_local, which runs inside every ingest flush.
--   - It creates an empty table and starts capturing new inserts only. History
--     is loaded with the bounded `api-rollups backfill --migration 021` tool
--     (crates/pensieve-serve/src/bin/api-rollups.rs). The API must not be
--     switched to this table before that backfill is validated.
--   - Follow docs/api_rollups.md ("Migration 021") and the ops/RUNBOOK.md
--     section "Kind activity (migration 021)".
--   - Every expression below must be non-throwing for arbitrary rows. A
--     throwing materialized view fails the whole events_local insert.

-- =============================================================================
-- KIND ACTIVITY: per (kind, grain, period) counts, content bytes, pubkeys
-- =============================================================================
--
-- One row per kind, time grain and UTC period of created_at:
--   grain = 'hour'   period = start of the UTC hour   (last 35 days only)
--   grain = 'day'    period = start of the UTC day    (all history)
--   grain = 'month'  period = start of the UTC month  (all history)
--
-- Time basis: the event's own created_at, NOT clamped (019 counts events
-- dated more than a day after ingestion on the day after ingestion; this
-- table does not). The endpoints it serves filter on created_at relative to
-- now(), so the unclamped period reproduces them at period granularity:
-- future-dated events stay in every "last N days" window of /kinds/{kind}
-- (no upper bound, as before) and enter /kinds/{kind}/activity once their
-- period starts (as `created_at <= now()` did). Hour rows are kept for 35
-- days, which covers the 30-day window plus the partial first hour; the view
-- writes them only for hours newer than now() - 35 days, the backfill only for
-- hours newer than the view boundary - 35 days. The TTL is capped at
-- 2106-01-01: ClickHouse evaluates a Date TTL as a DateTime, and a period
-- within 35 days of the DateTime maximum (2106-02-07, a common bogus
-- created_at) would otherwise wrap to 1970 and expire at once.
--
-- event_count and content_bytes (sum of length(content) in bytes, what the
-- old avg(length(content)) measured) are SUMS: not idempotent. Same
-- exactly-once model as 019's publisher_activity and 020's kind_totals: the
-- view writes live_min_indexed_at = min(indexed_at), the backfill the sentinel
-- 4294967295 (2106-02-07 06:28:15), so min(live_min_indexed_at) over view rows
-- is the first second the view captured. `api-rollups plan` fixes it; backfill
-- shards read only rows indexed before it and run exactly once. `origin` keeps
-- the backfill's and the view's rows apart forever, so `api-rollups validate`
-- can compare the backfill share with countIf(indexed_at < boundary) exactly.
--
-- pubkeys is a uniqCombined64 sketch (as kind_totals: ~0.3% standard error,
-- exact below ~4k values, at most ~96 KiB per row) of the period's authors,
-- for /kinds/{kind}/activity. Hour rows carry an empty sketch (no reader).
--
-- Unpartitioned: every backfill shard is exactly one part, committed
-- atomically, and odd timestamps can never exceed
-- max_partitions_per_insert_block and fail an ingest insert. TTL deletes of
-- old hour rows are applied by at most one rewrite per day.

CREATE TABLE IF NOT EXISTS nostr.kind_activity (
    kind UInt16,
    grain Enum8('hour' = 1, 'day' = 2, 'month' = 3),
    period DateTime('UTC'),
    origin Enum8('backfill' = 1, 'view' = 2),
    event_count SimpleAggregateFunction(sum, UInt64),
    content_bytes SimpleAggregateFunction(sum, UInt64),
    pubkeys AggregateFunction(uniqCombined64, String),
    live_min_indexed_at SimpleAggregateFunction(min, DateTime),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY (kind, grain, period, origin)
TTL least(toDate(period, 'UTC') + INTERVAL 35 DAY, toDate('2106-01-01')) DELETE WHERE grain = 'hour'
SETTINGS merge_with_ttl_timeout = 86400;

-- Guard: the boundary rule needs the view to start on an EMPTY table. If a
-- previous view was dropped, truncate kind_activity before re-applying.
SELECT throwIf(
    (SELECT count() FROM system.tables
     WHERE database = 'nostr' AND name = 'kind_activity_mv') = 0
    AND (SELECT count() FROM nostr.kind_activity) > 0,
    'nostr.kind_activity has rows but kind_activity_mv does not exist: TRUNCATE TABLE nostr.kind_activity first (see docs/api_rollups.md)'
) AS guard_kind_activity_empty
FORMAT Null;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.kind_activity_mv TO nostr.kind_activity
AS SELECT
    kind,
    CAST(slot, 'Enum8(''hour'' = 1, ''day'' = 2, ''month'' = 3)') AS grain,
    multiIf(slot = 1, toStartOfHour(created_at, 'UTC'), slot = 2, toStartOfDay(created_at, 'UTC'), toDateTime(toStartOfMonth(created_at, 'UTC'), 'UTC')) AS period,
    CAST('view', 'Enum8(''backfill'' = 1, ''view'' = 2)') AS origin,
    count() AS event_count,
    sum(length(content)) AS content_bytes,
    uniqCombined64StateIf(pubkey, slot != 1) AS pubkeys,
    min(indexed_at) AS live_min_indexed_at,
    max(indexed_at) AS last_indexed_at
FROM (
    SELECT kind, pubkey, content, created_at, indexed_at
    FROM nostr.events_local
)
ARRAY JOIN if(toStartOfHour(created_at, 'UTC') > now() - toIntervalDay(35), [1, 2, 3], [2, 3]) AS slot
GROUP BY kind, grain, period;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 021 objects' AS status, name, engine, metadata_modification_time
FROM system.tables
WHERE database = 'nostr'
  AND name IN ('kind_activity', 'kind_activity_mv')
ORDER BY name;
