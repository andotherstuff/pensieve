-- Migration: 014_relay_distribution
-- Description: Pre-computed relay distribution from latest NIP-65 relay lists (kind 10002)
-- Date: 2026-01-05
--
-- This creates a table showing how many users list each relay in their latest
-- kind 10002 event, with read/write breakdowns.
--
-- Normalization applied:
--   - Trim whitespace
--   - Lowercase
--   - Remove trailing slashes (one or more)
--   - Remove default port :443
--
-- Filtering applied:
--   - Must start with wss:// (ws:// is insecure and filtered out)
--   - No spaces, newlines, tabs, or commas in the URL (indicates garbage data)
--   - No localhost/local addresses
--   - At least 10 users (noise filter)
--
-- To run: just ch-migrate docs/migrations/014_relay_distribution.sql

SET allow_experimental_refreshable_materialized_view = 1;

-- =============================================================================
-- PART 1: DROP EXISTING (for idempotency)
-- =============================================================================

SELECT '=== Dropping existing objects (if any) ===' AS status;

DROP TABLE IF EXISTS nostr.relay_distribution_mv;
DROP TABLE IF EXISTS nostr.relay_distribution;

-- =============================================================================
-- PART 2: CREATE TARGET TABLE
-- =============================================================================

SELECT '=== Creating relay_distribution table ===' AS status;

-- Stores pre-computed relay distribution from latest NIP-65 events per user
-- ReplacingMergeTree deduplicates by relay_url, keeping the latest updated_at
CREATE TABLE nostr.relay_distribution (
    relay_url String,           -- Normalized: lowercase, no trailing slash, wss://
    user_count UInt64,          -- Number of users listing this relay
    read_count UInt64,          -- Users listing for read (includes read+write)
    write_count UInt64,         -- Users listing for write (includes read+write)
    updated_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY relay_url;

-- =============================================================================
-- PART 3: INITIAL BACKFILL
-- =============================================================================

SELECT '=== Backfilling relay_distribution ===' AS status;

INSERT INTO nostr.relay_distribution (relay_url, user_count, read_count, write_count, updated_at)
WITH normalized AS (
    SELECT
        pubkey,
        -- Normalize the URL:
        -- 1. Trim whitespace
        -- 2. Lowercase
        -- 3. Remove trailing slashes
        -- 4. Remove default port :443
        replaceRegexpOne(
            replaceRegexpOne(
                lower(trim(tag[2])),
                '/+$', ''                            -- Remove trailing slashes
            ),
            ':443$', ''                              -- Remove default wss port
        ) AS relay_url,
        tag[3] AS marker
    FROM (
        SELECT
            pubkey,
            argMax(tags, created_at) AS tags
        FROM events_local
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
    -- Must start with wss:// (after normalization)
    relay_url LIKE 'wss://%'
    -- Must have something after the protocol
    AND length(relay_url) > 6
    -- No spaces, newlines, tabs, or commas (garbage data indicators)
    AND NOT match(relay_url, '[ \n\r\t,]')
    -- No localhost/local addresses
    AND relay_url NOT LIKE '%.local%'
    AND relay_url NOT LIKE '%localhost%'
    AND relay_url NOT LIKE '%127.0.0.1%'
    AND relay_url NOT LIKE '%0.0.0.0%'
    -- No obviously malformed URLs
    AND relay_url NOT LIKE '%wss://%wss://%'  -- Multiple protocols
    AND relay_url NOT LIKE '%http%'            -- HTTP mixed in
GROUP BY relay_url
HAVING user_count >= 10;  -- Filter out noise (relays with <10 users)

SELECT 'Backfill complete.' AS status;

-- Optimize to merge parts
OPTIMIZE TABLE nostr.relay_distribution FINAL;

-- =============================================================================
-- PART 4: CREATE REFRESHABLE MV
-- =============================================================================

SELECT '=== Creating refreshable MV ===' AS status;

-- Refresh every 6 hours - relay lists don't change that frequently
CREATE MATERIALIZED VIEW nostr.relay_distribution_mv
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
        FROM events_local
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
HAVING user_count >= 10;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT '=== VERIFICATION ===' AS status;

SELECT 'Top 20 relays by user count:' AS info;
SELECT relay_url, user_count, read_count, write_count
FROM nostr.relay_distribution FINAL
ORDER BY user_count DESC
LIMIT 20;

SELECT 'Total relays tracked:' AS info;
SELECT count() AS total_relays FROM nostr.relay_distribution FINAL;

SELECT 'Checking for any remaining damus variants:' AS info;
SELECT relay_url, user_count
FROM nostr.relay_distribution FINAL
WHERE relay_url LIKE '%damus%'
ORDER BY user_count DESC;

SELECT 'Refresh status:' AS info;
SELECT view, status, last_refresh_time, next_refresh_time
FROM system.view_refreshes
WHERE database = 'nostr' AND view = 'relay_distribution_mv';

SELECT '=== Migration 014 complete ===' AS status;
