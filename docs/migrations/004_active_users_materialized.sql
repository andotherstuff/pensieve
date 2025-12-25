-- Migration: 004_active_users_materialized
-- Description: Convert active user views to materialized views for performance at scale
--              The previous regular views scan 800M+ rows on every query - these MVs
--              pre-aggregate data for millisecond query times.
-- Date: 2025-12-25
--
-- To run this migration:
--   just ch-migrate docs/migrations/004_active_users_materialized.sql
--
-- Or via docker:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/004_active_users_materialized.sql
--
-- IMPORTANT: The backfill step can take a while on large datasets (800M+ rows).
--            Consider running during low-traffic periods.

-- =============================================================================
-- STEP 1: DROP OLD VIEWS
-- =============================================================================
-- We'll replace these with MVs + query views

DROP VIEW IF EXISTS daily_active_users;
DROP VIEW IF EXISTS weekly_active_users;
DROP VIEW IF EXISTS monthly_active_users;
DROP VIEW IF EXISTS pubkeys_with_profile;
DROP VIEW IF EXISTS pubkeys_with_follows;

-- =============================================================================
-- STEP 2: CREATE PUBKEY LOOKUP TABLES
-- =============================================================================
-- These are small tables (millions of rows, not billions) that track which
-- pubkeys have profiles and follows lists. ReplacingMergeTree deduplicates.

-- Drop if exists (for idempotency)
DROP TABLE IF EXISTS pubkeys_with_profile_mv;
DROP TABLE IF EXISTS pubkeys_with_profile_data;
DROP TABLE IF EXISTS pubkeys_with_follows_mv;
DROP TABLE IF EXISTS pubkeys_with_follows_data;

-- Pubkeys with profile (kind = 0)
CREATE TABLE pubkeys_with_profile_data (
    pubkey String
) ENGINE = ReplacingMergeTree()
ORDER BY pubkey;

CREATE MATERIALIZED VIEW pubkeys_with_profile_mv TO pubkeys_with_profile_data AS
SELECT pubkey FROM events_local WHERE kind = 0;

-- Pubkeys with follows list (kind = 3)
CREATE TABLE pubkeys_with_follows_data (
    pubkey String
) ENGINE = ReplacingMergeTree()
ORDER BY pubkey;

CREATE MATERIALIZED VIEW pubkeys_with_follows_mv TO pubkeys_with_follows_data AS
SELECT pubkey FROM events_local WHERE kind = 3;

-- =============================================================================
-- STEP 3: CREATE DAILY ACTIVE USERS MV
-- =============================================================================
-- Uses AggregatingMergeTree with uniqState for accurate unique counts.
-- Stores intermediate state that gets merged at query time.

DROP TABLE IF EXISTS daily_active_users_mv;
DROP TABLE IF EXISTS daily_active_users_data;

CREATE TABLE daily_active_users_data (
    date Date,
    -- Aggregate states for unique pubkey counts
    all_pubkeys_state AggregateFunction(uniq, String),
    -- Event counts (simple sums)
    total_events UInt64
) ENGINE = AggregatingMergeTree()
ORDER BY date;

CREATE MATERIALIZED VIEW daily_active_users_mv TO daily_active_users_data AS
SELECT
    toDate(created_at) AS date,
    uniqState(pubkey) AS all_pubkeys_state,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)  -- Exclude throwaway key kinds
GROUP BY date;

-- =============================================================================
-- STEP 4: CREATE WEEKLY ACTIVE USERS MV
-- =============================================================================

DROP TABLE IF EXISTS weekly_active_users_mv;
DROP TABLE IF EXISTS weekly_active_users_data;

CREATE TABLE weekly_active_users_data (
    week Date,
    all_pubkeys_state AggregateFunction(uniq, String),
    total_events UInt64
) ENGINE = AggregatingMergeTree()
ORDER BY week;

CREATE MATERIALIZED VIEW weekly_active_users_mv TO weekly_active_users_data AS
SELECT
    toMonday(created_at) AS week,
    uniqState(pubkey) AS all_pubkeys_state,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY week;

-- =============================================================================
-- STEP 5: CREATE MONTHLY ACTIVE USERS MV
-- =============================================================================

DROP TABLE IF EXISTS monthly_active_users_mv;
DROP TABLE IF EXISTS monthly_active_users_data;

CREATE TABLE monthly_active_users_data (
    month Date,
    all_pubkeys_state AggregateFunction(uniq, String),
    total_events UInt64
) ENGINE = AggregatingMergeTree()
ORDER BY month;

