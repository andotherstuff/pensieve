-- Migration: 013_schema_optimizations
-- Description: Drop unused objects, replace slow AggregateFunction tables with pre-computed final counts
-- Date: 2026-01-04
--
-- PROBLEM: The *_summary tables use AggregateFunction columns which require
--          uniqMerge() at query time. This is slow (~4 seconds) because ClickHouse
--          must deserialize and merge HyperLogLog sketches.
--
-- SOLUTION: Store pre-computed UInt64 values. Query time becomes <10ms.
--
-- To run: just ch-migrate docs/migrations/013_schema_optimizations.sql

-- Enable refreshable MVs
SET allow_experimental_refreshable_materialized_view = 1;

-- =============================================================================
-- PART 1: DROP UNUSED OBJECTS
-- =============================================================================

SELECT '=== PART 1: Dropping unused objects ===' AS status;

-- Views (must drop before tables)
DROP VIEW IF EXISTS nostr.trending_videos;
DROP VIEW IF EXISTS nostr.video_stats;
DROP VIEW IF EXISTS nostr.video_hashtags;
DROP VIEW IF EXISTS nostr.popular_video_hashtags;
DROP VIEW IF EXISTS nostr.videos;
DROP VIEW IF EXISTS nostr.top_video_creators;
DROP VIEW IF EXISTS nostr.tag_stats;
DROP VIEW IF EXISTS nostr.user_profiles;
DROP VIEW IF EXISTS nostr.event_stats;
DROP VIEW IF EXISTS nostr.activity_by_kind;
DROP VIEW IF EXISTS nostr.pubkeys_with_profile;
DROP VIEW IF EXISTS nostr.pubkeys_with_follows;
DROP VIEW IF EXISTS nostr.daily_active_users;
DROP VIEW IF EXISTS nostr.weekly_active_users;
DROP VIEW IF EXISTS nostr.monthly_active_users;
DROP VIEW IF EXISTS nostr.daily_new_users;

-- MVs
DROP TABLE IF EXISTS nostr.event_tags_flat;
-- KEEPING: reaction_counts_mv, comment_counts_mv, repost_counts_mv (for future engagement endpoints)
DROP TABLE IF EXISTS nostr.daily_active_users_mv;
DROP TABLE IF EXISTS nostr.weekly_active_users_mv;
DROP TABLE IF EXISTS nostr.monthly_active_users_mv;
DROP TABLE IF EXISTS nostr.daily_pubkeys_mv;

-- Tables
DROP TABLE IF EXISTS nostr.event_tags_flat_data;
-- KEEPING: reaction_counts, comment_counts, repost_counts (for future engagement endpoints)
DROP TABLE IF EXISTS nostr.daily_active_users_data;
DROP TABLE IF EXISTS nostr.weekly_active_users_data;
DROP TABLE IF EXISTS nostr.monthly_active_users_data;
DROP TABLE IF EXISTS nostr.daily_pubkeys_data;

SELECT 'Unused objects dropped.' AS status;

-- =============================================================================
-- PART 2: DROP OLD SLOW SUMMARY TABLES (AggregateFunction-based)
-- =============================================================================

SELECT '=== PART 2: Dropping slow AggregateFunction tables ===' AS status;

-- Drop the MVs first
DROP TABLE IF EXISTS nostr.daily_active_users_summary_mv;
DROP TABLE IF EXISTS nostr.weekly_active_users_summary_mv;
DROP TABLE IF EXISTS nostr.monthly_active_users_summary_mv;
DROP TABLE IF EXISTS nostr.daily_new_users_summary_mv;

-- Drop the slow tables
DROP TABLE IF EXISTS nostr.daily_active_users_summary;
DROP TABLE IF EXISTS nostr.weekly_active_users_summary;
DROP TABLE IF EXISTS nostr.monthly_active_users_summary;
DROP TABLE IF EXISTS nostr.daily_new_users_summary;

SELECT 'Old summary tables dropped.' AS status;

-- =============================================================================
-- PART 3: CREATE NEW FAST TABLES (pre-computed UInt64 values)
-- =============================================================================

SELECT '=== PART 3: Creating fast pre-computed tables ===' AS status;

-- Idempotency: clean up any partially-created objects from a previous run.
-- (Refreshable materialized views are dropped with DROP TABLE in ClickHouse.)
DROP TABLE IF EXISTS nostr.active_users_daily_mv;
DROP TABLE IF EXISTS nostr.active_users_weekly_mv;
DROP TABLE IF EXISTS nostr.active_users_monthly_mv;
DROP TABLE IF EXISTS nostr.new_users_daily_mv;

DROP TABLE IF EXISTS nostr.active_users_daily;
DROP TABLE IF EXISTS nostr.active_users_weekly;
DROP TABLE IF EXISTS nostr.active_users_monthly;
DROP TABLE IF EXISTS nostr.new_users_daily;

