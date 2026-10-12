-- Test fixture: events_local as production has it on 2026-10-12
-- (docs/clickhouse_self_hosted.sql minus the projections, which production
-- dropped). Integration tests rewrite the `nostr.` qualifier to a scratch
-- database.

CREATE TABLE IF NOT EXISTS nostr.events_local (
    id String,
    pubkey String,
    created_at DateTime,
    kind UInt16,
    content String CODEC(ZSTD(3)),
    sig String,
    tags Array(Array(String)),
    indexed_at DateTime DEFAULT now(),
    relay_source String DEFAULT '',
    d_tag String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'd', tags), 1)[2],
    title String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'title', tags), 1)[2],
    thumbnail String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'thumb', tags), 1)[2],
    video_url String MATERIALIZED arrayElement(arrayFilter(t -> t[1] = 'url', tags), 1)[2],
    INDEX idx_created_at created_at TYPE minmax GRANULARITY 4,
    INDEX idx_kind kind TYPE minmax GRANULARITY 4,
    INDEX idx_pubkey pubkey TYPE bloom_filter(0.01) GRANULARITY 4
) ENGINE = ReplacingMergeTree(indexed_at)
ORDER BY (id)
SETTINGS index_granularity = 8192;
