-- Migration: 007_filter_future_dates_in_views
-- Description: Update active user views to filter out future-dated events at query time.
--              Events with created_at in the future (2100, 2077, etc.) will be excluded.
-- Date: 2025-12-31
--
-- To run this migration:
--   just ch-migrate docs/migrations/007_filter_future_dates_in_views.sql
--
-- This migration is safe to re-run (uses CREATE OR REPLACE).

-- =============================================================================
-- UPDATE VIEWS TO FILTER FUTURE DATES
-- =============================================================================
-- The underlying data tables keep all data (including future dates) for accuracy.
-- The views filter to show only valid dates (<= today).

-- Daily Active Users - filter to valid dates only
-- Nostr genesis: November 7, 2020 (first commit)
CREATE OR REPLACE VIEW daily_active_users AS
SELECT * FROM daily_active_users_summary FINAL
WHERE date >= toDate('2020-11-07')  -- Nostr genesis
  AND date <= today()               -- Exclude future dates
ORDER BY date DESC;

-- Weekly Active Users - filter to valid dates only
CREATE OR REPLACE VIEW weekly_active_users AS
SELECT * FROM weekly_active_users_summary FINAL
WHERE week >= toDate('2020-11-07')  -- Nostr genesis
  AND week <= toMonday(today())     -- Exclude future weeks
ORDER BY week DESC;

-- Monthly Active Users - filter to valid dates only
CREATE OR REPLACE VIEW monthly_active_users AS
SELECT * FROM monthly_active_users_summary FINAL
WHERE month >= toDate('2020-11-07')    -- Nostr genesis
  AND month <= toStartOfMonth(today()) -- Exclude future months
ORDER BY month DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 007 complete. Views now filter future dates.' AS status;

SELECT 'Recent daily_active_users (should show valid dates only):' AS check;
SELECT date, active_users, total_events
FROM daily_active_users
LIMIT 10;

SELECT 'Date range check:' AS check;
SELECT
    'daily' AS view,
    min(date) AS earliest,
    max(date) AS latest
FROM daily_active_users
UNION ALL
SELECT
    'weekly' AS view,
    min(week) AS earliest,
    max(week) AS latest
FROM weekly_active_users
UNION ALL
SELECT
    'monthly' AS view,
    min(month) AS earliest,
    max(month) AS latest
FROM monthly_active_users;