-- Daily active users - plain UInt64, no aggregation at query time
CREATE TABLE nostr.active_users_daily (
    date Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows UInt64,
    has_both UInt64,
    total_events UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY date;

-- Weekly active users
CREATE TABLE nostr.active_users_weekly (
    week Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows UInt64,
    has_both UInt64,
    total_events UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY week;

-- Monthly active users
CREATE TABLE nostr.active_users_monthly (
    month Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows UInt64,
    has_both UInt64,
    total_events UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY month;

-- Daily new users
CREATE TABLE nostr.new_users_daily (
    date Date,
    new_users UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY date;

SELECT 'Fast tables created.' AS status;

-- =============================================================================
-- PART 4: BACKFILL FROM daily_user_stats
-- =============================================================================

SELECT '=== PART 4: Backfilling tables ===' AS status;

-- Backfill daily
INSERT INTO nostr.active_users_daily (date, active_users, has_profile, has_follows, has_both, total_events, updated_at)
SELECT
    s.date,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM nostr.daily_user_stats s
WHERE s.date >= '2020-11-07' AND s.date <= today()
GROUP BY s.date;

SELECT 'Daily backfilled.' AS status;

-- Backfill weekly
INSERT INTO nostr.active_users_weekly (week, active_users, has_profile, has_follows, has_both, total_events, updated_at)
SELECT
    toMonday(s.date) AS week,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM nostr.daily_user_stats s
WHERE s.date >= '2020-11-07' AND s.date <= today()
GROUP BY week;

SELECT 'Weekly backfilled.' AS status;

-- Backfill monthly
INSERT INTO nostr.active_users_monthly (month, active_users, has_profile, has_follows, has_both, total_events, updated_at)
SELECT
    toStartOfMonth(s.date) AS month,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM nostr.daily_user_stats s
WHERE s.date >= '2020-11-07' AND s.date <= today()
GROUP BY month;

SELECT 'Monthly backfilled.' AS status;

-- Backfill new users from pubkey_first_seen
INSERT INTO nostr.new_users_daily (date, new_users, updated_at)
SELECT
    toDate(first_seen) AS date,
    count() AS new_users,
    now() AS updated_at
FROM nostr.pubkey_first_seen
WHERE first_seen >= toDateTime('2020-11-07 00:00:00') AND first_seen <= now()
GROUP BY date;

SELECT 'New users backfilled.' AS status;

-- Optimize to merge parts
OPTIMIZE TABLE nostr.active_users_daily FINAL;
OPTIMIZE TABLE nostr.active_users_weekly FINAL;
OPTIMIZE TABLE nostr.active_users_monthly FINAL;
OPTIMIZE TABLE nostr.new_users_daily FINAL;

SELECT 'Tables optimized.' AS status;

-- =============================================================================
-- PART 5: CREATE REFRESHABLE MVs TO KEEP DATA UPDATED
-- =============================================================================

SELECT '=== PART 5: Creating refreshable MVs ===' AS status;

-- Daily - refresh every hour, recompute last 30 days
CREATE MATERIALIZED VIEW nostr.active_users_daily_mv
REFRESH EVERY 1 HOUR
TO nostr.active_users_daily
AS
SELECT
    s.date AS date,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM nostr.daily_user_stats s
WHERE s.date >= today() - 30 AND s.date <= today()
GROUP BY s.date;

-- Weekly - refresh twice a day, recompute last 26 weeks
CREATE MATERIALIZED VIEW nostr.active_users_weekly_mv
REFRESH EVERY 12 HOUR
TO nostr.active_users_weekly
AS
SELECT
    toMonday(s.date) AS week,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM nostr.daily_user_stats s
WHERE s.date >= toMonday(today()) - 182 AND s.date <= today()
GROUP BY week;

-- Monthly - refresh daily, recompute last 12 months
CREATE MATERIALIZED VIEW nostr.active_users_monthly_mv
REFRESH EVERY 1 DAY
TO nostr.active_users_monthly
AS
SELECT
    toStartOfMonth(s.date) AS month,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.has_profile = 1) AS has_profile,
    uniqIf(s.pubkey, s.has_follows = 1) AS has_follows,
    uniqIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM nostr.daily_user_stats s
WHERE s.date >= toStartOfMonth(today()) - 365 AND s.date <= today()
GROUP BY month;

-- New users - refresh twice a day, recompute full history.
-- Note: pubkey_first_seen is one row per pubkey (much smaller than events_local),
-- so a full refresh is typically reasonable even on large datasets.
CREATE MATERIALIZED VIEW nostr.new_users_daily_mv
REFRESH EVERY 12 HOUR
TO nostr.new_users_daily
AS
SELECT
    toDate(first_seen) AS date,
    count() AS new_users,
    now() AS updated_at
FROM nostr.pubkey_first_seen
WHERE first_seen >= toDateTime('2020-11-07 00:00:00') AND first_seen <= now()
GROUP BY date;

SELECT 'Refreshable MVs created.' AS status;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT '=== VERIFICATION ===' AS status;

-- Test query times (should be <10ms now)
SELECT 'Testing active_users_daily query:' AS test;
SELECT date, active_users, has_profile, has_follows, has_both, total_events
FROM nostr.active_users_daily FINAL
ORDER BY date DESC
LIMIT 10;

SELECT 'Row counts:' AS info;
SELECT 'active_users_daily' AS table, countDistinct(date) AS periods FROM nostr.active_users_daily
UNION ALL SELECT 'active_users_weekly', countDistinct(week) FROM nostr.active_users_weekly
UNION ALL SELECT 'active_users_monthly', countDistinct(month) FROM nostr.active_users_monthly
UNION ALL SELECT 'new_users_daily', countDistinct(date) FROM nostr.new_users_daily;

SELECT 'Refresh status:' AS info;
SELECT view, status, last_refresh_time, next_refresh_time
FROM system.view_refreshes
WHERE database = 'nostr';

SELECT '=== Migration 013 complete ===' AS status;
