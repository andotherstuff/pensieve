-- Migration: 006_preaggregate_active_users
-- Description: Create pre-aggregated summary tables for instant active user queries.
--              The views from 005 still aggregate on-the-fly from daily_user_stats
--              which has millions of rows. This migration adds summary tables that
--              store the final aggregated values per day/week/month.
-- Date: 2025-12-26
--
-- To run this migration:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/006_preaggregate_active_users.sql

-- =============================================================================
-- STEP 1: CREATE DAILY SUMMARY TABLE
-- =============================================================================

DROP TABLE IF EXISTS daily_active_users_summary_mv;
DROP TABLE IF EXISTS daily_active_users_summary;

CREATE TABLE daily_active_users_summary (
    date Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows_list UInt64,
    has_profile_and_follows_list UInt64,
    total_events UInt64,
    events_with_profile UInt64,
    events_with_follows_list UInt64,
    events_with_profile_and_follows_list UInt64
) ENGINE = ReplacingMergeTree()
ORDER BY date;

-- MV to populate from daily_user_stats inserts
CREATE MATERIALIZED VIEW daily_active_users_summary_mv TO daily_active_users_summary AS
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
GROUP BY s.date;

-- =============================================================================
-- STEP 2: CREATE WEEKLY SUMMARY TABLE
-- =============================================================================

DROP TABLE IF EXISTS weekly_active_users_summary_mv;
DROP TABLE IF EXISTS weekly_active_users_summary;

CREATE TABLE weekly_active_users_summary (
    week Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows_list UInt64,
    has_profile_and_follows_list UInt64,
    total_events UInt64,
    events_with_profile UInt64,
    events_with_follows_list UInt64,
    events_with_profile_and_follows_list UInt64
) ENGINE = ReplacingMergeTree()
ORDER BY week;

-- MV to populate from daily_user_stats inserts
CREATE MATERIALIZED VIEW weekly_active_users_summary_mv TO weekly_active_users_summary AS
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
GROUP BY week;

-- =============================================================================
-- STEP 3: CREATE MONTHLY SUMMARY TABLE
-- =============================================================================

DROP TABLE IF EXISTS monthly_active_users_summary_mv;
DROP TABLE IF EXISTS monthly_active_users_summary;

CREATE TABLE monthly_active_users_summary (
    month Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows_list UInt64,
    has_profile_and_follows_list UInt64,
    total_events UInt64,
    events_with_profile UInt64,
    events_with_follows_list UInt64,
    events_with_profile_and_follows_list UInt64
) ENGINE = ReplacingMergeTree()
ORDER BY month;

-- MV to populate from daily_user_stats inserts
CREATE MATERIALIZED VIEW monthly_active_users_summary_mv TO monthly_active_users_summary AS
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
GROUP BY month;

-- =============================================================================
-- STEP 4: BACKFILL ALL SUMMARY TABLES
-- =============================================================================

SELECT 'Backfilling daily_active_users_summary...' AS status;
INSERT INTO daily_active_users_summary
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
GROUP BY s.date;

SELECT 'Backfilling weekly_active_users_summary...' AS status;
INSERT INTO weekly_active_users_summary
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
GROUP BY week;

SELECT 'Backfilling monthly_active_users_summary...' AS status;
INSERT INTO monthly_active_users_summary
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
GROUP BY month;

OPTIMIZE TABLE daily_active_users_summary FINAL;
OPTIMIZE TABLE weekly_active_users_summary FINAL;
OPTIMIZE TABLE monthly_active_users_summary FINAL;

-- =============================================================================
-- STEP 5: RECREATE VIEWS TO QUERY FROM SUMMARY TABLES (instant!)
-- =============================================================================

DROP VIEW IF EXISTS daily_active_users;
DROP VIEW IF EXISTS weekly_active_users;
DROP VIEW IF EXISTS monthly_active_users;

-- Daily - direct from summary (instant!)
CREATE VIEW daily_active_users AS
SELECT * FROM daily_active_users_summary FINAL
ORDER BY date DESC;

-- Weekly - direct from summary (instant!)
CREATE VIEW weekly_active_users AS
SELECT * FROM weekly_active_users_summary FINAL
ORDER BY week DESC;

-- Monthly - direct from summary (instant!)
CREATE VIEW monthly_active_users AS
SELECT * FROM monthly_active_users_summary FINAL
ORDER BY month DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 006 complete. Testing query performance...' AS status;

SELECT 'daily_active_users (should be <100ms):' AS status;
SELECT * FROM daily_active_users LIMIT 5;

SELECT 'weekly_active_users (should be <100ms):' AS status;
SELECT * FROM weekly_active_users LIMIT 5;

SELECT 'monthly_active_users (should be <100ms):' AS status;
SELECT * FROM monthly_active_users LIMIT 5;

SELECT 'Summary table sizes:' AS status;
SELECT
    name,
    formatReadableSize(total_bytes) AS size,
    total_rows
FROM system.tables
WHERE database = currentDatabase()
    AND name IN ('daily_active_users_summary', 'weekly_active_users_summary', 'monthly_active_users_summary')
ORDER BY name;
