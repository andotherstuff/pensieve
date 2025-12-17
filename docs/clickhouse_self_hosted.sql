-- Pensieve ClickHouse Schema (Self-Hosted Edition)
-- Version: 0.1.0
-- Description: Schema optimized for self-hosted ClickHouse with projections
--
-- This schema is designed for self-hosted ClickHouse deployments using
-- standard MergeTree engines. It includes projections for optimized query
-- performance across different access patterns.

-- =============================================================================
-- DATABASE SETUP
-- =============================================================================

CREATE DATABASE IF NOT EXISTS nostr;
USE nostr;

-- =============================================================================
-- DROP ALL (for fresh install - WARNING: deletes all data!)
-- =============================================================================

-- -- Views (must drop before tables they depend on)
-- DROP VIEW IF EXISTS trending_videos;
-- DROP VIEW IF EXISTS video_stats;
-- DROP VIEW IF EXISTS video_hashtags;
-- DROP VIEW IF EXISTS popular_video_hashtags;
-- DROP VIEW IF EXISTS videos;
-- DROP VIEW IF EXISTS daily_active_users;
-- DROP VIEW IF EXISTS weekly_active_users;
-- DROP VIEW IF EXISTS monthly_active_users;
-- DROP VIEW IF EXISTS user_profiles;
-- DROP VIEW IF EXISTS top_video_creators;
-- DROP VIEW IF EXISTS event_stats;
-- DROP VIEW IF EXISTS tag_stats;
-- DROP VIEW IF EXISTS activity_by_kind;

-- -- Materialized Views (DROP TABLE works for MVs)
-- DROP TABLE IF EXISTS event_tags_flat;
-- DROP TABLE IF EXISTS deleted_event_ids_mv;
-- DROP TABLE IF EXISTS reaction_counts_mv;
-- DROP TABLE IF EXISTS comment_counts_mv;
-- DROP TABLE IF EXISTS repost_counts_mv;

-- -- Base tables
-- DROP TABLE IF EXISTS event_tags_flat_data;
-- DROP TABLE IF EXISTS deleted_event_ids;
-- DROP TABLE IF EXISTS reaction_counts;
-- DROP TABLE IF EXISTS comment_counts;
-- DROP TABLE IF EXISTS repost_counts;
-- DROP TABLE IF EXISTS events_local;

-- =============================================================================
-- MAIN EVENTS TABLE
-- =============================================================================

