-- Migration: 006_analytics_flexible_distinct
-- Description: Versioned complete-hour HLL leaves tied to an exact analytics run.
-- Date: 2026-08-27

CREATE TABLE IF NOT EXISTS pensieve_analytics.flexible_distinct_products (
    product_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    snapshot_id TEXT NOT NULL,
    as_of_epoch BIGINT NOT NULL CHECK (
        as_of_epoch >= 0 AND as_of_epoch <= 4294967295
    ),
    complete_through_epoch BIGINT NOT NULL CHECK (
        complete_through_epoch >= 0
        AND complete_through_epoch <= as_of_epoch
        AND complete_through_epoch % 3600 = 0
    ),
    product_version TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL CHECK (evidence_sha256 ~ '^[0-9a-f]{64}$'),
    validation_evidence_sha256 TEXT NOT NULL
        CHECK (validation_evidence_sha256 ~ '^[0-9a-f]{64}$'),
    leaf_artifact_sha256 TEXT NOT NULL
        CHECK (leaf_artifact_sha256 ~ '^[0-9a-f]{64}$'),
    leaf_rows BIGINT NOT NULL CHECK (leaf_rows >= 0),
    sketch_bytes BIGINT NOT NULL CHECK (sketch_bytes >= 0),
    max_leaf_bytes BIGINT NOT NULL CHECK (max_leaf_bytes >= 0),
    published_at TIMESTAMPTZ NOT NULL,
    UNIQUE (run_id, product_version)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.flexible_distinct_leaves (
    product_id TEXT NOT NULL
        REFERENCES pensieve_analytics.flexible_distinct_products (product_id)
        ON DELETE CASCADE,
    hour_epoch BIGINT NOT NULL CHECK (
        hour_epoch >= 0 AND hour_epoch <= 4294967295
    ),
    kind INTEGER NOT NULL CHECK (kind >= 0 AND kind <= 65535),
    sketch BYTEA NOT NULL CHECK (octet_length(sketch) > 0),
    PRIMARY KEY (product_id, hour_epoch, kind)
);

CREATE INDEX IF NOT EXISTS flexible_distinct_leaves_window_lookup
    ON pensieve_analytics.flexible_distinct_leaves
        (product_id, hour_epoch, kind);

-- Deliberately no current-product view or pointer. Slice 6 remains dormant
-- until the separate API publication/cutover gate succeeds.
