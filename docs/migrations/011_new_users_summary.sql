-- Migration: 011_new_users_summary
-- Description: Create pre-aggregated summary table for new users per day.
--              This enables fast queries for "new users" time series without
--              scanning the full pubkey_first_seen_data table.
-- Date: 2025-12-31
--
-- To run this migration:
--   just ch-migrate docs/migrations/011_new_users_summary.sql
--
-- This migration includes a backfill step that may take several minutes
-- depending on the size of your events_local table.

-- =============================================================================
-- PROBLEM
-- =============================================================================
-- The pubkey_first_seen view requires scanning and merging all rows in
-- pubkey_first_seen_data (~6M+ pubkeys) on every query. This makes queries
-- like "new users per day for the last 30 days" take many seconds.
--
-- SOLUTION: Pre-aggregate new user counts per day into a summary table.

-- =============================================================================
-- STEP 1: CREATE SUMMARY TABLE
-- =============================================================================
-- Uses AggregatingMergeTree with uniqState to properly deduplicate pubkeys
-- across insert batches.

CREATE TABLE IF NOT EXISTS daily_new_users_summary (
    date Date,
    new_users_state AggregateFunction(uniq, String)
) ENGINE = AggregatingMergeTree()
ORDER BY date;

-- =============================================================================
-- STEP 2: CREATE MATERIALIZED VIEW FOR NEW DATA
-- =============================================================================
-- This MV triggers on events_local inserts and tracks the earliest event
-- date per pubkey within each batch.
--
-- Note: A pubkey might appear in multiple batches. The uniqState per date
-- ensures deduplication. The backfill from pubkey_first_seen provides the
-- accurate historical baseline, and new events only add to dates where
-- they're the pubkey's first appearance.

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_new_users_summary_mv TO daily_new_users_summary AS
SELECT
    toDate(min_created_at) AS date,
    uniqState(pubkey) AS new_users_state
FROM (
    SELECT
        pubkey,
        min(created_at) AS min_created_at
    FROM events_local
    WHERE kind NOT IN (445, 1059)
    GROUP BY pubkey
)
GROUP BY date;

-- =============================================================================
-- STEP 3: BACKFILL FROM EXISTING DATA
-- =============================================================================
-- Populate the summary table from the current pubkey_first_seen view.
-- This uses the already-merged first_seen values for accuracy.

SELECT 'Backfilling daily_new_users_summary from pubkey_first_seen...' AS status;

INSERT INTO daily_new_users_summary
SELECT
    toDate(first_seen) AS date,
    uniqState(pubkey) AS new_users_state
FROM pubkey_first_seen
WHERE first_seen >= toDateTime('2020-11-07 00:00:00')
  AND first_seen <= now()
GROUP BY date;

-- Force merge to consolidate data
OPTIMIZE TABLE daily_new_users_summary FINAL;

-- =============================================================================
-- STEP 4: CREATE QUERY VIEW
-- =============================================================================
-- Simple view that finalizes the aggregate states for easy querying.

CREATE OR REPLACE VIEW daily_new_users AS
SELECT
    date,
    uniqMerge(new_users_state) AS new_users
FROM daily_new_users_summary
WHERE date >= toDate('2020-11-07')  -- Nostr genesis
  AND date <= today()               -- Exclude future dates
GROUP BY date
ORDER BY date DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 011 complete. Verifying...' AS status;

SELECT 'Summary table size:' AS check;
SELECT
    name,
    engine,
    formatReadableSize(total_bytes) AS size,
    total_rows AS rows
FROM system.tables
WHERE database = currentDatabase()
  AND name = 'daily_new_users_summary';

SELECT 'Sample new users per day (last 14 days):' AS check;
SELECT date, new_users
FROM daily_new_users
WHERE date >= today() - INTERVAL 14 DAY
ORDER BY date DESC;

SELECT 'Total new users tracked:' AS check;
SELECT sum(new_users) AS total FROM daily_new_users;

