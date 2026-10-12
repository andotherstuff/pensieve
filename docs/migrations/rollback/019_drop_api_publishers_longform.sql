-- Rollback: 019_api_publishers_longform
-- Description: Remove the API rollup tables for /api/v1/stats/publishers and
--              /api/v1/stats/longform and the views that feed them from
--              events_local inserts.
-- Date: 2026-10-12
--
-- To run:
--   just ch-migrate docs/migrations/rollback/019_drop_api_publishers_longform.sql
--
-- IMPORTANT:
--   - The API reads these tables. After this rollback both endpoints answer
--     503 metric_unavailable (they do not fall back to scanning events_local).
--     Deploy an API build without the 019 reads first if that is not wanted.
--   - Views are dropped before tables so no ingest flush writes to a missing
--     target. Dropping the views removes their per-insert cost immediately.
--   - This does not touch events_local. Re-applying prepared/019 later needs
--     a fresh `api-rollups plan --discard-progress` and backfill.

DROP VIEW IF EXISTS nostr.publisher_activity_mv;
DROP VIEW IF EXISTS nostr.longform_latest_mv;

DROP TABLE IF EXISTS nostr.publisher_activity;
DROP TABLE IF EXISTS nostr.longform_latest;

SELECT 'Migration 019 objects remaining' AS status, count() AS objects
FROM system.tables
WHERE database = 'nostr'
  AND name IN (
      'publisher_activity', 'publisher_activity_mv',
      'longform_latest', 'longform_latest_mv'
  );
