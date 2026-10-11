-- Prepared migration: 018_drop_events_by_kind_projection
-- Description: Remove the full-row events_by_kind projection (~1.34 TiB in
--              production) now that pensieve-preview reads the compact lookup
--              tables from prepared migration 017.
-- Date: 2026-10-11
--
-- Prepared for ClickHouse 24.8.14.39. Like 016, this migration is
-- asynchronous: monitor system.mutations until the DROP PROJECTION mutation
-- finishes and the projection bytes disappear.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/018_drop_events_by_kind_projection.sql
--
-- Rollback (practically unavailable on the current disk; see the file):
--   just ch-migrate docs/migrations/rollback/018_restore_events_by_kind_projection.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it.
--   - Preconditions (enforced below where SQL can check them):
--       * migration 017 is applied and backfilled, and pensieve-preview runs
--         with PREVIEW_COMPACT_LOOKUPS=all (see ops/RUNBOOK.md);
--       * no query outside the preview-lookups operator tool has used
--         events_by_kind in the last 6 hours;
--       * no other mutation is pending on events_local.
--   - After removal, PREVIEW_COMPACT_LOOKUPS=none would bring back 10-30 s
--     legacy reads, and preview-lookups plan/validate/bench need
--     --allow-full-scan (rebuilds use --shard-by id-prefix).
--   - Do not use OPTIMIZE TABLE ... FINAL to accelerate cleanup.

-- Return immediately after scheduling the per-part projection removal.
SET mutations_sync = 0;

-- Pre-migration evidence. An already-migrated installation returns zeroes.
SELECT
    'events_by_kind before removal' AS status,
    count() AS active_parts,
    sum(rows) AS rows,
    formatReadableSize(sum(bytes_on_disk)) AS bytes_on_disk
FROM system.projection_parts
WHERE database = 'nostr'
  AND table = 'events_local'
  AND name = 'events_by_kind'
  AND active;

-- The compact lookup views from migration 017 must exist (preview reads them).
SELECT throwIf(
    count() != 3,
    'migration 017 compact lookup views are missing; apply and backfill 017 before 018'
)
FROM system.tables
WHERE database = 'nostr'
  AND name IN ('profile_latest_mv', 'zap_receipts_by_target_mv', 'addressable_latest_mv');

-- Nothing but the operator tool may still depend on the projection.
SELECT throwIf(
    count() > 0,
    'events_by_kind was used in the last 6 hours outside preview-lookups; run the consumer audit in ops/RUNBOOK.md'
)
FROM system.query_log
WHERE event_time > now() - INTERVAL 6 HOUR
  AND type = 'QueryFinish'
  AND has(projections, 'nostr.events_local.events_by_kind')
  AND NOT startsWith(log_comment, 'preview-lookups');

-- Avoid competing part rewrites. Re-run after the existing mutation finishes.
SELECT throwIf(
    count() > 0,
    'events_local already has a pending mutation; wait before migration 018'
)
FROM system.mutations
WHERE database = 'nostr'
  AND table = 'events_local'
  AND is_done = 0;

-- IF EXISTS makes the migration safe to re-run and a no-op on fresh installs
-- whose base schema already omits this projection.
ALTER TABLE nostr.events_local
    DROP PROJECTION IF EXISTS events_by_kind;

SELECT 'events_by_kind projection removal scheduled' AS status;

-- A successful command normally appears here with parts_to_do decreasing to
-- zero and is_done changing to 1. Disk is not considered reclaimed until it is
-- done and active projection parts have disappeared.
SELECT
    mutation_id,
    command,
    create_time,
    parts_to_do,
    is_done,
    latest_fail_reason
FROM system.mutations
WHERE database = 'nostr'
  AND table = 'events_local'
  AND command LIKE '%DROP PROJECTION%events_by_kind%'
ORDER BY create_time DESC
LIMIT 5;

SELECT
    count() AS active_projection_parts_remaining,
    formatReadableSize(sum(bytes_on_disk)) AS active_projection_bytes_remaining
FROM system.projection_parts
WHERE database = 'nostr'
  AND table = 'events_local'
  AND name = 'events_by_kind'
  AND active;

SELECT
    name,
    path,
    formatReadableSize(free_space) AS free_space,
    formatReadableSize(unreserved_space) AS unreserved_space,
    formatReadableSize(total_space) AS total_space
FROM system.disks
ORDER BY name;