CREATE MATERIALIZED VIEW monthly_active_users_mv TO monthly_active_users_data AS
SELECT
    toStartOfMonth(created_at) AS month,
    uniqState(pubkey) AS all_pubkeys_state,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY month;

-- =============================================================================
-- STEP 6: CREATE DAILY PUBKEY TRACKING (for breakdown queries)
-- =============================================================================
-- This stores which pubkeys were active on which days, enabling fast
-- JOIN-based breakdown queries. Uses ReplacingMergeTree to deduplicate.

DROP TABLE IF EXISTS daily_pubkeys_mv;
DROP TABLE IF EXISTS daily_pubkeys_data;

CREATE TABLE daily_pubkeys_data (
    date Date,
    pubkey String
) ENGINE = ReplacingMergeTree()
ORDER BY (date, pubkey);

CREATE MATERIALIZED VIEW daily_pubkeys_mv TO daily_pubkeys_data AS
SELECT
    toDate(created_at) AS date,
    pubkey
FROM events_local
WHERE kind NOT IN (445, 1059);

-- =============================================================================
-- STEP 7: BACKFILL ALL MVS FROM EXISTING DATA
-- =============================================================================
-- This is the slow part - runs once to populate MVs with historical data.

SELECT 'Backfilling pubkeys_with_profile_data...' AS status;
INSERT INTO pubkeys_with_profile_data
SELECT DISTINCT pubkey FROM events_local WHERE kind = 0;

SELECT 'Backfilling pubkeys_with_follows_data...' AS status;
INSERT INTO pubkeys_with_follows_data
SELECT DISTINCT pubkey FROM events_local WHERE kind = 3;

SELECT 'Backfilling daily_active_users_data...' AS status;
INSERT INTO daily_active_users_data
SELECT
    toDate(created_at) AS date,
    uniqState(pubkey) AS all_pubkeys_state,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY date;

SELECT 'Backfilling weekly_active_users_data...' AS status;
INSERT INTO weekly_active_users_data
SELECT
    toMonday(created_at) AS week,
    uniqState(pubkey) AS all_pubkeys_state,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY week;

SELECT 'Backfilling monthly_active_users_data...' AS status;
INSERT INTO monthly_active_users_data
SELECT
    toStartOfMonth(created_at) AS month,
    uniqState(pubkey) AS all_pubkeys_state,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY month;

SELECT 'Backfilling daily_pubkeys_data...' AS status;
INSERT INTO daily_pubkeys_data
SELECT DISTINCT
    toDate(created_at) AS date,
    pubkey
FROM events_local
WHERE kind NOT IN (445, 1059);

-- Optimize tables to merge parts
SELECT 'Optimizing tables...' AS status;
OPTIMIZE TABLE pubkeys_with_profile_data FINAL;
OPTIMIZE TABLE pubkeys_with_follows_data FINAL;
OPTIMIZE TABLE daily_active_users_data FINAL;
OPTIMIZE TABLE weekly_active_users_data FINAL;
OPTIMIZE TABLE monthly_active_users_data FINAL;
OPTIMIZE TABLE daily_pubkeys_data FINAL;

-- =============================================================================
-- STEP 8: CREATE QUERY VIEWS (same names as before for dashboard compatibility)
-- =============================================================================
-- These views query the pre-aggregated MV tables for fast results.

-- Helper views for compatibility
CREATE VIEW pubkeys_with_profile AS
SELECT pubkey FROM pubkeys_with_profile_data FINAL;

CREATE VIEW pubkeys_with_follows AS
SELECT pubkey FROM pubkeys_with_follows_data FINAL;

