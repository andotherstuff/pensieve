-- Prepared migration: 020_incremental_derived_views (phase 2 of 2: cutover)
-- Description: Switch the active-users refreshes and daily_user_stats_mv to
--              the user flags, recompute relay_distribution from
--              relay_list_latest, and drop the kinds_stats_mv rescan.
-- Date: 2026-10-12
--
-- Prepared for ClickHouse 24.8.14.39.
--
-- Run with the guarded tool (it refuses unless the phase 1 backfills are
-- complete and nothing still reads kinds_stats_mv, and records the time):
--   api-rollups cutover            (or: api-rollups cutover --dry-run)
--
-- Rollback:
--   just ch-migrate docs/migrations/rollback/020_undo_derived_views_cutover.sql
--
-- IMPORTANT:
--   - Only after phase 1 (020_incremental_derived_views.sql), a complete
--     `api-rollups backfill --migration 020`, a passing `validate`, and the API
--     release that reads kind_totals (the old API reads kinds_stats_mv, which
--     this file drops).
--   - Every statement is idempotent; re-running the file is safe.
--   - Run outside :58-:07 UTC. Creating a refreshable view runs it at once:
--     expect one refresh of each active-users view and of relay_distribution_mv
--     right after this file (system.view_refreshes).
--   - The settings are inline (not SET) so the tool can run the statements
--     one at a time.

-- =============================================================================
-- 1. ACTIVE USERS: flags from pubkey_flags instead of insert-time lookups
-- =============================================================================
--
-- Same schedules, windows, targets and columns as migration 013. A row's
-- profile (follows) flag is
--   - its stored flag, for rows written by the old insert-time lookup
--     (last_indexed_at = 1970-01-01); `> 0`, because SummingMergeTree SUMS the
--     stored UInt8 flags on merge (013's `= 1` missed every merged row whose
--     events were flagged more than once);
--   - profile_since < last_indexed_at, for rows written after the cutover:
--     some event of the row was inserted after the pubkey's first kind 0.
-- A pubkey missing from pubkey_flags joins with known = 0 and is unflagged.
-- The right side is every flagged pubkey (~30M): one join build per refresh
-- (hourly, 12-hourly, daily) instead of two ~30M-row set builds per ingest
-- insert (~2,800/day). It is keyed by sipHash128(pubkey) (16 bytes instead of
-- 64; a collision is ~1e-23 likely) and uses grace_hash with a 512 MiB
-- in-memory bound, spilling the rest to disk: with 25M flagged pubkeys and 30M
-- (date, pubkey) rows a plain hash join on the hex string needed > 7 GiB, this
-- one peaks near 1 GiB.

DROP VIEW IF EXISTS nostr.active_users_daily_mv;
CREATE MATERIALIZED VIEW nostr.active_users_daily_mv
REFRESH EVERY 1 HOUR
TO nostr.active_users_daily
AS
SELECT
    s.date AS date,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.profile) AS has_profile,
    uniqIf(s.pubkey, s.follows) AS has_follows,
    uniqIf(s.pubkey, s.profile AND s.follows) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM (
    SELECT
        d.date AS date,
        d.pubkey AS pubkey,
        d.event_count AS event_count,
        d.has_profile > 0 OR (f.known = 1 AND d.last_indexed_at > f.profile_since) AS profile,
        d.has_follows > 0 OR (f.known = 1 AND d.last_indexed_at > f.follows_since) AS follows
    FROM nostr.daily_user_stats AS d
    LEFT ANY JOIN (
        SELECT
            sipHash128(pubkey) AS key,
            profile_since,
            follows_since,
            toUInt8(1) AS known
        FROM nostr.pubkey_flags FINAL
    ) AS f ON sipHash128(d.pubkey) = f.key
    WHERE d.date >= today() - 30 AND d.date <= today()
) AS s
GROUP BY s.date
SETTINGS
    join_use_nulls = 0,
    join_algorithm = 'grace_hash',
    max_bytes_in_join = 536870912,
    allow_experimental_refreshable_materialized_view = 1;

