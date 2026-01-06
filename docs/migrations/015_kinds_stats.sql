-- Migration: 015_kinds_stats
-- Description: Pre-aggregated kind statistics with exact unique pubkey counts
-- Date: 2026-01-06
--
-- Replaces on-demand uniq() with scheduled uniqExact() for accurate counts.
-- Refreshes hourly - kinds don't change that frequently.
--
-- To run: just ch-migrate docs/migrations/015_kinds_stats.sql

-- Drop existing view if present (for idempotency)
DROP VIEW IF EXISTS kinds_stats_mv;

-- Create the refreshable materialized view
-- Uses ReplacingMergeTree to handle the full refresh pattern
-- SETTINGS clause enables experimental feature for this statement
CREATE MATERIALIZED VIEW IF NOT EXISTS kinds_stats_mv
REFRESH EVERY 1 HOUR
ENGINE = ReplacingMergeTree()
ORDER BY kind
AS
SELECT
    kind,
    count() AS event_count,
    uniqExact(pubkey) AS unique_pubkeys,
    toUInt32(min(created_at)) AS first_seen,
    toUInt32(max(created_at)) AS last_seen
FROM events_local
GROUP BY kind
SETTINGS allow_experimental_refreshable_materialized_view = 1;

-- Force initial population
SYSTEM REFRESH VIEW kinds_stats_mv;

-- Verification
SELECT 'Migration 015 complete' AS status;
SELECT count() AS kinds_count FROM kinds_stats_mv;
SELECT * FROM kinds_stats_mv ORDER BY event_count DESC LIMIT 10;

