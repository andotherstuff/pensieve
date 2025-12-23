-- Migration: 002_pubkey_first_seen
-- Description: Track first-seen timestamp for each pubkey to enable retention cohorts
--              and new user time series. Uses AggregatingMergeTree for efficient updates.
-- Date: 2025-12-23
--
-- To run this migration:
--   just ch-migrate docs/migrations/002_pubkey_first_seen.sql
--
-- IMPORTANT: After running this migration, you must backfill historical data:
--   just ch-query "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"
--
-- Or via docker:
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/002_pubkey_first_seen.sql
--   docker exec -i pensieve-clickhouse clickhouse-client --database nostr -q "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"

-- =============================================================================
-- PUBKEY FIRST SEEN TABLE (AggregatingMergeTree)
-- =============================================================================
--
-- This table stores the minimum created_at timestamp for each pubkey.
-- We use AggregatingMergeTree with minState/minMerge to efficiently track
-- the earliest event from each pubkey, even as new events are inserted.
--
-- Excludes throwaway key kinds (445, 1059) to match active user heuristics.

CREATE TABLE IF NOT EXISTS pubkey_first_seen_data (
    pubkey String,
    first_seen_state AggregateFunction(min, DateTime)
) ENGINE = AggregatingMergeTree()
ORDER BY (pubkey);

-- Materialized view to populate first_seen for new events
CREATE MATERIALIZED VIEW IF NOT EXISTS pubkey_first_seen_mv TO pubkey_first_seen_data
AS SELECT
    pubkey,
    minState(created_at) AS first_seen_state
FROM events_local
WHERE kind NOT IN (445, 1059)
GROUP BY pubkey;

-- Helper view for easy querying (finalizes the aggregate)
CREATE VIEW IF NOT EXISTS pubkey_first_seen AS
SELECT
    pubkey,
    minMerge(first_seen_state) AS first_seen
FROM pubkey_first_seen_data
GROUP BY pubkey;

-- =============================================================================
-- VERIFICATION
-- =============================================================================

SELECT 'Migration complete. Verifying tables/views exist:' AS status;

SELECT
    name,
    engine
FROM system.tables
WHERE database = currentDatabase()
    AND name IN ('pubkey_first_seen_data', 'pubkey_first_seen_mv', 'pubkey_first_seen')
ORDER BY name;

SELECT 'REMINDER: Run backfill query to populate historical data!' AS reminder;
SELECT 'INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey' AS backfill_query;

