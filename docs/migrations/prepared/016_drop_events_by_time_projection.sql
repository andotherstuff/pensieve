-- Prepared migration: 016_drop_events_by_time_projection
-- Description: Remove the full-row events_by_time projection to reclaim disk
--              while retaining the created_at minmax skipping index.
-- Date: 2026-07-26
--
-- Prepared for ClickHouse 24.8.14.39. This migration is intentionally
-- asynchronous: monitor system.mutations until the DROP PROJECTION mutation
-- finishes and the projection bytes disappear.
--
-- To run:
--   just ch-migrate docs/migrations/prepared/016_drop_events_by_time_projection.sql
--
-- Rollback:
--   just ch-migrate docs/migrations/rollback/016_restore_events_by_time_projection.sql
--
-- IMPORTANT:
--   - This file is deliberately outside docs/migrations/*.sql, so
--     `just ch-migrate-all` cannot apply it.
--   - Run it only through the explicit command above after the documented
--     production preflight has been completed.
--   - Do not run the rollback materialization without the free-space check in
--     docs/migrations/README.md. Production used about 986 GiB for this projection.
--   - Do not use OPTIMIZE TABLE ... FINAL to accelerate cleanup.

-- Return immediately after scheduling the per-part projection removal.
SET mutations_sync = 0;

-- Pre-migration evidence. An already-migrated installation returns zeroes.
SELECT
    'events_by_time before removal' AS status,
    count() AS active_parts,
    sum(rows) AS rows,
    formatReadableSize(sum(bytes_on_disk)) AS bytes_on_disk
FROM system.projection_parts
WHERE database = 'nostr'
  AND table = 'events_local'
  AND name = 'events_by_time'
  AND active;

-- Avoid competing part rewrites. Re-run after the existing mutation finishes.
SELECT throwIf(
    count() > 0,
    'events_local already has a pending mutation; wait before migration 016'
)
FROM system.mutations
WHERE database = 'nostr'
  AND table = 'events_local'
  AND is_done = 0;

-- IF EXISTS makes the migration safe to re-run and a no-op on fresh installs
-- whose base schema already omits this projection.
ALTER TABLE nostr.events_local
    DROP PROJECTION IF EXISTS events_by_time;

SELECT 'events_by_time projection removal scheduled' AS status;

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
  AND command LIKE '%DROP PROJECTION%events_by_time%'
ORDER BY create_time DESC
LIMIT 5;

SELECT
    count() AS active_projection_parts_remaining,
    formatReadableSize(sum(bytes_on_disk)) AS active_projection_bytes_remaining
FROM system.projection_parts
WHERE database = 'nostr'
  AND table = 'events_local'
  AND name = 'events_by_time'
  AND active;

SELECT
    name,
    path,
    formatReadableSize(free_space) AS free_space,
    formatReadableSize(unreserved_space) AS unreserved_space,
    formatReadableSize(total_space) AS total_space
FROM system.disks
ORDER BY name;
