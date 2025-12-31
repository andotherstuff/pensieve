-- Migration: 008_fix_active_users_aggregation
-- Description: Fix the active users summary tables to use AggregatingMergeTree
--              instead of ReplacingMergeTree. The previous approach (migration 006)
--              incorrectly used ReplacingMergeTree which REPLACES rows on merge
--              instead of COMBINING aggregates. This caused data loss when multiple
--              batches were inserted for the same date.
-- Date: 2025-12-31
--
-- To run this migration:
--   just ch-migrate docs/migrations/008_fix_active_users_aggregation.sql
--
-- IMPORTANT: This migration drops and recreates the summary tables with correct
--            aggregation. It then backfills from daily_user_stats.

-- =============================================================================
-- STEP 1: DROP BROKEN SUMMARY TABLES AND MVS
-- =============================================================================

DROP TABLE IF EXISTS daily_active_users_summary_mv;
DROP TABLE IF EXISTS weekly_active_users_summary_mv;
DROP TABLE IF EXISTS monthly_active_users_summary_mv;
DROP TABLE IF EXISTS daily_active_users_summary;
DROP TABLE IF EXISTS weekly_active_users_summary;
DROP TABLE IF EXISTS monthly_active_users_summary;

-- =============================================================================
-- STEP 2: CREATE FIXED DAILY SUMMARY TABLE WITH AGGREGATINGMERGETREE
-- =============================================================================

CREATE TABLE daily_active_users_summary (
    date Date,
    -- Use AggregateFunction types for proper incremental aggregation
    active_users_state AggregateFunction(uniq, String),
    has_profile_state AggregateFunction(uniq, String),
    has_follows_list_state AggregateFunction(uniq, String),
    has_profile_and_follows_list_state AggregateFunction(uniq, String),
    -- Use AggregateFunction for sums too (simpler than SummingMergeTree)
    total_events_state AggregateFunction(sum, UInt64),
    events_with_profile_state AggregateFunction(sum, UInt64),
    events_with_follows_list_state AggregateFunction(sum, UInt64),
    events_with_profile_and_follows_list_state AggregateFunction(sum, UInt64)
) ENGINE = AggregatingMergeTree()
ORDER BY date;

-- MV to populate from daily_user_stats inserts
-- Uses *State functions to create mergeable aggregate states
-- Note: Cast event_count to UInt64 to match the AggregateFunction type
CREATE MATERIALIZED VIEW daily_active_users_summary_mv TO daily_active_users_summary AS
SELECT
    s.date AS date,
    uniqState(s.pubkey) AS active_users_state,
    uniqStateIf(s.pubkey, s.has_profile = 1) AS has_profile_state,
    uniqStateIf(s.pubkey, s.has_follows = 1) AS has_follows_list_state,
    uniqStateIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list_state,
    sumState(toUInt64(s.event_count)) AS total_events_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1) AS events_with_profile_state,
    sumStateIf(toUInt64(s.event_count), s.has_follows = 1) AS events_with_follows_list_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list_state
FROM daily_user_stats s
GROUP BY s.date;

-- =============================================================================
-- STEP 3: CREATE FIXED WEEKLY SUMMARY TABLE
-- =============================================================================

CREATE TABLE weekly_active_users_summary (
    week Date,
    active_users_state AggregateFunction(uniq, String),
    has_profile_state AggregateFunction(uniq, String),
    has_follows_list_state AggregateFunction(uniq, String),
    has_profile_and_follows_list_state AggregateFunction(uniq, String),
    total_events_state AggregateFunction(sum, UInt64),
    events_with_profile_state AggregateFunction(sum, UInt64),
    events_with_follows_list_state AggregateFunction(sum, UInt64),
    events_with_profile_and_follows_list_state AggregateFunction(sum, UInt64)
) ENGINE = AggregatingMergeTree()
ORDER BY week;

CREATE MATERIALIZED VIEW weekly_active_users_summary_mv TO weekly_active_users_summary AS
SELECT
    toMonday(s.date) AS week,
    uniqState(s.pubkey) AS active_users_state,
    uniqStateIf(s.pubkey, s.has_profile = 1) AS has_profile_state,
    uniqStateIf(s.pubkey, s.has_follows = 1) AS has_follows_list_state,
    uniqStateIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list_state,
    sumState(toUInt64(s.event_count)) AS total_events_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1) AS events_with_profile_state,
    sumStateIf(toUInt64(s.event_count), s.has_follows = 1) AS events_with_follows_list_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list_state
