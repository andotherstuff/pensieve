-- Rollback: 017_preview_compact_lookups
-- Description: Remove the compact preview lookup tables and the materialized
--              views that feed them from events_local inserts.
-- Date: 2026-10-10
--
-- To run:
--   just ch-migrate docs/migrations/rollback/017_drop_preview_compact_lookups.sql
--
-- IMPORTANT:
--   - First set PREVIEW_COMPACT_LOOKUPS=none and restart pensieve-preview, so
--     no preview read depends on these tables.
--   - Views are dropped before tables so no ingest flush writes to a missing
--     target. Dropping the views removes their per-insert cost immediately.
--   - This does not touch events_local or its projections. Re-applying
--     prepared/017 later requires a fresh plan and backfill.

DROP VIEW IF EXISTS nostr.profile_latest_mv;
DROP VIEW IF EXISTS nostr.zap_receipts_by_target_mv;
DROP VIEW IF EXISTS nostr.addressable_latest_mv;

DROP TABLE IF EXISTS nostr.profile_latest;
DROP TABLE IF EXISTS nostr.zap_receipts_by_target;
DROP TABLE IF EXISTS nostr.addressable_latest;

SELECT 'Migration 017 objects remaining' AS status, count() AS objects
FROM system.tables
WHERE database = 'nostr'
  AND name IN (
      'profile_latest', 'profile_latest_mv',
      'zap_receipts_by_target', 'zap_receipts_by_target_mv',
      'addressable_latest', 'addressable_latest_mv'
  );
