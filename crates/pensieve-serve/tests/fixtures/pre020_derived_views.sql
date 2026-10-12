-- Test fixture: the derived views migration 020 replaces, as production has
-- them on 2026-10-12 (migrations 004, 005, 013, 014 and 015, qualified and
-- without their one-off backfills). Integration tests rewrite the `nostr.`
-- qualifier to a scratch database. Settings are inline because the tests run
-- one statement per request.

-- 004: lookup tables of pubkeys with a profile (kind 0) / follow list (kind 3).
CREATE TABLE IF NOT EXISTS nostr.pubkeys_with_profile_data (
    pubkey String
) ENGINE = ReplacingMergeTree()
ORDER BY pubkey;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.pubkeys_with_profile_mv TO nostr.pubkeys_with_profile_data AS
SELECT pubkey FROM nostr.events_local WHERE kind = 0;

CREATE TABLE IF NOT EXISTS nostr.pubkeys_with_follows_data (
    pubkey String
) ENGINE = ReplacingMergeTree()
ORDER BY pubkey;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.pubkeys_with_follows_mv TO nostr.pubkeys_with_follows_data AS
SELECT pubkey FROM nostr.events_local WHERE kind = 3;

-- 005: daily_user_stats and its insert-time lookups.
CREATE TABLE IF NOT EXISTS nostr.daily_user_stats (
    date Date,
    pubkey String,
    has_profile UInt8,
    has_follows UInt8,
    event_count UInt32
) ENGINE = SummingMergeTree()
ORDER BY (date, pubkey);

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.daily_user_stats_mv TO nostr.daily_user_stats AS
SELECT
    toDate(created_at) AS date,
    pubkey,
    toUInt8(pubkey IN (SELECT pubkey FROM nostr.pubkeys_with_profile_data)) AS has_profile,
    toUInt8(pubkey IN (SELECT pubkey FROM nostr.pubkeys_with_follows_data)) AS has_follows,
    toUInt32(1) AS event_count
FROM nostr.events_local
WHERE kind NOT IN (445, 1059);

-- 013: active users tables and their refreshes.
CREATE TABLE IF NOT EXISTS nostr.active_users_daily (
    date Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows UInt64,
    has_both UInt64,
    total_events UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY date;

CREATE TABLE IF NOT EXISTS nostr.active_users_weekly (
    week Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows UInt64,
    has_both UInt64,
    total_events UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY week;

CREATE TABLE IF NOT EXISTS nostr.active_users_monthly (
    month Date,
    active_users UInt64,
    has_profile UInt64,
    has_follows UInt64,
    has_both UInt64,
    total_events UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY month;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.active_users_daily_mv
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

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.active_users_weekly_mv
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

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.active_users_monthly_mv
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

-- 014: relay distribution, a 6-hourly rescan of events_local.
CREATE TABLE IF NOT EXISTS nostr.relay_distribution (
    relay_url String,
    user_count UInt64,
    read_count UInt64,
    write_count UInt64,
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY relay_url;

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.relay_distribution_mv
REFRESH EVERY 6 HOUR
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

-- 015: kind statistics, an hourly rescan of events_local.
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
