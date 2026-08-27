-- Migration: 008_analytics_relay_distribution
-- Description: Dormant versioned Slice 8 current NIP-65 relay distribution.
-- Date: 2026-08-28

CREATE TABLE IF NOT EXISTS pensieve_analytics.relay_distribution_products (
    product_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    snapshot_id TEXT NOT NULL,
    as_of_epoch BIGINT NOT NULL CHECK (
        as_of_epoch >= 0 AND as_of_epoch <= 4294967295
    ),
    product_version TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL CHECK (evidence_sha256 ~ '^[0-9a-f]{64}$'),
    rows_sha256 TEXT NOT NULL CHECK (rows_sha256 ~ '^[0-9a-f]{64}$'),
    candidate_events BIGINT NOT NULL CHECK (candidate_events >= 0),
    winning_pubkeys BIGINT NOT NULL CHECK (winning_pubkeys >= 0),
    minimum_users BIGINT NOT NULL CHECK (minimum_users > 0),
    relay_rows BIGINT NOT NULL CHECK (relay_rows >= 0),
    published_at TIMESTAMPTZ NOT NULL,
    UNIQUE (run_id, product_version)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.relay_distribution_rows (
    product_id TEXT NOT NULL
        REFERENCES pensieve_analytics.relay_distribution_products (product_id)
        ON DELETE CASCADE,
    relay_url TEXT NOT NULL,
    user_count BIGINT NOT NULL CHECK (user_count > 0),
    read_count BIGINT NOT NULL CHECK (read_count >= 0 AND read_count <= user_count),
    write_count BIGINT NOT NULL CHECK (write_count >= 0 AND write_count <= user_count),
    PRIMARY KEY (product_id, relay_url)
);

CREATE INDEX IF NOT EXISTS relay_distribution_serving_order
    ON pensieve_analytics.relay_distribution_rows
        (product_id, user_count DESC, relay_url ASC);

-- Deliberately no current-product view or pointer. The Slice 8 comparison
-- canary must pass before the API is authorized to select this product.
