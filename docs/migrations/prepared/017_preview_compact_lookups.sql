-- Prepared migration: 017_preview_compact_lookups
-- Description: Compact lookup tables for pensieve-preview (nstr.to) so profile,
--              zap and naddr previews no longer depend on the full-row
--              events_by_kind projection on events_local.
-- Date: 2026-10-10
--
-- Prepared for ClickHouse 24.8.14.39.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/017_preview_compact_lookups.sql
--
-- Rollback:
--   just ch-migrate docs/migrations/rollback/017_drop_preview_compact_lookups.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it. It attaches new insert-triggered
--     materialized views to events_local, which run inside every ingest flush.
--   - It creates empty tables and starts capturing new inserts only. History is
--     loaded separately with the bounded `preview-lookups backfill` tool, which
--     must run while the events_by_kind projection still exists.
--   - Follow docs/preview_compact_lookups.md and the ops/RUNBOOK.md section
--     "Preview compact lookups" for preflight, backfill, validation and rollout.
--   - Every expression below must be non-throwing for arbitrary tag arrays. A
--     throwing materialized view fails the whole async-insert flush, and with
--     wait_for_async_insert=0 the ingester never sees that error.
--
-- Correctness model (see docs/preview_compact_lookups.md):
--   Reads never depend on background merges. Every read collapses duplicate
--   rows/states with GROUP BY / -Merge, so results are identical before and
--   after merges, and replays, out-of-order arrivals and backfill overlap are
--   all harmless.

-- =============================================================================
-- PROFILE LOOKUP: latest kind 0 per pubkey
-- =============================================================================
--
-- Winner rule (NIP-01 replaceable): highest created_at wins; on a created_at
-- tie the lowest event id wins. argMin over the key (-created_at, id) encodes
-- exactly that rule, and the payload tuple keeps id, content and created_at
-- from the same event.

CREATE TABLE IF NOT EXISTS nostr.profile_latest (
    pubkey String,
    latest AggregateFunction(argMin, Tuple(String, String, DateTime), Tuple(Int64, String)),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY pubkey;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.profile_latest_mv TO nostr.profile_latest
AS SELECT
    pubkey,
    argMinState(
        CAST((id, content, created_at), 'Tuple(String, String, DateTime)'),
        CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')
    ) AS latest,
    max(indexed_at) AS last_indexed_at
FROM nostr.events_local
WHERE kind = 0
GROUP BY pubkey;

-- =============================================================================
-- ZAP LOOKUP: one row per (target event, zap receipt)
-- =============================================================================
--
-- The amount is derived from the receipt's own bolt11 tag using exactly the
-- migration 009 rules (amount from the first bolt11 tag; values above 1B msats
-- become 0; included only if some bolt11 tag matches ^lnbc[0-9]+[munp]1). It
-- never joins another table, so there is no ordering dependency between a
-- receipt and its amount.
--
-- Targets are every distinct non-empty `e` tag value, matching the legacy
-- preview query's arrayExists(t -> t[1] = 'e' AND t[2] = ?) semantics.
--
-- parser_version allows a corrected parser to run beside this one (as a new
-- materialized view writing version 2) and be backfilled without a gap. Reads
-- take argMax(..., parser_version) per receipt, so the newest parser wins
-- regardless of which version's row arrived first.

CREATE TABLE IF NOT EXISTS nostr.zap_receipts_by_target (
    target_event_id String,
    receipt_id String,
    amount_msats UInt64,
    amount_parsed UInt8,
    parser_version UInt32,
    receipt_created_at DateTime,
    indexed_at DateTime
) ENGINE = ReplacingMergeTree(parser_version)
ORDER BY (target_event_id, receipt_id);

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.zap_receipts_by_target_mv TO nostr.zap_receipts_by_target
AS WITH
    (arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2] AS first_bolt11,
    multiIf(
        extract(first_bolt11, '^lnbc([0-9]+)m1') != '',
        toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)m1')) * 100000000,
        extract(first_bolt11, '^lnbc([0-9]+)u1') != '',
        toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)u1')) * 100000,
        extract(first_bolt11, '^lnbc([0-9]+)n1') != '',
        toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)n1')) * 100,
        extract(first_bolt11, '^lnbc([0-9]+)p1') != '',
        intDiv(toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)p1')), 10),
        toUInt64(0)
    ) AS raw_amount_msats
SELECT
    target_event_id,
    id AS receipt_id,
    if(raw_amount_msats > toUInt64(1000000000), toUInt64(0), raw_amount_msats) AS amount_msats,
    toUInt8(arrayExists(
        t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]+[munp]1'),
        tags
    )) AS amount_parsed,
    toUInt32(1) AS parser_version,
    created_at AS receipt_created_at,
    indexed_at
FROM nostr.events_local
ARRAY JOIN arrayDistinct(arrayMap(
    t -> t[2],
    arrayFilter(t -> length(t) >= 2 AND t[1] = 'e' AND t[2] != '', tags)
)) AS target_event_id
WHERE kind = 9735;

-- =============================================================================
-- NADDR LOOKUP: latest event pointer per (pubkey, kind, d_tag)
-- =============================================================================
--
-- Covers replaceable (10000-19999) and addressable (30000-39999) kinds. Stores
-- only the winning event id; previews fetch the row from events_local by
-- primary key. d_tag uses the same expression as the events_local MATERIALIZED
-- d_tag column so lookups match the legacy `d_tag = ?` filter exactly. Same
-- winner rule as profiles.

CREATE TABLE IF NOT EXISTS nostr.addressable_latest (
    pubkey String,
    kind UInt16,
    d_tag String,
    latest AggregateFunction(argMin, String, Tuple(Int64, String)),
    last_indexed_at SimpleAggregateFunction(max, DateTime)
) ENGINE = AggregatingMergeTree
ORDER BY (pubkey, kind, d_tag);

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.addressable_latest_mv TO nostr.addressable_latest
AS SELECT
    pubkey,
    kind,
    arrayElement(arrayFilter(t -> t[1] = 'd', tags), 1)[2] AS d_tag,
    argMinState(id, CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')) AS latest,
    max(indexed_at) AS last_indexed_at
FROM nostr.events_local
WHERE (kind >= 10000 AND kind < 20000) OR (kind >= 30000 AND kind < 40000)
GROUP BY pubkey, kind, d_tag;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 017 objects' AS status, name, engine, metadata_modification_time
FROM system.tables
WHERE database = 'nostr'
  AND name IN (
      'profile_latest', 'profile_latest_mv',
      'zap_receipts_by_target', 'zap_receipts_by_target_mv',
      'addressable_latest', 'addressable_latest_mv'
  )
ORDER BY name;
