-- Rollback: 020_incremental_derived_views (phase 2: cutover)
-- Description: Restore the pre-020 definitions of daily_user_stats_mv
--              (migration 005), active_users_{daily,weekly,monthly}_mv (013),
--              relay_distribution_mv (014) and kinds_stats_mv (015).
-- Date: 2026-10-12
--
-- To run:
--   just ch-migrate docs/migrations/rollback/020_undo_derived_views_cutover.sql
--
-- IMPORTANT:
--   - This brings back the costs 020 removed: ~6-8 s of IN-subquery work in
--     every ingest insert, an hourly 180 GiB rescan and a 6-hourly 1.3 TiB
--     rescan.
--   - kinds_stats_mv is recreated EMPTY and refreshes at once (a full rescan,
--     several minutes). The pre-020 API reads it: wait until
--       SELECT view, status, last_success_time FROM system.view_refreshes
--       WHERE database = 'nostr' AND view = 'kinds_stats_mv'
--     shows a success before pointing the API back at a pre-020 release.
--   - relay_distribution_mv is recreated with EMPTY: relay_distribution keeps
--     its current rows and the 014 rescan resumes at the next 00/06/12/18 UTC.
--   - daily_user_stats rows written between the cutover and this rollback
--     store their flags as 0 (the flags lived in pubkey_flags). The restored
--     013 refreshes read only stored flags, so has_profile / has_follows /
--     has_both undercount the days in that interval until they leave the
--     refresh windows. active_users and total_events are unaffected.
--   - Phase 1 objects (pubkey_flags, relay_list_latest, kind_totals and their
--     views) stay. Drop them with
--     rollback/020_drop_incremental_derived_views.sql afterwards if wanted.

SET allow_experimental_refreshable_materialized_view = 1;

-- 1. daily_user_stats_mv: the insert-time lookups of migration 005.
ALTER TABLE nostr.daily_user_stats_mv MODIFY QUERY
SELECT
    toDate(created_at) AS date,
    pubkey,
    toUInt8(pubkey IN (SELECT pubkey FROM nostr.pubkeys_with_profile_data)) AS has_profile,
    toUInt8(pubkey IN (SELECT pubkey FROM nostr.pubkeys_with_follows_data)) AS has_follows,
    toUInt32(1) AS event_count
FROM nostr.events_local
WHERE kind NOT IN (445, 1059);

-- 2. Active users: migration 013's refreshes.
DROP VIEW IF EXISTS nostr.active_users_daily_mv;
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
GROUP BY s.date
SETTINGS allow_experimental_refreshable_materialized_view = 1;

DROP VIEW IF EXISTS nostr.active_users_weekly_mv;
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
GROUP BY week
SETTINGS allow_experimental_refreshable_materialized_view = 1;

DROP VIEW IF EXISTS nostr.active_users_monthly_mv;
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
GROUP BY month
SETTINGS allow_experimental_refreshable_materialized_view = 1;

-- 3. Relay distribution: migration 014's 6-hourly rescan (EMPTY: no rescan now).
DROP VIEW IF EXISTS nostr.relay_distribution_mv;
CREATE MATERIALIZED VIEW nostr.relay_distribution_mv
REFRESH EVERY 6 HOUR
TO nostr.relay_distribution
EMPTY
AS
WITH normalized AS (
    SELECT
        pubkey,
        replaceRegexpOne(
            replaceRegexpOne(
                lower(trim(tag[2])),
                '/+$', ''
            ),
            ':443$', ''
        ) AS relay_url,
        tag[3] AS marker
    FROM (
        SELECT
            pubkey,
            argMax(tags, created_at) AS tags
        FROM nostr.events_local
        WHERE kind = 10002
        GROUP BY pubkey
    )
    ARRAY JOIN arrayFilter(t -> t[1] = 'r' AND length(t) >= 2, tags) AS tag
)
SELECT
    relay_url,
    count() AS user_count,
    countIf(marker = '' OR marker = 'read') AS read_count,
    countIf(marker = '' OR marker = 'write') AS write_count,
    now() AS updated_at
FROM normalized
WHERE
    relay_url LIKE 'wss://%'
    AND length(relay_url) > 6
    AND NOT match(relay_url, '[ \n\r\t,]')
    AND relay_url NOT LIKE '%.local%'
    AND relay_url NOT LIKE '%localhost%'
    AND relay_url NOT LIKE '%127.0.0.1%'
    AND relay_url NOT LIKE '%0.0.0.0%'
    AND relay_url NOT LIKE '%wss://%wss://%'
    AND relay_url NOT LIKE '%http%'
GROUP BY relay_url
HAVING user_count >= 10
SETTINGS allow_experimental_refreshable_materialized_view = 1;

-- 4. Kinds: migration 015's hourly rescan (refreshes at once).
CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.kinds_stats_mv
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
FROM nostr.events_local
GROUP BY kind
SETTINGS allow_experimental_refreshable_materialized_view = 1;

-- VERIFICATION
SELECT 'Migration 020 cutover undone' AS step, view, status, last_refresh_time, next_refresh_time
FROM system.view_refreshes
WHERE database = 'nostr'
ORDER BY view;

SELECT 'daily_user_stats_mv' AS step, as_select
FROM system.tables
WHERE database = 'nostr' AND name = 'daily_user_stats_mv';
