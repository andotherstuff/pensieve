-- Prepared migration: 022_hourly_pubkeys
-- Description: Small insert-fed rollup of distinct authors per UTC hour for
--              pensieve-serve's /api/v1/stats/activity/hourly, which otherwise
--              scans events_local (~2.47B rows, no projections): 6.72 s for
--              days=30 on 2026-10-14, and every refresh of the cached default.
--              Its event counts come from migration 021's kind_activity hour
--              rows; those rows carry an empty author sketch, so this table
--              holds only the authors.
-- Date: 2026-10-14
--
-- Prepared for ClickHouse 24.8.14.39. Applied after 021.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/022_hourly_pubkeys.sql
--
-- Rollback:
--   just ch-migrate docs/migrations/rollback/022_drop_hourly_pubkeys.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it. It attaches one insert-triggered
--     materialized view to events_local, which runs inside every ingest flush.
--   - It creates an empty table and starts capturing new inserts only. The
--     last 35 days are loaded with the bounded
--     `api-rollups backfill --target hourly-pubkeys` tool
--     (crates/pensieve-serve/src/bin/api-rollups.rs). The API must not be
--     switched to this table before that backfill is validated: until then
--     unique_pubkeys would undercount.
--   - Follow docs/api_rollups.md ("Migration 022") and the ops/RUNBOOK.md
--     section "Hourly authors (migration 022)".
--   - Every expression below must be non-throwing for arbitrary rows. A
--     throwing materialized view fails the whole events_local insert.

-- =============================================================================
-- HOURLY PUBKEYS: per (kind, UTC hour) author sketches, last 35 days
-- =============================================================================
--
-- One row per kind and UTC hour of created_at, plus one per hour for all
-- kinds together (kind = -1, as publisher_activity's all-kinds rows), so the
-- unfiltered endpoint merges one sketch per hour instead of one per kind and
-- hour.
--
-- Time basis: the event's own created_at in UTC, unclamped, as 021's
-- kind_activity (the counts the endpoint pairs these sketches with). The view
-- writes only hours newer than now() - 35 days; rows expire 35 days after
-- their hour (TTL capped at 2106-01-01: a Date TTL near the DateTime maximum
-- would otherwise wrap to 1970 and expire at once; see 021). The endpoint
-- reads at most 30 days plus the partial first hour.
--
-- pubkeys is a uniqCombined64 sketch (as kind_totals and kind_activity: ~0.3%
-- standard error, exact below ~4k values, at most ~96 KiB per row). Sketches
-- are IDEMPOTENT (a set union): a row counted by both the view and the
-- backfill, or by a re-run backfill shard, changes nothing. So there are no
-- sums, no view boundary and no origin column; the backfill follows 017's
-- model (grace period after the view's creation, no insert that started
-- before it still running).
--
-- Unpartitioned, as 019-021: odd timestamps can never exceed
-- max_partitions_per_insert_block and fail an ingest insert. TTL deletes are
-- applied by at most one rewrite per day.

CREATE TABLE IF NOT EXISTS nostr.hourly_pubkeys (
    kind Int32,
    period DateTime('UTC'),
    pubkeys AggregateFunction(uniqCombined64, String),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY (kind, period)
TTL least(toDate(period, 'UTC') + INTERVAL 35 DAY, toDate('2106-01-01'))
SETTINGS merge_with_ttl_timeout = 86400;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.hourly_pubkeys_mv TO nostr.hourly_pubkeys
AS SELECT
    scope AS kind,
    toStartOfHour(created_at, 'UTC') AS period,
    uniqCombined64State(pubkey) AS pubkeys,
    max(indexed_at) AS last_indexed_at
FROM (
    SELECT kind AS event_kind, pubkey, created_at, indexed_at
    FROM nostr.events_local
    WHERE toStartOfHour(created_at, 'UTC') > now() - toIntervalDay(35)
)
ARRAY JOIN [toInt32(event_kind), -1] AS scope
GROUP BY kind, period;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 022 objects' AS status, name, engine, metadata_modification_time
FROM system.tables
WHERE database = 'nostr'
  AND name IN ('hourly_pubkeys', 'hourly_pubkeys_mv')
ORDER BY name;
