-- Migration: 010_fix_pubkey_first_seen_dates
-- Description: Filter out invalid dates (future dates and pre-Nostr genesis) from
--              the pubkey_first_seen view. This fixes the "new users" time series
--              to only include pubkeys with valid first-seen timestamps.
-- Date: 2025-12-31
--
-- To run this migration:
--   just ch-migrate docs/migrations/010_fix_pubkey_first_seen_dates.sql
--
-- This migration is safe to re-run (uses CREATE OR REPLACE).
-- No backfill required.

-- =============================================================================
-- PROBLEM
-- =============================================================================
-- The pubkey_first_seen view was not filtering invalid dates, unlike the
-- active user views (fixed in migration 007). This caused:
-- - Events with future created_at (2100, 2077, etc.) to appear as "new users"
-- - Events with pre-Nostr genesis dates to be included
--
-- The dashboard "New Users" charts showed incorrect data because it counted
-- pubkeys with these invalid first_seen timestamps.

-- =============================================================================
-- FIX: Update view to filter valid date range
-- =============================================================================
-- Nostr genesis: November 7, 2020 (first commit to nostr-proto repo)
-- We filter to: first_seen >= '2020-11-07' AND first_seen <= today()

CREATE OR REPLACE VIEW pubkey_first_seen AS
SELECT
    pubkey,
    minMerge(first_seen_state) AS first_seen
FROM pubkey_first_seen_data
GROUP BY pubkey
HAVING first_seen >= toDateTime('2020-11-07 00:00:00')
   AND first_seen <= now();

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 010 complete. pubkey_first_seen now filters invalid dates.' AS status;

SELECT 'Date range check:' AS check;
SELECT
    min(first_seen) AS earliest,
    max(first_seen) AS latest,
    count() AS total_pubkeys
FROM pubkey_first_seen;

SELECT 'Sample new users per day (last 7 days):' AS check;
SELECT
    toDate(first_seen) AS date,
    count() AS new_users
FROM pubkey_first_seen
WHERE first_seen >= today() - INTERVAL 7 DAY
GROUP BY date
ORDER BY date DESC;