DROP VIEW IF EXISTS nostr.active_users_weekly_mv;
CREATE MATERIALIZED VIEW nostr.active_users_weekly_mv
REFRESH EVERY 12 HOUR
TO nostr.active_users_weekly
AS
SELECT
    toMonday(s.date) AS week,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.profile) AS has_profile,
    uniqIf(s.pubkey, s.follows) AS has_follows,
    uniqIf(s.pubkey, s.profile AND s.follows) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM (
    SELECT
        d.date AS date,
        d.pubkey AS pubkey,
        d.event_count AS event_count,
        d.has_profile > 0 OR (f.known = 1 AND d.last_indexed_at > f.profile_since) AS profile,
        d.has_follows > 0 OR (f.known = 1 AND d.last_indexed_at > f.follows_since) AS follows
    FROM nostr.daily_user_stats AS d
    LEFT ANY JOIN (
        SELECT
            sipHash128(pubkey) AS key,
            profile_since,
            follows_since,
            toUInt8(1) AS known
        FROM nostr.pubkey_flags FINAL
    ) AS f ON sipHash128(d.pubkey) = f.key
    WHERE d.date >= toMonday(today()) - 182 AND d.date <= today()
) AS s
GROUP BY week
SETTINGS
    join_use_nulls = 0,
    join_algorithm = 'grace_hash',
    max_bytes_in_join = 536870912,
    allow_experimental_refreshable_materialized_view = 1;

DROP VIEW IF EXISTS nostr.active_users_monthly_mv;
CREATE MATERIALIZED VIEW nostr.active_users_monthly_mv
REFRESH EVERY 1 DAY
TO nostr.active_users_monthly
AS
SELECT
    toStartOfMonth(s.date) AS month,
    uniq(s.pubkey) AS active_users,
    uniqIf(s.pubkey, s.profile) AS has_profile,
    uniqIf(s.pubkey, s.follows) AS has_follows,
    uniqIf(s.pubkey, s.profile AND s.follows) AS has_both,
    sum(s.event_count) AS total_events,
    now() AS updated_at
FROM (
    SELECT
        d.date AS date,
        d.pubkey AS pubkey,
        d.event_count AS event_count,
        d.has_profile > 0 OR (f.known = 1 AND d.last_indexed_at > f.profile_since) AS profile,
        d.has_follows > 0 OR (f.known = 1 AND d.last_indexed_at > f.follows_since) AS follows
    FROM nostr.daily_user_stats AS d
    LEFT ANY JOIN (
        SELECT
            sipHash128(pubkey) AS key,
            profile_since,
            follows_since,
            toUInt8(1) AS known
        FROM nostr.pubkey_flags FINAL
    ) AS f ON sipHash128(d.pubkey) = f.key
    WHERE d.date >= toStartOfMonth(today()) - 365 AND d.date <= today()
) AS s
GROUP BY month
SETTINGS
    join_use_nulls = 0,
    join_algorithm = 'grace_hash',
    max_bytes_in_join = 536870912,
    allow_experimental_refreshable_materialized_view = 1;

-- =============================================================================
-- 2. DAILY USER STATS: no lookups in the insert path
-- =============================================================================
--
-- Atomic for inserts: an insert builds its view pipeline once, from either the
-- old or the new query, so every event is counted exactly once. Flags are
-- written as 0 and last_indexed_at carries the insert time for the refreshes
-- above, which must therefore be switched first (they are, in step 1).

ALTER TABLE nostr.daily_user_stats_mv MODIFY QUERY
SELECT
    toDate(created_at) AS date,
    pubkey,
    toUInt8(0) AS has_profile,
    toUInt8(0) AS has_follows,
    toUInt32(1) AS event_count,
    indexed_at AS last_indexed_at
FROM nostr.events_local
WHERE kind NOT IN (445, 1059);

-- =============================================================================
-- 3. RELAY DISTRIBUTION: from relay_list_latest, hourly at :20
-- =============================================================================
--
-- Migration 014's query with the per-pubkey argMax(tags, created_at) over
-- events_local replaced by the stored winner's 'r' tags. Same target table,
-- columns, normalisation and filters; the refresh replaces the table's
-- contents atomically, as before. It reads a few million small states, so it
-- runs hourly, off the :00-:07 window.

DROP VIEW IF EXISTS nostr.relay_distribution_mv;
CREATE MATERIALIZED VIEW nostr.relay_distribution_mv
REFRESH EVERY 1 HOUR OFFSET 20 MINUTE
TO nostr.relay_distribution
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
            tupleElement(argMinMerge(latest), 3) AS relays
        FROM nostr.relay_list_latest
        GROUP BY pubkey
    )
    ARRAY JOIN relays AS tag
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

-- =============================================================================
-- 4. KINDS: drop the hourly rescan (the API reads kind_totals)
-- =============================================================================

DROP VIEW IF EXISTS nostr.kinds_stats_mv;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration 020 cutover' AS step, view, status, last_refresh_time, next_refresh_time
FROM system.view_refreshes
WHERE database = 'nostr'
ORDER BY view;

SELECT 'daily_user_stats_mv' AS step, as_select
FROM system.tables
WHERE database = 'nostr' AND name = 'daily_user_stats_mv';