FROM daily_user_stats s
GROUP BY week;

-- =============================================================================
-- STEP 4: CREATE FIXED MONTHLY SUMMARY TABLE
-- =============================================================================

CREATE TABLE monthly_active_users_summary (
    month Date,
    active_users_state AggregateFunction(uniq, String),
    has_profile_state AggregateFunction(uniq, String),
    has_follows_list_state AggregateFunction(uniq, String),
    has_profile_and_follows_list_state AggregateFunction(uniq, String),
    total_events_state AggregateFunction(sum, UInt64),
    events_with_profile_state AggregateFunction(sum, UInt64),
    events_with_follows_list_state AggregateFunction(sum, UInt64),
    events_with_profile_and_follows_list_state AggregateFunction(sum, UInt64)
) ENGINE = AggregatingMergeTree()
ORDER BY month;

CREATE MATERIALIZED VIEW monthly_active_users_summary_mv TO monthly_active_users_summary AS
SELECT
    toStartOfMonth(s.date) AS month,
    uniqState(s.pubkey) AS active_users_state,
    uniqStateIf(s.pubkey, s.has_profile = 1) AS has_profile_state,
    uniqStateIf(s.pubkey, s.has_follows = 1) AS has_follows_list_state,
    uniqStateIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list_state,
    sumState(toUInt64(s.event_count)) AS total_events_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1) AS events_with_profile_state,
    sumStateIf(toUInt64(s.event_count), s.has_follows = 1) AS events_with_follows_list_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list_state
FROM daily_user_stats s
GROUP BY month;

-- =============================================================================
-- STEP 5: BACKFILL ALL SUMMARY TABLES FROM daily_user_stats
-- =============================================================================

SELECT 'Backfilling daily_active_users_summary...' AS status;
INSERT INTO daily_active_users_summary
SELECT
    s.date AS date,
    uniqState(s.pubkey) AS active_users_state,
    uniqStateIf(s.pubkey, s.has_profile = 1) AS has_profile_state,
    uniqStateIf(s.pubkey, s.has_follows = 1) AS has_follows_list_state,
    uniqStateIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list_state,
    sumState(toUInt64(s.event_count)) AS total_events_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1) AS events_with_profile_state,
    sumStateIf(toUInt64(s.event_count), s.has_follows = 1) AS events_with_follows_list_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list_state
FROM daily_user_stats s
GROUP BY s.date;

SELECT 'Backfilling weekly_active_users_summary...' AS status;
INSERT INTO weekly_active_users_summary
SELECT
    toMonday(s.date) AS week,
    uniqState(s.pubkey) AS active_users_state,
    uniqStateIf(s.pubkey, s.has_profile = 1) AS has_profile_state,
    uniqStateIf(s.pubkey, s.has_follows = 1) AS has_follows_list_state,
    uniqStateIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list_state,
    sumState(toUInt64(s.event_count)) AS total_events_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1) AS events_with_profile_state,
    sumStateIf(toUInt64(s.event_count), s.has_follows = 1) AS events_with_follows_list_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list_state
FROM daily_user_stats s
GROUP BY week;

SELECT 'Backfilling monthly_active_users_summary...' AS status;
INSERT INTO monthly_active_users_summary
SELECT
    toStartOfMonth(s.date) AS month,
    uniqState(s.pubkey) AS active_users_state,
    uniqStateIf(s.pubkey, s.has_profile = 1) AS has_profile_state,
    uniqStateIf(s.pubkey, s.has_follows = 1) AS has_follows_list_state,
    uniqStateIf(s.pubkey, s.has_profile = 1 AND s.has_follows = 1) AS has_profile_and_follows_list_state,
    sumState(toUInt64(s.event_count)) AS total_events_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1) AS events_with_profile_state,
    sumStateIf(toUInt64(s.event_count), s.has_follows = 1) AS events_with_follows_list_state,
    sumStateIf(toUInt64(s.event_count), s.has_profile = 1 AND s.has_follows = 1) AS events_with_profile_and_follows_list_state
