-- Test fixture: production-shaped events_local plus the legacy zap_amounts objects
-- (docs/clickhouse_self_hosted.sql, 003_zap_amounts.sql, 009_fix_zap_amounts.sql).
-- Integration tests rewrite the `nostr.` qualifier to a scratch database.

CREATE TABLE IF NOT EXISTS nostr.events_local (
    -- Event fields (NIP-01)
    id String,                    -- 32-byte hex event ID (SHA-256 hash, always 64 chars)
    pubkey String,                -- 32-byte hex public key (always 64 chars)
    created_at DateTime,          -- Unix timestamp when event was created
    kind UInt16,                  -- Event kind (0-65535, see NIP-01)
    content String CODEC(ZSTD(3)), -- Event content (arbitrary string)
    sig String,                   -- 64-byte hex Schnorr signature (always 128 chars)
    tags Array(Array(String)),    -- Nested array of tags

    -- Metadata fields
    indexed_at DateTime DEFAULT now(),
    relay_source String DEFAULT '',

    -- Materialized columns for video events (computed at insert time)
    d_tag String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'd', tags), 1)[2],
    title String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'title', tags), 1)[2],
    thumbnail String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'thumb', tags), 1)[2],
    video_url String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'url', tags), 1)[2],

    -- Secondary indexes (fallback for queries not matching projections)
    INDEX idx_created_at created_at TYPE minmax GRANULARITY 4,
    INDEX idx_kind kind TYPE minmax GRANULARITY 4,
    INDEX idx_pubkey pubkey TYPE bloom_filter(0.01) GRANULARITY 4,

    -- Projections for alternate query patterns
    -- ClickHouse automatically selects the best projection for each query
    PROJECTION events_by_kind (
        SELECT *
        ORDER BY (kind, created_at, pubkey)
    ),
    PROJECTION events_by_author (
        SELECT *
        ORDER BY (pubkey, created_at, kind)
    )

) ENGINE = ReplacingMergeTree(indexed_at)
ORDER BY (id)
SETTINGS index_granularity = 8192,
         deduplicate_merge_projection_mode = 'rebuild';

CREATE TABLE IF NOT EXISTS nostr.zap_amounts_data (
    event_id String,
    zapper_pubkey String,          -- pubkey of the zap receipt event (LNURL server)
    created_at DateTime,
    amount_msats UInt64,           -- amount in millisatoshis
    recipient_pubkey String,       -- p tag: who received the zap
    sender_pubkey String           -- P tag: who sent the zap (optional, may be empty)
) ENGINE = ReplacingMergeTree(created_at)
ORDER BY (event_id);

CREATE MATERIALIZED VIEW IF NOT EXISTS nostr.zap_amounts_mv TO nostr.zap_amounts_data
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
FROM nostr.events_local
WHERE kind = 9735
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]+[munp]1'), tags);
