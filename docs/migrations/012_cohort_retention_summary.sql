-- Migration: 012_cohort_retention_summary
-- Description: Create pre-aggregated summary tables for cohort retention analysis
--              using Refreshable Materialized Views for automatic daily updates.
-- Date: 2026-01-04
--
-- To run this migration:
--   just ch-migrate docs/migrations/012_cohort_retention_summary.sql
--
-- =============================================================================
-- PROBLEM
-- =============================================================================
-- The retention endpoint performs an extremely expensive query:
-- 1. Scans pubkey_first_seen (6M+ rows with aggregation finalization)
-- 2. JOINs against events_local (800M+ rows)
-- 3. Groups by cohort period and activity period
--
-- This query can take 60+ seconds or timeout entirely.
--
-- =============================================================================
-- SOLUTION
-- =============================================================================
-- Use Refreshable Materialized Views (ClickHouse 23.12+) to:
-- - Pre-aggregate cohort retention data into summary tables
-- - Automatically refresh daily at 03:00 UTC
-- - Full table replacement ensures cohort assignments stay correct
--   (important when backfilling historical data shifts first_seen dates)
--
-- Tables created:
-- - cohort_retention_weekly: Weekly cohort retention (cohort_week, activity_week)
-- - cohort_retention_monthly: Monthly cohort retention (cohort_month, activity_month)
--
-- =============================================================================
-- DEPENDENCIES
-- =============================================================================
-- Requires:
-- - pubkey_first_seen (migration 002) - for cohort assignment
-- - daily_user_stats (migration 005) - for activity tracking
--
-- =============================================================================
-- STEP 1: DROP EXISTING OBJECTS (if re-running migration)
-- =============================================================================

SELECT 'Dropping existing objects if present...' AS status;

DROP VIEW IF EXISTS cohort_retention_weekly_mv;
DROP VIEW IF EXISTS cohort_retention_monthly_mv;
DROP VIEW IF EXISTS cohort_retention_weekly_view;
DROP VIEW IF EXISTS cohort_retention_monthly_view;
DROP TABLE IF EXISTS cohort_retention_weekly;
DROP TABLE IF EXISTS cohort_retention_monthly;

-- =============================================================================
-- STEP 2: CREATE WEEKLY TARGET TABLE AND REFRESHABLE MV
-- =============================================================================

SELECT 'Creating cohort_retention_weekly with refreshable MV...' AS status;

-- Target table for the MV to populate
CREATE TABLE IF NOT EXISTS cohort_retention_weekly (
    cohort_week Date,      -- Monday of the week user first appeared
    activity_week Date,    -- Monday of the week of activity
    active_count UInt64    -- Count of unique pubkeys
) ENGINE = MergeTree()
ORDER BY (cohort_week, activity_week);

-- Refreshable MV: runs daily at 03:00 UTC, replaces entire table
CREATE MATERIALIZED VIEW cohort_retention_weekly_mv
REFRESH EVERY 1 DAY OFFSET 3 HOUR
TO cohort_retention_weekly
AS
SELECT
    toMonday(pfs.first_seen) AS cohort_week,
    toMonday(dus.date) AS activity_week,
    uniq(dus.pubkey) AS active_count
FROM daily_user_stats dus
INNER JOIN pubkey_first_seen pfs ON dus.pubkey = pfs.pubkey
WHERE toDate(pfs.first_seen) <= dus.date  -- Only activity on or after first seen
  AND toMonday(pfs.first_seen) >= toDate('2020-11-07')  -- Nostr genesis
GROUP BY cohort_week, activity_week;

-- =============================================================================
-- STEP 3: CREATE MONTHLY TARGET TABLE AND REFRESHABLE MV
-- =============================================================================

SELECT 'Creating cohort_retention_monthly with refreshable MV...' AS status;

-- Target table for the MV to populate
CREATE TABLE IF NOT EXISTS cohort_retention_monthly (
    cohort_month Date,     -- First day of the month user first appeared
    activity_month Date,   -- First day of the month of activity
    active_count UInt64    -- Count of unique pubkeys
) ENGINE = MergeTree()
ORDER BY (cohort_month, activity_month);

