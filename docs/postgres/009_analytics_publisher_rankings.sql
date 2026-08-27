-- Migration: 009_analytics_publisher_rankings
-- Description: Versioned exact predefined-window Slice 9 publisher rankings.
-- Date: 2026-08-28

CREATE TABLE IF NOT EXISTS pensieve_analytics.publisher_ranking_products (
    product_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    snapshot_id TEXT NOT NULL,
    as_of_epoch BIGINT NOT NULL CHECK (
        as_of_epoch >= 0 AND as_of_epoch <= 4294967295
    ),
    product_version TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL CHECK (evidence_sha256 ~ '^[0-9a-f]{64}$'),
    activity_evidence_sha256 TEXT NOT NULL CHECK (
        activity_evidence_sha256 ~ '^[0-9a-f]{64}$'
    ),
    activity_artifact_sha256 TEXT NOT NULL CHECK (
        activity_artifact_sha256 ~ '^[0-9a-f]{64}$'
    ),
    ranking_artifact_sha256 TEXT NOT NULL CHECK (
        ranking_artifact_sha256 ~ '^[0-9a-f]{64}$'
    ),
    windows_days INTEGER[] NOT NULL,
    top_limit INTEGER NOT NULL CHECK (top_limit BETWEEN 1 AND 1000),
    source_records BIGINT NOT NULL CHECK (source_records >= 0),
    ledger_rows BIGINT NOT NULL CHECK (ledger_rows >= 0),
    ranking_groups BIGINT NOT NULL CHECK (ranking_groups >= 0),
    ranking_rows BIGINT NOT NULL CHECK (ranking_rows >= 0),
    published_at TIMESTAMPTZ NOT NULL,
    UNIQUE (run_id, product_version)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.publisher_ranking_rows (
    product_id TEXT NOT NULL
        REFERENCES pensieve_analytics.publisher_ranking_products (product_id)
        ON DELETE CASCADE,
    days INTEGER NOT NULL CHECK (days > 0),
    -- -1 is the canonical all-kind sentinel; Nostr kinds are 0..65535.
    kind INTEGER NOT NULL CHECK (kind BETWEEN -1 AND 65535),
    pubkey BYTEA NOT NULL CHECK (octet_length(pubkey) = 32),
    event_count BIGINT NOT NULL CHECK (event_count > 0),
    kinds_count BIGINT NOT NULL CHECK (kinds_count > 0),
    first_event BIGINT NOT NULL CHECK (first_event BETWEEN 0 AND 4294967295),
    last_event BIGINT NOT NULL CHECK (
        last_event BETWEEN first_event AND 4294967295
    ),
    PRIMARY KEY (product_id, days, kind, pubkey)
);

CREATE INDEX IF NOT EXISTS publisher_ranking_serving_order
    ON pensieve_analytics.publisher_ranking_rows
        (product_id, days, kind, event_count DESC, pubkey ASC);

-- No current-product view is exposed until the production comparison and
-- request-latency gate passes.
