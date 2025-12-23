-- Migration: 003_zap_amounts
-- Description: Parse bolt11 invoices from zap receipts (kind 9735) to extract amounts.
--              This enables zap statistics without runtime bolt11 parsing.
-- Date: 2025-12-23
--
-- To run this migration:
--   just ch-migrate docs/migrations/003_zap_amounts.sql
--
-- IMPORTANT: After running this migration, you must backfill historical data:
--   just ch-query "INSERT INTO zap_amounts_data SELECT id, pubkey, created_at, extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1] AS amount_raw, extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] AS multiplier, multiIf(multiplier = 'm', toUInt64(amount_raw) * 100000000000, multiplier = 'u', toUInt64(amount_raw) * 100000000, multiplier = 'n', toUInt64(amount_raw) * 100000, multiplier = 'p', toUInt64(amount_raw) * 100, multiplier = '', toUInt64(amount_raw) * 100000000000000, 0) AS amount_msats, arrayElement(arrayFilter(t -> t[1] = 'p', tags), 1)[2] AS recipient_pubkey, arrayElement(arrayFilter(t -> t[1] = 'P', tags), 1)[2] AS sender_pubkey FROM events_local ARRAY JOIN tags AS tag WHERE kind = 9735 AND tag[1] = 'bolt11' AND length(tag) >= 2 AND match(tag[2], '^lnbc[0-9]')"
--
-- Or via docker:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/003_zap_amounts.sql

-- =============================================================================
-- ZAP AMOUNTS TABLE
-- =============================================================================
--
-- Stores parsed zap amounts from kind 9735 (zap receipt) events.
-- Amount is extracted from the bolt11 invoice in the "bolt11" tag.
--
-- Bolt11 format: lnbc<amount><multiplier>1<rest>
-- Multipliers (amount is in BTC base units):
--   m = milli (10^-3 BTC) = 100,000,000,000 msats per unit
--   u = micro (10^-6 BTC) = 100,000,000 msats per unit
--   n = nano  (10^-9 BTC) = 100,000 msats per unit
--   p = pico  (10^-12 BTC) = 100 msats per unit
--   (none) = 1 BTC = 100,000,000,000,000 msats per unit
--
-- Example: lnbc10u1... = 10 micro-BTC = 1,000,000,000 msats = 1000 sats

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
CREATE MATERIALIZED VIEW IF NOT EXISTS zap_amounts_mv TO zap_amounts_data
AS SELECT
    id AS event_id,
    pubkey AS zapper_pubkey,
    created_at,
    -- Parse bolt11 amount: extract digits and multiplier, then convert to msats
    multiIf(
        -- milli: 10^-3 BTC = 10^11 msats per unit
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'm',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000000000,
        -- micro: 10^-6 BTC = 10^8 msats per unit
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'u',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000000,
        -- nano: 10^-9 BTC = 10^5 msats per unit
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'n',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000,
        -- pico: 10^-12 BTC = 10^2 msats per unit
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'p',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100,
        -- no multiplier: 1 BTC = 10^14 msats per unit (rare but valid)
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = '',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000000000000,
        -- fallback
        0
    ) AS amount_msats,
    -- Extract recipient (p tag)
    arrayElement(arrayFilter(t -> t[1] = 'p', tags), 1)[2] AS recipient_pubkey,
    -- Extract sender (P tag, optional)
    arrayElement(arrayFilter(t -> t[1] = 'P', tags), 1)[2] AS sender_pubkey
FROM events_local
ARRAY JOIN tags AS tag
WHERE kind = 9735
    AND tag[1] = 'bolt11'
    AND length(tag) >= 2
    AND match(tag[2], '^lnbc[0-9]');

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