-- Daily active users - queries from daily_pubkeys_data with LEFT JOINs for breakdown
-- Uses the pre-materialized pubkey lookup tables for fast IN checks
CREATE VIEW daily_active_users AS
SELECT
    dp.date AS date,
    -- User counts from daily_pubkeys_data
    uniq(dp.pubkey) AS active_users,
    uniqIf(dp.pubkey, pp.pubkey IS NOT NULL) AS has_profile,
    uniqIf(dp.pubkey, pf.pubkey IS NOT NULL) AS has_follows_list,
    uniqIf(dp.pubkey, pp.pubkey IS NOT NULL AND pf.pubkey IS NOT NULL) AS has_profile_and_follows_list,
    -- Event counts from aggregated data (joined)
    any(agg.total_events) AS total_events,
    -- Event breakdown (approximate - based on user breakdown ratio)
    toUInt64(any(agg.total_events) * uniqIf(dp.pubkey, pp.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_profile,
    toUInt64(any(agg.total_events) * uniqIf(dp.pubkey, pf.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_follows_list,
    toUInt64(any(agg.total_events) * uniqIf(dp.pubkey, pp.pubkey IS NOT NULL AND pf.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_profile_and_follows_list
FROM daily_pubkeys_data dp FINAL
LEFT JOIN pubkeys_with_profile_data pp FINAL ON dp.pubkey = pp.pubkey
LEFT JOIN pubkeys_with_follows_data pf FINAL ON dp.pubkey = pf.pubkey
LEFT JOIN daily_active_users_data agg ON dp.date = agg.date
GROUP BY dp.date
ORDER BY dp.date DESC;

-- Weekly active users
CREATE VIEW weekly_active_users AS
SELECT
    toMonday(dp.date) AS week,
    uniq(dp.pubkey) AS active_users,
    uniqIf(dp.pubkey, pp.pubkey IS NOT NULL) AS has_profile,
    uniqIf(dp.pubkey, pf.pubkey IS NOT NULL) AS has_follows_list,
    uniqIf(dp.pubkey, pp.pubkey IS NOT NULL AND pf.pubkey IS NOT NULL) AS has_profile_and_follows_list,
    sum(agg.total_events) AS total_events,
    toUInt64(sum(agg.total_events) * uniqIf(dp.pubkey, pp.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_profile,
    toUInt64(sum(agg.total_events) * uniqIf(dp.pubkey, pf.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_follows_list,
    toUInt64(sum(agg.total_events) * uniqIf(dp.pubkey, pp.pubkey IS NOT NULL AND pf.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_profile_and_follows_list
FROM daily_pubkeys_data dp FINAL
LEFT JOIN pubkeys_with_profile_data pp FINAL ON dp.pubkey = pp.pubkey
LEFT JOIN pubkeys_with_follows_data pf FINAL ON dp.pubkey = pf.pubkey
LEFT JOIN daily_active_users_data agg ON dp.date = agg.date
GROUP BY week
ORDER BY week DESC;

-- Monthly active users
CREATE VIEW monthly_active_users AS
SELECT
    toStartOfMonth(dp.date) AS month,
    uniq(dp.pubkey) AS active_users,
    uniqIf(dp.pubkey, pp.pubkey IS NOT NULL) AS has_profile,
    uniqIf(dp.pubkey, pf.pubkey IS NOT NULL) AS has_follows_list,
    uniqIf(dp.pubkey, pp.pubkey IS NOT NULL AND pf.pubkey IS NOT NULL) AS has_profile_and_follows_list,
    sum(agg.total_events) AS total_events,
    toUInt64(sum(agg.total_events) * uniqIf(dp.pubkey, pp.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_profile,
    toUInt64(sum(agg.total_events) * uniqIf(dp.pubkey, pf.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_follows_list,
    toUInt64(sum(agg.total_events) * uniqIf(dp.pubkey, pp.pubkey IS NOT NULL AND pf.pubkey IS NOT NULL) / uniq(dp.pubkey)) AS events_with_profile_and_follows_list
FROM daily_pubkeys_data dp FINAL
LEFT JOIN pubkeys_with_profile_data pp FINAL ON dp.pubkey = pp.pubkey
LEFT JOIN pubkeys_with_follows_data pf FINAL ON dp.pubkey = pf.pubkey
LEFT JOIN daily_active_users_data agg ON dp.date = agg.date
GROUP BY month
ORDER BY month DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration complete. Verifying tables and views:' AS status;

SELECT
    name,
    engine,
    total_rows,
    formatReadableSize(total_bytes) AS size
FROM system.tables
WHERE database = currentDatabase()
    AND name IN (
        'pubkeys_with_profile_data', 'pubkeys_with_follows_data',
        'daily_active_users_data', 'weekly_active_users_data', 'monthly_active_users_data',
        'daily_pubkeys_data',
        'pubkeys_with_profile', 'pubkeys_with_follows',
        'daily_active_users', 'weekly_active_users', 'monthly_active_users'
    )
ORDER BY name;

-- Quick test query
SELECT 'Testing daily_active_users view (last 7 days):' AS status;
SELECT date, active_users, has_profile, total_events
FROM daily_active_users
WHERE date >= today() - 7
ORDER BY date DESC
LIMIT 7;

