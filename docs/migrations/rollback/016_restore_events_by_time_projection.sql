-- Rollback: 016_restore_events_by_time_projection
-- Description: Restore and materialize the former full-row events_by_time
--              projection after migration 016.
-- Date: 2026-07-26
--
-- To run:
--   just ch-migrate docs/migrations/rollback/016_restore_events_by_time_projection.sql
--
-- WARNING:
--   - This schedules a full historical materialization.
--   - Production previously used about 986 GiB for this projection.
--   - Verify adequate free and unreserved space before running this file.
--   - Wait for any DROP PROJECTION mutation to finish before starting rollback.
--   - Do not run this file again while its MATERIALIZE mutation is pending.

SET mutations_sync = 0;

-- Operator preflight. See docs/migrations/README.md for the full procedure.
SELECT
    name,
    path,
    formatReadableSize(free_space) AS free_space,
    formatReadableSize(unreserved_space) AS unreserved_space,
    formatReadableSize(total_space) AS total_space
FROM system.disks
ORDER BY name;

-- Refuse to start a nearly 1 TiB rebuild without conservative working space.
-- 1.2 TiB = 1,319,413,953,331 bytes.
SELECT throwIf(
    (
        SELECT unreserved_space
        FROM system.disks
        WHERE name = 'default'
    ) < 1319413953331,
    'rollback 016 requires at least 1.2 TiB unreserved on the default disk'
);

-- Do not overlap the rollback with projection removal or another part rewrite.
SELECT throwIf(
    count() > 0,
    'events_local already has a pending mutation; wait before rollback 016'
)
FROM system.mutations
WHERE database = 'nostr'
  AND table = 'events_local'
  AND is_done = 0;

-- This is the exact definition observed in production before migration 016.
ALTER TABLE nostr.events_local
    ADD PROJECTION IF NOT EXISTS events_by_time
    (
        SELECT *
        ORDER BY (created_at, kind, pubkey)
    );

-- ADD affects new parts only. MATERIALIZE rebuilds the projection for existing
-- parts in the background so historical time-range queries can use it again.
ALTER TABLE nostr.events_local
    MATERIALIZE PROJECTION events_by_time
    SETTINGS mutations_sync = 0;

SELECT 'events_by_time projection restoration scheduled' AS status;

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
  AND command LIKE '%MATERIALIZE PROJECTION%events_by_time%'
ORDER BY create_time DESC
LIMIT 5;

SELECT
    count() AS active_projection_parts,
    sum(rows) AS projection_rows,
    formatReadableSize(sum(bytes_on_disk)) AS projection_bytes
FROM system.projection_parts
WHERE database = 'nostr'
  AND table = 'events_local'
  AND name = 'events_by_time'
  AND active;
