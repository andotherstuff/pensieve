-- Migration: 001_active_user_views
-- Description: Improve active user views to exclude throwaway pubkeys (Gift Wrap, Marmot)
--              and add breakdown columns for has_profile, has_follows, has_both
-- Date: 2025-12-19
--
-- To run this migration:
--   clickhouse-client --host <host> --user <user> --password <pass> --database nostr < docs/migrations/001_active_user_views.sql
--
-- Or via HTTP:
--   curl -X POST 'http://localhost:8123/?database=nostr' --data-binary @docs/migrations/001_active_user_views.sql

-- =============================================================================
-- DROP OLD VIEWS
-- =============================================================================
-- Views must be dropped in order (dependents first)

DROP VIEW IF EXISTS daily_active_users;
DROP VIEW IF EXISTS weekly_active_users;
DROP VIEW IF EXISTS monthly_active_users;

-- =============================================================================
-- CREATE NEW HELPER VIEWS
-- =============================================================================

-- Helper view: pubkeys that have published a profile (kind 0)
DROP VIEW IF EXISTS pubkeys_with_profile;
CREATE VIEW pubkeys_with_profile AS
SELECT DISTINCT pubkey FROM events_local WHERE kind = 0;

-- Helper view: pubkeys that have published a follows list (kind 3)
DROP VIEW IF EXISTS pubkeys_with_follows;
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
-- Columns:
-- - active_users: all unique pubkeys active in the period
-- - has_profile: subset with kind 0 (profile metadata) ever published
-- - has_follows: subset with kind 3 (follows list) ever published
-- - has_both: subset with both profile AND follows (strongest signal of real user)
-- - total_events: count of all events in the period

CREATE VIEW daily_active_users AS
SELECT
    toDate(created_at) AS date,
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows,
    uniqIf(pubkey,
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS has_both,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY date
ORDER BY date DESC;

CREATE VIEW weekly_active_users AS
SELECT
    toMonday(created_at) AS week,
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows,
    uniqIf(pubkey,
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS has_both,
    count() AS total_events
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY week
ORDER BY week DESC;

CREATE VIEW monthly_active_users AS
SELECT
    toStartOfMonth(created_at) AS month,
    uniq(pubkey) AS active_users,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_profile)) AS has_profile,
    uniqIf(pubkey, pubkey IN (SELECT pubkey FROM pubkeys_with_follows)) AS has_follows,
    uniqIf(pubkey,
        pubkey IN (SELECT pubkey FROM pubkeys_with_profile)
        AND pubkey IN (SELECT pubkey FROM pubkeys_with_follows)
    ) AS has_both,
    count() AS total_events
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