-- Refreshable MV: runs daily at 03:15 UTC (staggered from weekly)
CREATE MATERIALIZED VIEW cohort_retention_monthly_mv
REFRESH EVERY 1 DAY OFFSET 3 HOUR 15 MINUTE
TO cohort_retention_monthly
AS
SELECT
    toStartOfMonth(pfs.first_seen) AS cohort_month,
    toStartOfMonth(dus.date) AS activity_month,
    uniq(dus.pubkey) AS active_count
FROM daily_user_stats dus
INNER JOIN pubkey_first_seen pfs ON dus.pubkey = pfs.pubkey
WHERE toDate(pfs.first_seen) <= dus.date  -- Only activity on or after first seen
  AND toStartOfMonth(pfs.first_seen) >= toDate('2020-11-07')  -- Nostr genesis
GROUP BY cohort_month, activity_month;

-- =============================================================================
-- STEP 4: CREATE QUERY VIEWS (for consistent API interface)
-- =============================================================================

SELECT 'Creating query views...' AS status;

-- Weekly retention view with string formatting for API compatibility
CREATE OR REPLACE VIEW cohort_retention_weekly_view AS
SELECT
    toString(cohort_week) AS cohort,
    toString(activity_week) AS activity_period,
    active_count
FROM cohort_retention_weekly
WHERE cohort_week <= today()
  AND activity_week <= today()
ORDER BY cohort_week, activity_week;

-- Monthly retention view with string formatting for API compatibility
CREATE OR REPLACE VIEW cohort_retention_monthly_view AS
SELECT
    toString(cohort_month) AS cohort,
    toString(activity_month) AS activity_period,
    active_count
FROM cohort_retention_monthly
WHERE cohort_month <= today()
  AND activity_month <= today()
ORDER BY cohort_month, activity_month;

-- =============================================================================
-- STEP 5: TRIGGER INITIAL REFRESH
-- =============================================================================
-- Refreshable MVs don't populate immediately on creation.
-- We trigger the first refresh manually so data is available right away.

SELECT 'Triggering initial refresh (this may take 5-15 minutes)...' AS status;

SYSTEM REFRESH VIEW cohort_retention_weekly_mv;
SYSTEM REFRESH VIEW cohort_retention_monthly_mv;

-- Wait a moment for refresh to start, then check status
SELECT sleep(2);

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 012 complete. Checking refresh status...' AS status;

-- Check MV refresh status
SELECT
    'Refresh status:' AS info,
    database,
    view AS materialized_view,
    status,
    last_refresh_time,
    next_refresh_time
FROM system.view_refreshes
WHERE database = currentDatabase()
  AND view IN ('cohort_retention_weekly_mv', 'cohort_retention_monthly_mv');

-- Note: Initial data may take a few minutes to appear.
-- Check row counts after refresh completes:
SELECT 'Row counts (may be 0 if refresh still running):' AS info;
SELECT 'cohort_retention_weekly' AS table, count() AS rows FROM cohort_retention_weekly
UNION ALL
SELECT 'cohort_retention_monthly' AS table, count() AS rows FROM cohort_retention_monthly;

SELECT 'Tables and views created:' AS info;
SELECT
    name,
    engine
FROM system.tables
WHERE database = currentDatabase()
  AND name IN (
      'cohort_retention_weekly',
      'cohort_retention_monthly',
      'cohort_retention_weekly_mv',
      'cohort_retention_monthly_mv',
      'cohort_retention_weekly_view',
      'cohort_retention_monthly_view'
  )
ORDER BY name;

-- =============================================================================
-- MANUAL REFRESH (if needed)
-- =============================================================================
-- To manually trigger a refresh:
--   SYSTEM REFRESH VIEW cohort_retention_weekly_mv;
--   SYSTEM REFRESH VIEW cohort_retention_monthly_mv;
--
-- To check refresh status:
--   SELECT * FROM system.view_refreshes WHERE database = 'nostr';
--
-- To modify refresh schedule:
--   ALTER TABLE cohort_retention_weekly_mv MODIFY REFRESH EVERY 1 WEEK;
