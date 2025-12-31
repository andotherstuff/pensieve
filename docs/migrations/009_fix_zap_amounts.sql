-- Migration: 009_fix_zap_amounts
-- Description: Fix zap amount parsing to reject amounts without valid multipliers.
--              The original migration incorrectly treated missing multipliers as BTC,
--              causing massive inflation for malformed invoices.
-- Date: 2025-12-31
--
-- To run this migration:
--   just ch-migrate docs/migrations/009_fix_zap_amounts.sql
--
-- This migration:
-- 1. Drops and recreates the materialized view with fixed parsing
-- 2. Truncates the data table
-- 3. Backfills from events_local

-- =============================================================================
-- DROP AND RECREATE VIEW WITH FIXED PARSING
-- =============================================================================

DROP VIEW IF EXISTS zap_amounts_mv;

-- Recreate with safer parsing: only accept known multipliers (m, u, n, p)
-- If no valid multiplier is matched, return 0 (invalid amount)
-- EXCLUDE amounts over 1M sats (0.01 BTC) - anything higher is almost certainly fake
-- Data analysis shows 99.9% of zaps are under 1M sats (p99.9 = 900K)
-- We set fake amounts to 0 rather than capping, so they don't inflate totals
CREATE MATERIALIZED VIEW IF NOT EXISTS zap_amounts_mv TO zap_amounts_data
AS SELECT
    id AS event_id,
    pubkey AS zapper_pubkey,
    created_at,
    -- Parse bolt11 amount with strict multiplier validation AND sanity threshold
    -- bolt11 format: lnbc<amount><multiplier>1<rest>
    -- Only accept m, u, n, p multipliers. No multiplier = invalid (not BTC!)
    -- Amounts over 1M sats (1B msats) are set to 0 (excluded as fake)
    if(
        multiIf(
            -- milli-BTC: 1m = 100,000 sats = 100,000,000 msats
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1')) * 100000000,
            -- micro-BTC: 1u = 100 sats = 100,000 msats
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1')) * 100000,
            -- nano-BTC: 1n = 0.1 sats = 100 msats
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1')) * 100,
            -- pico-BTC: 1p = 0.0001 sats = 0.1 msats
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1') != '',
            intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1')), 10),
            -- No valid multiplier = invalid, return 0
            toUInt64(0)
        ) > toUInt64(1000000000),  -- If over 1M sats (1B msats = 0.01 BTC)
        toUInt64(0),                -- Set to 0 (exclude as fake)
        multiIf(
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1')) * 100000000,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1')) * 100000,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1')) * 100,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1') != '',
            intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1')), 10),
            toUInt64(0)
        )
    ) AS amount_msats,
    -- Extract recipient (p tag)
    (arrayFilter(t -> t[1] = 'p', tags)[1])[2] AS recipient_pubkey,
    -- Extract sender (P tag, optional - may be empty if anonymous zap)
    (arrayFilter(t -> t[1] = 'P', tags)[1])[2] AS sender_pubkey
FROM events_local
WHERE kind = 9735
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]+[munp]1'), tags);

-- =============================================================================
-- TRUNCATE AND BACKFILL DATA
-- =============================================================================

TRUNCATE TABLE zap_amounts_data;

INSERT INTO zap_amounts_data
SELECT
    id AS event_id,
    pubkey AS zapper_pubkey,
    created_at,
    -- Same logic as MV: parse with multiplier validation, EXCLUDE amounts over 1M sats
    if(
        multiIf(
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1')) * 100000000,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1')) * 100000,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1')) * 100,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1') != '',
            intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1')), 10),
            toUInt64(0)
        ) > toUInt64(1000000000),  -- If over 1M sats (1B msats = 0.01 BTC)
        toUInt64(0),                -- Set to 0 (exclude as fake)
        multiIf(
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)m1')) * 100000000,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)u1')) * 100000,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1') != '',
            toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)n1')) * 100,
            extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1') != '',
            intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)p1')), 10),
            toUInt64(0)
        )
    ) AS amount_msats,
    (arrayFilter(t -> t[1] = 'p', tags)[1])[2] AS recipient_pubkey,
    (arrayFilter(t -> t[1] = 'P', tags)[1])[2] AS sender_pubkey
FROM events_local
WHERE kind = 9735
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]+[munp]1'), tags);

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT '=== Migration 009 complete. Running verification checks ===' AS status;

-- 1. Overall stats
SELECT '1. Overall zap statistics:' AS check;
SELECT
    count() AS total_zaps,
    countIf(amount_msats > 0) AS valid_amount_zaps,
    countIf(amount_msats = 0) AS zero_amount_zaps,
    toUInt64(sum(amount_msats) / 1000) AS total_sats,
    round(avg(amount_msats) / 1000, 2) AS avg_sats,
    round(quantile(0.5)(amount_msats) / 1000, 2) AS median_sats,
    max(amount_msats) / 1000 AS max_sats
FROM zap_amounts_data;

-- 2. Check for excluded (zeroed) fake amounts and distribution of large zaps
SELECT '2. Checking for excluded/large zaps:' AS check;
SELECT
    countIf(amount_msats = 0) AS excluded_fake_or_invalid,
    countIf(amount_msats > 0 AND amount_msats <= 100000000) AS under_1k_sats,
    countIf(amount_msats > 100000000 AND amount_msats <= 1000000000) AS 1k_to_10k_sats,
    countIf(amount_msats > 1000000000 AND amount_msats <= 100000000000) AS 10k_to_100k_sats,
    countIf(amount_msats > 100000000000 AND amount_msats <= 1000000000000) AS 100k_to_1m_sats,
    max(amount_msats) / 1000 AS max_sats_should_be_under_1m
FROM zap_amounts_data;

-- 3. Sample the largest zaps to verify they're reasonable
SELECT '3. Top 10 largest zaps (verify these are reasonable):' AS check;
SELECT
    event_id,
    amount_msats / 1000 AS sats,
    created_at
FROM zap_amounts_data
WHERE amount_msats > 0
ORDER BY amount_msats DESC
LIMIT 10;

-- 4. Compare count with original kind 9735 events
SELECT '4. Coverage check (zaps parsed vs total kind 9735 events):' AS check;
SELECT
    (SELECT count() FROM events_local WHERE kind = 9735) AS total_9735_events,
    (SELECT count() FROM zap_amounts_data) AS parsed_zaps,
    round(100.0 * (SELECT count() FROM zap_amounts_data) /
          nullIf((SELECT count() FROM events_local WHERE kind = 9735), 0), 2) AS coverage_pct;

-- 5. Show any events NOT parsed (to understand what we're missing)
SELECT '5. Sample of kind 9735 events NOT in zap_amounts_data (if any):' AS check;
SELECT
    substring((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], 1, 40) AS bolt11_prefix
FROM events_local
WHERE kind = 9735
    AND id NOT IN (SELECT event_id FROM zap_amounts_data)
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11', tags)
LIMIT 10;

SELECT '=== Verification complete ===' AS status;

