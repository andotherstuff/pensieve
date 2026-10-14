-- Rollback: 022_hourly_pubkeys
-- Description: Remove the hourly_pubkeys rollup for
--              /api/v1/stats/activity/hourly and the view that feeds it from
--              events_local inserts.
-- Date: 2026-10-14
--
-- To run:
--   just ch-migrate docs/migrations/rollback/022_drop_hourly_pubkeys.sql
--
-- IMPORTANT:
--   - The API reads this table. After this rollback
--     /api/v1/stats/activity/hourly answers 503 metric_unavailable (it does
--     not fall back to scanning events_local). Deploy an API build without the
--     022 read first if that is not wanted. /stats/events and
--     /stats/throughput read migration 021's kind_activity and are unaffected.
--   - The view is dropped before the table so no ingest flush writes to a
--     missing target. Dropping the view removes its per-insert cost at once.
--   - This does not touch events_local or the 019/020/021 rollups. Re-applying
--     prepared/022 later needs `api-rollups plan --target hourly-pubkeys
--     --discard-progress` and a backfill (idempotent, last 35 days only).

DROP VIEW IF EXISTS nostr.hourly_pubkeys_mv;

DROP TABLE IF EXISTS nostr.hourly_pubkeys;

SELECT 'Migration 022 objects remaining' AS status, count() AS objects
FROM system.tables
WHERE database = 'nostr'
  AND name IN ('hourly_pubkeys', 'hourly_pubkeys_mv');
