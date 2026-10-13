-- Rollback: 021_kind_activity
-- Description: Remove the kind_activity rollup for /api/v1/kinds/{kind} and
--              /api/v1/kinds/{kind}/activity and the view that feeds it from
--              events_local inserts.
-- Date: 2026-10-13
--
-- To run:
--   just ch-migrate docs/migrations/rollback/021_drop_kind_activity.sql
--
-- IMPORTANT:
--   - The API reads this table. After this rollback both endpoints answer
--     503 metric_unavailable (they do not fall back to scanning events_local).
--     Deploy an API build without the 021 reads first if that is not wanted.
--   - The view is dropped before the table so no ingest flush writes to a
--     missing target. Dropping the view removes its per-insert cost at once.
--   - This does not touch events_local or the 019/020 rollups. Re-applying
--     prepared/021 later needs `api-rollups plan --target kind-activity
--     --discard-progress` and a full backfill.

DROP VIEW IF EXISTS nostr.kind_activity_mv;

DROP TABLE IF EXISTS nostr.kind_activity;

SELECT 'Migration 021 objects remaining' AS status, count() AS objects
FROM system.tables
WHERE database = 'nostr'
  AND name IN ('kind_activity', 'kind_activity_mv');