-- Main events table.
-- NOTE: Ingestion should prefer at-most-once insertion by event id; some downstream
-- materialized views are not duplicate-safe if duplicates are inserted.
-- We still use ReplacingMergeTree as defense-in-depth in case duplicates slip through.
-- ORDER BY (id) ensures merges are keyed by event id.
-- No partitioning - we store all historic data.
--
-- Materialized columns extract common video tags at insert time for faster queries.
--
-- PROJECTIONS provide alternate sort orders for common query patterns:
-- - events_by_time: Optimized for time-range queries (e.g., "last 24 hours")
-- - events_by_kind: Optimized for kind-filtered queries (e.g., "all reactions")
-- - events_by_author: Optimized for author queries (e.g., "videos by pubkey X")
CREATE TABLE IF NOT EXISTS events_local (
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
    PROJECTION events_by_time (
        SELECT *
        ORDER BY (created_at, kind, pubkey)
    ),
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

-- =============================================================================
-- TAG MATERIALIZED VIEW
-- =============================================================================

-- Flattened tag storage table (for advanced tag queries)
-- ORDER BY optimized: low cardinality (tag_name) -> high cardinality (event_id)
-- created_at moved to end as it's typically a range filter
CREATE TABLE IF NOT EXISTS event_tags_flat_data (
    event_id String,
    pubkey String,
    kind UInt16,
    tag_name String,
    tag_value_primary String,
    tag_value_position_3 String,
    tag_value_position_4 String,
    tag_value_position_5 String,
    tag_value_position_6 String,
    tag_full Array(String),
    tag_value_count UInt8,
    created_at DateTime,

    INDEX idx_kind kind TYPE minmax GRANULARITY 4
) ENGINE = MergeTree()
ORDER BY (tag_name, tag_value_primary, event_id, created_at)
SETTINGS index_granularity = 8192;

-- Materialized view to populate tag data using ARRAY JOIN syntax
CREATE MATERIALIZED VIEW IF NOT EXISTS event_tags_flat TO event_tags_flat_data
AS SELECT
    id AS event_id,
    pubkey,
    kind,
    tag[1] AS tag_name,
    tag[2] AS tag_value_primary,
    tag[3] AS tag_value_position_3,
    tag[4] AS tag_value_position_4,
    tag[5] AS tag_value_position_5,
    tag[6] AS tag_value_position_6,
    tag AS tag_full,
    toUInt8(length(tag) - 1) AS tag_value_count,
    created_at
FROM events_local
ARRAY JOIN tags AS tag
WHERE length(tag) >= 1;

-- =============================================================================
-- DELETION INDEX (NIP-09)
-- =============================================================================
--
-- Deletion events (kind 5) are stored in `events_local` like any other event.
-- This table indexes deleted event ids so queries can optionally filter out deleted
-- content without mutating the base events table.
--
-- NOTE: This indexes `e` tags (event ids). Indexing addressable deletions (`a` tags)
-- can be added later if needed.
CREATE TABLE IF NOT EXISTS deleted_event_ids (
    target_event_id String,
    deleted_at DateTime
) ENGINE = ReplacingMergeTree(deleted_at)
ORDER BY (target_event_id);

CREATE MATERIALIZED VIEW IF NOT EXISTS deleted_event_ids_mv TO deleted_event_ids
AS SELECT
    tag[2] AS target_event_id,
    created_at AS deleted_at
FROM events_local
ARRAY JOIN tags AS tag
WHERE kind = 5
    AND tag[1] = 'e'
    AND length(tag) >= 2;

-- =============================================================================
-- VIDEO-SPECIFIC VIEWS (Kinds 34235, 34236)
-- =============================================================================

-- Video events view using materialized columns (no runtime tag extraction)
CREATE VIEW IF NOT EXISTS videos AS
SELECT
    id,
    pubkey,
    created_at,
    kind,
    content,
    tags,
    sig,
    indexed_at,
    d_tag,
    title,
    thumbnail,
    video_url
FROM events_local
WHERE kind IN (34235, 34236);

-- =============================================================================
-- ENGAGEMENT METRICS (Materialized Views with SummingMergeTree)
-- =============================================================================
-- NOTE: SummingMergeTree requires explicit sum() in queries for accurate counts.
-- The MV inserts count=1 for each event; merges happen asynchronously.
--
-- IMPORTANT: This pattern is NOT duplicate-safe if the same event can be inserted
-- more than once into `events_local` (e.g., due to ingest retries or multi-relay
-- duplication). If duplicate inserts are possible, consider:
-- - Enforcing at-most-once insertion upstream (durable dedupe/spool semantics), OR
-- - Redesigning derived tables/queries to be duplicate-tolerant (e.g., countDistinct
--   on event ids, or store per-event rows and aggregate).

-- Reaction counts table
CREATE TABLE IF NOT EXISTS reaction_counts (
    target_event_id String,
    reaction_count UInt64
) ENGINE = SummingMergeTree()
ORDER BY (target_event_id);

-- Reaction counts MV (kind 7 = reactions)
CREATE MATERIALIZED VIEW IF NOT EXISTS reaction_counts_mv TO reaction_counts
AS SELECT
    tag[2] AS target_event_id,
    toUInt64(1) AS reaction_count
FROM events_local
ARRAY JOIN tags AS tag
WHERE kind = 7
    AND tag[1] = 'e'
    AND length(tag) >= 2;

-- Comment counts table
CREATE TABLE IF NOT EXISTS comment_counts (
    target_event_id String,
    comment_count UInt64
) ENGINE = SummingMergeTree()
ORDER BY (target_event_id);

-- Comment counts MV (kind 1 with 'e' tag = reply)
CREATE MATERIALIZED VIEW IF NOT EXISTS comment_counts_mv TO comment_counts
AS SELECT
    tag[2] AS target_event_id,
    toUInt64(1) AS comment_count
FROM events_local
ARRAY JOIN tags AS tag
WHERE kind = 1
    AND tag[1] = 'e'
    AND length(tag) >= 2;

-- Repost counts table
CREATE TABLE IF NOT EXISTS repost_counts (
    target_event_id String,
    repost_count UInt64
) ENGINE = SummingMergeTree()
ORDER BY (target_event_id);

-- Repost counts MV (kind 6 = repost, kind 16 = generic repost)
CREATE MATERIALIZED VIEW IF NOT EXISTS repost_counts_mv TO repost_counts
AS SELECT
    tag[2] AS target_event_id,
    toUInt64(1) AS repost_count
FROM events_local
ARRAY JOIN tags AS tag
WHERE kind IN (6, 16)
    AND tag[1] = 'e'
    AND length(tag) >= 2;

-- =============================================================================
-- VIDEO ANALYTICS VIEWS
-- =============================================================================

-- Video stats aggregated view
-- IMPORTANT: Uses sum() on SummingMergeTree tables for correct counts
CREATE VIEW IF NOT EXISTS video_stats AS
SELECT
    v.id,
    v.pubkey,
    v.created_at,
    v.kind,
    v.d_tag,
    v.title,
    v.thumbnail,
    ifNull(r.reaction_count, 0) AS reactions,
    ifNull(c.comment_count, 0) AS comments,
    ifNull(rp.repost_count, 0) AS reposts,
    ifNull(r.reaction_count, 0) +
        ifNull(c.comment_count, 0) * 2 +
        ifNull(rp.repost_count, 0) * 3 AS engagement_score
FROM videos v
LEFT JOIN (
    SELECT target_event_id, sum(reaction_count) AS reaction_count
    FROM reaction_counts
    GROUP BY target_event_id
) r ON v.id = r.target_event_id
LEFT JOIN (
    SELECT target_event_id, sum(comment_count) AS comment_count
    FROM comment_counts
    GROUP BY target_event_id
) c ON v.id = c.target_event_id
LEFT JOIN (
    SELECT target_event_id, sum(repost_count) AS repost_count
    FROM repost_counts
    GROUP BY target_event_id
) rp ON v.id = rp.target_event_id;

-- Trending videos (recent videos with high engagement)
-- Uses 30-day window with time decay (newer content ranks higher)
CREATE VIEW IF NOT EXISTS trending_videos AS
SELECT
    *,
    engagement_score * exp(-dateDiff('hour', created_at, now()) / 168.0) AS trending_score
FROM video_stats
WHERE created_at > now() - INTERVAL 30 DAY
ORDER BY trending_score DESC;

-- Videos by hashtag
CREATE VIEW IF NOT EXISTS video_hashtags AS
SELECT
    t.event_id,
    t.tag_value_primary AS hashtag,
    t.created_at,
    t.pubkey,
    t.kind,
    v.title,
    v.thumbnail,
    v.d_tag
FROM event_tags_flat t
JOIN videos v ON t.event_id = v.id
WHERE t.tag_name = 't' AND t.kind IN (34235, 34236);

-- Popular hashtags
CREATE VIEW IF NOT EXISTS popular_video_hashtags AS
SELECT
    tag_value_primary AS hashtag,
    count() AS usage_count,
    uniq(pubkey) AS unique_creators,
    max(created_at) AS last_used
FROM event_tags_flat
WHERE tag_name = 't' AND kind IN (34235, 34236)
GROUP BY hashtag
ORDER BY usage_count DESC;

-- =============================================================================
-- USER ANALYTICS VIEWS
-- =============================================================================

CREATE VIEW IF NOT EXISTS daily_active_users AS
SELECT
    toDate(created_at) AS date,
    uniq(pubkey) AS active_users,
    count() AS total_events
FROM events_local
GROUP BY date
ORDER BY date DESC;

CREATE VIEW IF NOT EXISTS weekly_active_users AS
SELECT
    toMonday(created_at) AS week,
    uniq(pubkey) AS active_users,
    count() AS total_events
FROM events_local
GROUP BY week
ORDER BY week DESC;

CREATE VIEW IF NOT EXISTS monthly_active_users AS
SELECT
    toStartOfMonth(created_at) AS month,
    uniq(pubkey) AS active_users,
    count() AS total_events
FROM events_local
GROUP BY month
ORDER BY month DESC;

CREATE VIEW IF NOT EXISTS user_profiles AS
SELECT
    pubkey,
    argMax(content, created_at) AS metadata_json,
    max(created_at) AS last_updated,
    count() AS update_count
FROM events_local
WHERE kind = 0
GROUP BY pubkey;

CREATE VIEW IF NOT EXISTS top_video_creators AS
SELECT
    pubkey,
    count() AS video_count,
    countIf(kind = 34235) AS normal_videos,
    countIf(kind = 34236) AS short_videos,
    min(created_at) AS first_video,
    max(created_at) AS last_video,
    sum(engagement_score) AS total_engagement
FROM video_stats
GROUP BY pubkey
ORDER BY video_count DESC;

-- =============================================================================
-- GENERAL ANALYTICS VIEWS
-- =============================================================================

CREATE VIEW IF NOT EXISTS event_stats AS
SELECT
    toStartOfDay(created_at) AS date,
    kind,
    count() AS event_count,
    uniq(pubkey) AS unique_authors,
    avg(length(content)) AS avg_content_length
FROM events_local
GROUP BY date, kind
ORDER BY date DESC, event_count DESC;

CREATE VIEW IF NOT EXISTS tag_stats AS
SELECT
    tag_name,
    count() AS occurrence_count,
    uniq(event_id) AS unique_events,
    uniq(pubkey) AS unique_users
FROM event_tags_flat
GROUP BY tag_name
ORDER BY occurrence_count DESC;

CREATE VIEW IF NOT EXISTS activity_by_kind AS
SELECT
    toDate(created_at) AS date,
    kind,
    count() AS events,
    uniq(pubkey) AS unique_publishers
FROM events_local
GROUP BY date, kind
ORDER BY date DESC, events DESC;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT
    database,
    name AS table_name,
    engine,
    total_rows,
    formatReadableSize(total_bytes) AS size
FROM system.tables
WHERE database = 'nostr'
ORDER BY name;

-- =============================================================================
-- PROJECTION VERIFICATION (Self-hosted only)
-- =============================================================================
-- Check that projections were created successfully:

SELECT
    table,
    name AS projection_name,
    formatReadableSize(sum(data_compressed_bytes)) AS compressed_size,
    formatReadableSize(sum(data_uncompressed_bytes)) AS uncompressed_size
FROM system.projection_parts
WHERE database = 'nostr'
GROUP BY table, name
ORDER BY table, name;
