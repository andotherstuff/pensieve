-- Rollback: 020_incremental_derived_views (phase 1)
-- Description: Remove pubkey_flags, relay_list_latest, kind_totals, the views
--              that feed them from events_local inserts, and the
--              daily_user_stats.last_indexed_at column.
-- Date: 2026-10-12
--
-- To run:
--   just ch-migrate docs/migrations/rollback/020_drop_incremental_derived_views.sql
--
-- IMPORTANT:
--   - If the cutover ran, undo it first
--     (rollback/020_undo_derived_views_cutover.sql). The guards below refuse
--     while daily_user_stats_mv, an active-users refresh or
--     relay_distribution_mv still reads a 020 object, and the API release that
--     reads kind_totals answers 503 metric_unavailable on /kinds without it.
--   - Views are dropped before tables so no ingest flush writes to a missing
--     target. Dropping the views removes their per-insert cost immediately.
--   - This does not touch events_local. Re-applying prepared/020 later needs
--     a fresh `api-rollups plan --migration 020 --discard-progress` and backfill.

SELECT throwIf(
    (SELECT count() FROM system.tables
     WHERE database = 'nostr'
       AND name NOT IN (
           'pubkey_flags', 'pubkey_flags_mv',
           'relay_list_latest', 'relay_list_latest_mv',
           'kind_totals', 'kind_totals_mv'
       )
       AND (position(create_table_query, 'pubkey_flags') > 0
            OR position(create_table_query, 'relay_list_latest') > 0
            OR position(create_table_query, 'kind_totals') > 0
            OR (name = 'daily_user_stats_mv'
                AND position(create_table_query, 'last_indexed_at') > 0))) > 0,
    'a view still reads a migration 020 object: run rollback/020_undo_derived_views_cutover.sql first'
) AS guard_cutover_undone
FORMAT Null;

DROP VIEW IF EXISTS nostr.pubkey_flags_mv;
DROP VIEW IF EXISTS nostr.relay_list_latest_mv;
DROP VIEW IF EXISTS nostr.kind_totals_mv;

DROP TABLE IF EXISTS nostr.pubkey_flags;
DROP TABLE IF EXISTS nostr.relay_list_latest;
DROP TABLE IF EXISTS nostr.kind_totals;

ALTER TABLE nostr.daily_user_stats DROP COLUMN IF EXISTS last_indexed_at;

SELECT 'Migration 020 objects remaining' AS step, count() AS objects
FROM system.tables
WHERE database = 'nostr'
  AND name IN (
      'pubkey_flags', 'pubkey_flags_mv',
      'relay_list_latest', 'relay_list_latest_mv',
      'kind_totals', 'kind_totals_mv'
  );
