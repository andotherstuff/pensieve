-- Migration: 003_zap_amounts
-- Description: Parse bolt11 invoices from zap receipts (kind 9735) to extract amounts.
--              This enables zap statistics without runtime bolt11 parsing.
-- Date: 2025-12-23
--
-- To run this migration:
--   just ch-migrate docs/migrations/003_zap_amounts.sql
--
-- Or via docker:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/003_zap_amounts.sql
--
-- IMPORTANT: After running this migration, you must backfill historical data.
-- See docs/migrations/README.md for the backfill command.

-- =============================================================================
-- ZAP AMOUNTS TABLE
-- =============================================================================
--
-- Stores parsed zap amounts from kind 9735 (zap receipt) events.
-- Amount is extracted from the bolt11 invoice in the "bolt11" tag.
--
-- Bolt11 format: lnbc<amount><multiplier>1<rest>
-- Multipliers (amount is in BTC base units):
--   m = milli (10^-3 BTC) = 100,000,000 msats per unit (10^8)
--   u = micro (10^-6 BTC) = 100,000 msats per unit (10^5)
--   n = nano  (10^-9 BTC) = 100 msats per unit (10^2)
--   p = pico  (10^-12 BTC) = 0.1 msats per unit (sub-msat, truncated to 0)
--   (none) = 1 BTC = 100,000,000,000 msats per unit (10^11)
--
-- Example: lnbc420u1... = 420 micro-BTC = 42,000,000 msats = 42,000 sats

CREATE TABLE IF NOT EXISTS zap_amounts_data (
    event_id String,
    zapper_pubkey String,          -- pubkey of the zap receipt event (LNURL server)
    created_at DateTime,
    amount_msats UInt64,           -- amount in millisatoshis
    recipient_pubkey String,       -- p tag: who received the zap
    sender_pubkey String           -- P tag: who sent the zap (optional, may be empty)
) ENGINE = ReplacingMergeTree(created_at)
ORDER BY (event_id);

-- Materialized view to extract zap amounts from new events
-- Uses separate regex patterns since ClickHouse extract() only returns first capture group
CREATE MATERIALIZED VIEW IF NOT EXISTS zap_amounts_mv TO zap_amounts_data
AS SELECT
    id AS event_id,
    pubkey AS zapper_pubkey,
    created_at,
    -- Parse bolt11 amount using separate patterns for number and multiplier
    -- bolt11 format: lnbc<amount><multiplier>1<rest>
    -- Note: multipliers convert bolt11 amount to millisatoshis
    -- 1 BTC = 100,000,000 sats = 100,000,000,000 msats
    multiIf(
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'm',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000000,      -- milli: 10^8 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'u',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000,         -- micro: 10^5 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'n',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100,            -- nano: 10^2 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'p',
        intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')), 10),      -- pico: 0.1 msats (truncated)
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = '',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000000000,   -- BTC: 10^11 msats
        toUInt64(0)
    ) AS amount_msats,
    -- Extract recipient (p tag) - same pattern as d_tag materialized column
    (arrayFilter(t -> t[1] = 'p', tags)[1])[2] AS recipient_pubkey,
    -- Extract sender (P tag, optional - may be empty if anonymous zap)
    (arrayFilter(t -> t[1] = 'P', tags)[1])[2] AS sender_pubkey
FROM events_local
WHERE kind = 9735
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]'), tags);

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration complete. Verifying tables/views exist:' AS status;

SELECT
    name,
    engine
FROM system.tables
WHERE database = currentDatabase()
    AND name IN ('zap_amounts_data', 'zap_amounts_mv')
ORDER BY name;

SELECT 'REMINDER: Run backfill query to populate historical zap data!' AS reminder;