FROM daily_user_stats s
GROUP BY month;

OPTIMIZE TABLE daily_active_users_summary FINAL;
OPTIMIZE TABLE weekly_active_users_summary FINAL;
OPTIMIZE TABLE monthly_active_users_summary FINAL;

-- =============================================================================
-- STEP 6: RECREATE VIEWS TO USE *Merge FUNCTIONS
-- =============================================================================
-- The views use *Merge functions to combine the aggregate states into final values.

DROP VIEW IF EXISTS daily_active_users;
DROP VIEW IF EXISTS weekly_active_users;
DROP VIEW IF EXISTS monthly_active_users;

-- Daily Active Users view
-- Uses uniqMerge/sumMerge to finalize the aggregate states
CREATE VIEW daily_active_users AS
SELECT
    date,
    uniqMerge(active_users_state) AS active_users,
    uniqMerge(has_profile_state) AS has_profile,
    uniqMerge(has_follows_list_state) AS has_follows_list,
    uniqMerge(has_profile_and_follows_list_state) AS has_profile_and_follows_list,
    sumMerge(total_events_state) AS total_events,
    sumMerge(events_with_profile_state) AS events_with_profile,
    sumMerge(events_with_follows_list_state) AS events_with_follows_list,
    sumMerge(events_with_profile_and_follows_list_state) AS events_with_profile_and_follows_list
FROM daily_active_users_summary
WHERE date >= toDate('2020-11-07')  -- Nostr genesis
  AND date <= today()               -- Exclude future dates
GROUP BY date
ORDER BY date DESC;

-- Weekly Active Users view
CREATE VIEW weekly_active_users AS
SELECT
    week,
    uniqMerge(active_users_state) AS active_users,
    uniqMerge(has_profile_state) AS has_profile,
    uniqMerge(has_follows_list_state) AS has_follows_list,
    uniqMerge(has_profile_and_follows_list_state) AS has_profile_and_follows_list,
    sumMerge(total_events_state) AS total_events,
    sumMerge(events_with_profile_state) AS events_with_profile,
    sumMerge(events_with_follows_list_state) AS events_with_follows_list,
    sumMerge(events_with_profile_and_follows_list_state) AS events_with_profile_and_follows_list
FROM weekly_active_users_summary
WHERE week >= toDate('2020-11-07')  -- Nostr genesis
  AND week <= toMonday(today())     -- Exclude future weeks
GROUP BY week
ORDER BY week DESC;

-- Monthly Active Users view
CREATE VIEW monthly_active_users AS
SELECT
    month,
    uniqMerge(active_users_state) AS active_users,
    uniqMerge(has_profile_state) AS has_profile,
    uniqMerge(has_follows_list_state) AS has_follows_list,
    uniqMerge(has_profile_and_follows_list_state) AS has_profile_and_follows_list,
    sumMerge(total_events_state) AS total_events,
    sumMerge(events_with_profile_state) AS events_with_profile,
    sumMerge(events_with_follows_list_state) AS events_with_follows_list,
    sumMerge(events_with_profile_and_follows_list_state) AS events_with_profile_and_follows_list
FROM monthly_active_users_summary
WHERE month >= toDate('2020-11-07')    -- Nostr genesis
  AND month <= toStartOfMonth(today()) -- Exclude future months
GROUP BY month
ORDER BY month DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 008 complete. Verifying data...' AS status;

SELECT 'Summary table sizes:' AS check;
SELECT
    name,
    engine,
    formatReadableSize(total_bytes) AS size,
    total_rows
FROM system.tables
WHERE database = currentDatabase()
    AND name IN ('daily_active_users_summary', 'weekly_active_users_summary', 'monthly_active_users_summary')
ORDER BY name;

SELECT 'Recent daily_active_users (should show consistent data):' AS check;
SELECT date, active_users, total_events
FROM daily_active_users
WHERE date >= today() - 14
ORDER BY date DESC
LIMIT 14;

SELECT 'Date range verification:' AS check;
SELECT
    'daily' AS view,
    min(date) AS earliest,
    max(date) AS latest,
    count() AS days
FROM daily_active_users;

