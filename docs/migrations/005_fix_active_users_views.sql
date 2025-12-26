-- Migration: 005_fix_active_users_views
-- Description: Fix the slow active users views from migration 004
--              The views in 004 do expensive JOINs at query time against
--              daily_pubkeys_data (100M+ rows). This migration creates a
--              pre-aggregated daily_user_stats table that stores the breakdown
--              info per (date, pubkey), making queries instant.
-- Date: 2025-12-26
--
-- To run this migration:
--   just ch-migrate docs/migrations/005_fix_active_users_views.sql
--
-- Or via docker:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/005_fix_active_users_views.sql
--
-- IMPORTANT:
--   1. Stop Grafana and pensieve-ingest BEFORE running this migration
--   2. The backfill can take 10-30 minutes depending on data size
--   3. Run during low-traffic periods

-- =============================================================================
-- STEP 1: DROP THE SLOW VIEWS FROM MIGRATION 004
-- =============================================================================

DROP VIEW IF EXISTS daily_active_users;
DROP VIEW IF EXISTS weekly_active_users;
DROP VIEW IF EXISTS monthly_active_users;

-- =============================================================================
-- STEP 2: CREATE PRE-AGGREGATED DAILY USER STATS TABLE
-- =============================================================================
-- This table stores one row per (date, pubkey) with pre-computed flags for
-- whether that pubkey has a profile and follows list. This eliminates the
-- expensive JOINs at query time.

DROP TABLE IF EXISTS daily_user_stats_mv;
DROP TABLE IF EXISTS daily_user_stats;

CREATE TABLE daily_user_stats (
    date Date,
    pubkey String,
    has_profile UInt8,      -- 1 if pubkey has ever posted kind=0
    has_follows UInt8,      -- 1 if pubkey has ever posted kind=3
    event_count UInt32      -- Number of events from this pubkey on this date
) ENGINE = SummingMergeTree()
ORDER BY (date, pubkey);

-- =============================================================================
-- STEP 3: CREATE MATERIALIZED VIEW FOR NEW DATA
-- =============================================================================
-- This MV populates daily_user_stats as new events arrive.
-- It uses EXISTS subqueries which are efficient for the small lookup tables.

CREATE MATERIALIZED VIEW daily_user_stats_mv TO daily_user_stats AS
SELECT
    toDate(created_at) AS date,
    pubkey,
    -- Check if this pubkey has a profile (kind=0) in our lookup table
    toUInt8(pubkey IN (SELECT pubkey FROM pubkeys_with_profile_data)) AS has_profile,
    -- Check if this pubkey has a follows list (kind=3) in our lookup table
    toUInt8(pubkey IN (SELECT pubkey FROM pubkeys_with_follows_data)) AS has_follows,
    toUInt32(1) AS event_count
FROM events_local
WHERE kind NOT IN (445, 1059);  -- Exclude throwaway key kinds

-- =============================================================================
-- STEP 4: BACKFILL FROM EXISTING DATA
-- =============================================================================
-- This populates daily_user_stats with all historical data.
-- Uses the existing daily_pubkeys_data (which has unique date+pubkey pairs)
-- and JOINs against the profile/follows lookup tables.

SELECT 'Backfilling daily_user_stats from existing data...' AS status;
SELECT 'This may take 10-30 minutes for large datasets.' AS status;

-- Use a simpler approach: aggregate events and JOIN with lookup tables
INSERT INTO daily_user_stats
SELECT
    toDate(created_at) AS date,
    e.pubkey,
    toUInt8(pp.pubkey != '') AS has_profile,
    toUInt8(pf.pubkey != '') AS has_follows,
    toUInt32(count()) AS event_count
FROM events_local e
LEFT JOIN (SELECT DISTINCT pubkey FROM pubkeys_with_profile_data) pp ON e.pubkey = pp.pubkey
LEFT JOIN (SELECT DISTINCT pubkey FROM pubkeys_with_follows_data) pf ON e.pubkey = pf.pubkey
WHERE e.kind NOT IN (445, 1059)
GROUP BY date, e.pubkey, pp.pubkey, pf.pubkey;

SELECT 'Backfill complete. Optimizing table...' AS status;
OPTIMIZE TABLE daily_user_stats FINAL;

-- =============================================================================
-- STEP 5: CREATE FAST QUERY VIEWS
-- =============================================================================
-- These views aggregate from the pre-computed daily_user_stats table.
-- No expensive JOINs - just simple aggregations.

-- Daily Active Users
-- Note: Use table alias 's' to avoid column/alias name conflicts
CREATE VIEW daily_active_users AS
SELECT
    s.date AS date,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows_list,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list,
    sum(s.event_count) AS total_events,
    sumIf(s.event_count, s.has_profile = 1) AS events_with_profile,
    sumIf(s.event_count, s.has_follows = 1) AS events_with_follows_list,
    sumIf(s.event_count, s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list
FROM daily_user_stats s
GROUP BY s.date
ORDER BY date DESC;

-- Weekly Active Users
CREATE VIEW weekly_active_users AS
SELECT
    toMonday(s.date) AS week,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows_list,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list,
    sum(s.event_count) AS total_events,
    sumIf(s.event_count, s.has_profile = 1) AS events_with_profile,
    sumIf(s.event_count, s.has_follows = 1) AS events_with_follows_list,
    sumIf(s.event_count, s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list
FROM daily_user_stats s
GROUP BY week
ORDER BY week DESC;

-- Monthly Active Users
CREATE VIEW monthly_active_users AS
SELECT
    toStartOfMonth(s.date) AS month,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows_list,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list,
    sum(s.event_count) AS total_events,
    sumIf(s.event_count, s.has_profile = 1) AS events_with_profile,
    sumIf(s.event_count, s.has_follows = 1) AS events_with_follows_list,
    sumIf(s.event_count, s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list
FROM daily_user_stats s
GROUP BY month
ORDER BY month DESC;

-- =============================================================================
-- STEP 6: CLEANUP OLD EXPENSIVE TABLES (optional)
-- =============================================================================
-- The daily_pubkeys_data table from migration 004 is no longer needed.
-- Uncomment these lines to drop it and free up space:

-- DROP TABLE IF EXISTS daily_pubkeys_mv;
-- DROP TABLE IF EXISTS daily_pubkeys_data;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 005 complete. Checking table sizes...' AS status;

SELECT
    name,
    engine,
    total_rows,
    formatReadableSize(total_bytes) AS size
FROM system.tables
WHERE database = currentDatabase()
    AND name IN ('daily_user_stats', 'daily_active_users', 'weekly_active_users', 'monthly_active_users')
ORDER BY name;

SELECT 'Testing daily_active_users (should be FAST now):' AS status;
SELECT date, active_users, has_profile, has_follows_list, total_events
FROM daily_active_users
WHERE date >= today() - 7
ORDER BY date DESC
LIMIT 7;

SELECT 'Migration 005 complete!' AS status;
