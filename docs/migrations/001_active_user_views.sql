-- Migration: 001_active_user_views
-- Description: Improve active user views to exclude throwaway pubkeys (Gift Wrap, Marmot)
--              and add breakdown columns for user types and event counts by user type
-- Date: 2025-12-19
--
-- To run this migration:
--   just ch-migrate docs/migrations/001_active_user_views.sql
--
-- Or via docker:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/001_active_user_views.sql

-- =============================================================================
-- DROP OLD VIEWS
-- =============================================================================
-- Views must be dropped in order (dependents first)

DROP VIEW IF EXISTS daily_active_users;
DROP VIEW IF EXISTS weekly_active_users;
DROP VIEW IF EXISTS monthly_active_users;
DROP VIEW IF EXISTS pubkeys_with_profile;
DROP VIEW IF EXISTS pubkeys_with_follows;

-- =============================================================================
-- CREATE NEW HELPER VIEWS
-- =============================================================================

-- Helper view: pubkeys that have published a profile (kind 0)
CREATE VIEW pubkeys_with_profile AS
SELECT DISTINCT pubkey FROM events_local WHERE kind = 0;

-- Helper view: pubkeys that have published a follows list (kind 3)
CREATE VIEW pubkeys_with_follows AS
SELECT DISTINCT pubkey FROM events_local WHERE kind = 3;

-- =============================================================================
-- CREATE NEW ACTIVE USER VIEWS
-- =============================================================================
--
-- Active user heuristic: Count all pubkeys EXCEPT those using throwaway keys.
-- Throwaway key kinds (pubkey intentionally hides real sender):
-- - 445: Marmot Group Event (E2EE messaging via MLS protocol)
-- - 1059: Gift Wrap (NIP-59, privacy wrapper for DMs)
--
-- User count columns:
-- - active_users: all unique pubkeys active in the period
-- - has_profile: subset with kind 0 (profile metadata) ever published
-- - has_follows_list: subset with kind 3 (follows list) ever published
-- - has_profile_and_follows_list: subset with both (strongest signal of real user)
--
-- Event count columns:
-- - total_events: count of all events in the period
-- - events_with_profile: events from users who have a profile
-- - events_with_follows_list: events from users who have a follows list
-- - events_with_profile_and_follows_list: events from users with both

CREATE VIEW daily_active_users AS
SELECT
    toDate(created_at) AS date,
    -- User counts
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
    uniqIf(pubkey,
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS has_profile_and_follows_list,
    -- Event counts
    count() AS total_events,
    countIf(pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS events_with_profile,
    countIf(pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS events_with_follows_list,
    countIf(
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS events_with_profile_and_follows_list
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY date
ORDER BY date DESC;

CREATE VIEW weekly_active_users AS
SELECT
    toMonday(created_at) AS week,
    -- User counts
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
    uniqIf(pubkey,
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS has_profile_and_follows_list,
    -- Event counts
    count() AS total_events,
    countIf(pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS events_with_profile,
    countIf(pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS events_with_follows_list,
    countIf(
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS events_with_profile_and_follows_list
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY week
ORDER BY week DESC;

CREATE VIEW monthly_active_users AS
SELECT
    toStartOfMonth(created_at) AS month,
    -- User counts
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows_list,
    uniqIf(pubkey,
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS has_profile_and_follows_list,
    -- Event counts
    count() AS total_events,
    countIf(pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS events_with_profile,
    countIf(pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS events_with_follows_list,
    countIf(
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS events_with_profile_and_follows_list
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY month
ORDER BY month DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration complete. Verifying views exist:' AS status;

SELECT
    name,
    engine
FROM system.tables
WHERE database = currentDatabase()
    AND name IN ('pubkeys_with_profile', 'pubkeys_with_follows', 'daily_active_users', 'weekly_active_users', 'monthly_active_users')
ORDER BY name;
