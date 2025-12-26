
-- Weekly Active Users
CREATE VIEW weekly_active_users AS
SELECT
    toMonday(date) AS week,
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, has_profile > 0) AS has_profile,
    uniqIf(pubkey, has_follows > 0) AS has_follows_list,
    uniqIf(pubkey, has_profile > 0 AND has_follows > 0) AS has_profile_and_follows_list,
    sum(event_count) AS total_events,
    sumIf(event_count, has_profile > 0) AS events_with_profile,
    sumIf(event_count, has_follows > 0) AS events_with_follows_list,
    sumIf(event_count, has_profile > 0 AND has_follows > 0) AS events_with_profile_and_follows_list
FROM daily_user_stats
GROUP BY week
ORDER BY week DESC;

-- Monthly Active Users
CREATE VIEW monthly_active_users AS
SELECT
    toStartOfMonth(date) AS month,
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, has_profile > 0) AS has_profile,
    uniqIf(pubkey, has_follows > 0) AS has_follows_list,
    uniqIf(pubkey, has_profile > 0 AND has_follows > 0) AS has_profile_and_follows_list,
    sum(event_count) AS total_events,
    sumIf(event_count, has_profile > 0) AS events_with_profile,
    sumIf(event_count, has_follows > 0) AS events_with_follows_list,
    sumIf(event_count, has_profile > 0 AND has_follows > 0) AS events_with_profile_and_follows_list
FROM daily_user_stats
GROUP BY month
ORDER BY month DESC;

-- =============================================================================
-- STEP 5: CLEAN UP OLD TABLES (optional - keep for now as backup)
-- =============================================================================
-- The following tables from migration 004 are no longer needed but we'll keep them
-- in case we need to rollback. They can be dropped later with:
--   DROP TABLE IF EXISTS daily_pubkeys_data;
--   DROP TABLE IF EXISTS daily_pubkeys_mv;
--   DROP TABLE IF EXISTS daily_active_users_data;
--   DROP TABLE IF EXISTS daily_active_users_mv;
--   DROP TABLE IF EXISTS weekly_active_users_data;
--   DROP TABLE IF EXISTS weekly_active_users_mv;
--   DROP TABLE IF EXISTS monthly_active_users_data;
--   DROP TABLE IF EXISTS monthly_active_users_mv;

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

SELECT 'Testing daily_active_users (last 7 days):' AS status;
SELECT date, active_users, has_profile, has_follows_list, total_events
FROM daily_active_users
WHERE date >= today() - 7
ORDER BY date DESC
LIMIT 7;

SELECT 'Migration 005 complete!' AS status;
