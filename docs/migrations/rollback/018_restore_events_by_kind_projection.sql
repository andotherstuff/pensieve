-- Rollback: 018_restore_events_by_kind_projection
-- Description: Restore and materialize the former full-row events_by_kind
--              projection after migration 018.
-- Date: 2026-10-11
--
-- To run:
--   just ch-migrate docs/migrations/rollback/018_restore_events_by_kind_projection.sql
--
-- WARNING:
--   - This schedules a full historical materialization. Production used about
--     1.34 TiB for this projection (2.41 billion rows, 2026-10-11), and /data
--     had ~225 GiB free before removal, so this is not possible without a
--     larger disk. It exists for completeness and for smaller installations.
--   - pensieve-preview does not need this projection (it reads the compact
--     lookup tables from migration 017); restore it only if another consumer
--     turns out to need kind-ordered scans.
--   - Wait for any DROP PROJECTION mutation to finish before starting rollback.
--   - Do not run this file again while its MATERIALIZE mutation is pending.

SET mutations_sync = 0;

SELECT
    name,
    path,
    formatReadableSize(free_space) AS free_space,
    formatReadableSize(unreserved_space) AS unreserved_space,
    formatReadableSize(total_space) AS total_space
FROM system.disks
ORDER BY name;

-- Refuse to start a ~1.34 TiB rebuild without conservative working space.
-- 1.6 TiB = 1,759,218,604,441 bytes.
SELECT throwIf(
    (
        SELECT unreserved_space
        FROM system.disks
        WHERE name = 'default'
    ) < 1759218604441,
    'rollback 018 requires at least 1.6 TiB unreserved on the default disk'
);

-- Do not overlap the rollback with projection removal or another part rewrite.
SELECT throwIf(
    count() > 0,
    'events_local already has a pending mutation; wait before rollback 018'
)
FROM system.mutations
WHERE database = 'nostr'
  AND table = 'events_local'
  AND is_done = 0;

-- This is the exact definition observed in production before migration 018.
ALTER TABLE nostr.events_local
    ADD PROJECTION IF NOT EXISTS events_by_kind
    (
        SELECT *
        ORDER BY (kind, created_at, pubkey)
    );

-- ADD affects new parts only. MATERIALIZE rebuilds the projection for existing
-- parts in the background.
ALTER TABLE nostr.events_local
    MATERIALIZE PROJECTION events_by_kind
    SETTINGS mutations_sync = 0;

SELECT 'events_by_kind projection restoration scheduled' AS status;

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
  AND command LIKE '%MATERIALIZE PROJECTION%events_by_kind%'
ORDER BY create_time DESC
LIMIT 5;

SELECT
    count() AS active_projection_parts,
    sum(rows) AS projection_rows,
    formatReadableSize(sum(bytes_on_disk)) AS projection_bytes
FROM system.projection_parts
WHERE database = 'nostr'
  AND table = 'events_local'
  AND name = 'events_by_kind'
  AND active;
