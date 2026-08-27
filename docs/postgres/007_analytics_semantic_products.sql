-- Migration: 007_analytics_semantic_products
-- Description: Versioned exact Slice 7 engagement, long-form, and zap rollups.
-- Date: 2026-08-27

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_products (
    product_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    snapshot_id TEXT NOT NULL,
    as_of_epoch BIGINT NOT NULL CHECK (
        as_of_epoch >= 0 AND as_of_epoch <= 4294967295
    ),
    product_version TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL CHECK (evidence_sha256 ~ '^[0-9a-f]{64}$'),
    fact_artifact_sha256 TEXT NOT NULL
        CHECK (fact_artifact_sha256 ~ '^[0-9a-f]{64}$'),
    rollup_sha256 TEXT NOT NULL CHECK (rollup_sha256 ~ '^[0-9a-f]{64}$'),
    logical_relevant_events BIGINT NOT NULL CHECK (logical_relevant_events >= 0),
    engagement_days BIGINT NOT NULL CHECK (engagement_days >= 0),
    longform_days BIGINT NOT NULL CHECK (longform_days >= 0),
    zap_days BIGINT NOT NULL CHECK (zap_days >= 0),
    published_at TIMESTAMPTZ NOT NULL,
    UNIQUE (run_id, product_version)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_engagement_daily (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.semantic_products (product_id)
        ON DELETE CASCADE,
    day_epoch BIGINT NOT NULL CHECK (day_epoch >= 0 AND day_epoch % 86400 = 0),
    original_notes BIGINT NOT NULL CHECK (original_notes >= 0),
    replies BIGINT NOT NULL CHECK (replies >= 0),
    reactions BIGINT NOT NULL CHECK (reactions >= 0),
    PRIMARY KEY (product_id, day_epoch)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_longform_daily (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.semantic_products (product_id)
        ON DELETE CASCADE,
    day_epoch BIGINT NOT NULL CHECK (day_epoch >= 0 AND day_epoch % 86400 = 0),
    articles BIGINT NOT NULL CHECK (articles >= 0),
    content_bytes BIGINT NOT NULL CHECK (content_bytes >= 0),
    PRIMARY KEY (product_id, day_epoch)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_zap_daily (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.semantic_products (product_id)
        ON DELETE CASCADE,
    day_epoch BIGINT NOT NULL CHECK (day_epoch >= 0 AND day_epoch % 86400 = 0),
    accepted BIGINT NOT NULL CHECK (accepted >= 0),
    amount_msats BIGINT NOT NULL CHECK (amount_msats >= 0),
    validated_sender_facts BIGINT NOT NULL CHECK (validated_sender_facts >= 0),
    validated_recipient_facts BIGINT NOT NULL CHECK (validated_recipient_facts >= 0),
    PRIMARY KEY (product_id, day_epoch)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_zap_histogram_daily (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.semantic_products (product_id)
        ON DELETE CASCADE,
    day_epoch BIGINT NOT NULL CHECK (day_epoch >= 0 AND day_epoch % 86400 = 0),
    bucket SMALLINT NOT NULL CHECK (bucket >= 0 AND bucket <= 16),
    zap_count BIGINT NOT NULL CHECK (zap_count >= 0),
    amount_msats BIGINT NOT NULL CHECK (amount_msats >= 0),
    PRIMARY KEY (product_id, day_epoch, bucket)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_zap_rejections_daily (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.semantic_products (product_id)
        ON DELETE CASCADE,
    day_epoch BIGINT NOT NULL CHECK (day_epoch >= 0 AND day_epoch % 86400 = 0),
    reason SMALLINT NOT NULL CHECK (reason >= 0 AND reason <= 5),
    rejected_count BIGINT NOT NULL CHECK (rejected_count >= 0),
    PRIMARY KEY (product_id, day_epoch, reason)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_zap_distinct_products (
    product_id TEXT PRIMARY KEY,
    semantic_product_id TEXT NOT NULL
        REFERENCES pensieve_analytics.semantic_products (product_id)
        ON DELETE CASCADE,
    complete_through_epoch BIGINT NOT NULL CHECK (
        complete_through_epoch >= 0 AND complete_through_epoch % 86400 = 0
    ),
    product_version TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL CHECK (evidence_sha256 ~ '^[0-9a-f]{64}$'),
    identity_artifact_sha256 TEXT NOT NULL
        CHECK (identity_artifact_sha256 ~ '^[0-9a-f]{64}$'),
    physical_identities BIGINT NOT NULL CHECK (physical_identities >= 0),
    logical_identities BIGINT NOT NULL CHECK (logical_identities >= 0),
    duplicate_identities BIGINT NOT NULL CHECK (duplicate_identities >= 0),
    leaf_rows BIGINT NOT NULL CHECK (leaf_rows >= 0),
    sketch_bytes BIGINT NOT NULL CHECK (sketch_bytes >= 0),
    max_leaf_bytes BIGINT NOT NULL CHECK (max_leaf_bytes >= 0),
    published_at TIMESTAMPTZ NOT NULL,
    UNIQUE (semantic_product_id, product_version)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.semantic_zap_distinct_leaves (
    product_id TEXT NOT NULL
        REFERENCES pensieve_analytics.semantic_zap_distinct_products (product_id)
        ON DELETE CASCADE,
    day_epoch BIGINT NOT NULL CHECK (day_epoch >= 0 AND day_epoch % 86400 = 0),
    role SMALLINT NOT NULL CHECK (role IN (0, 1)),
    exact_identities BIGINT NOT NULL CHECK (exact_identities >= 0),
    estimated_identities BIGINT NOT NULL CHECK (estimated_identities >= 0),
    relative_error_ppm BIGINT NOT NULL CHECK (
        relative_error_ppm >= 0 AND relative_error_ppm <= 20000
    ),
    sketch BYTEA NOT NULL,
    PRIMARY KEY (product_id, day_epoch, role)
);

CREATE INDEX IF NOT EXISTS semantic_engagement_window
    ON pensieve_analytics.semantic_engagement_daily (product_id, day_epoch);
CREATE INDEX IF NOT EXISTS semantic_longform_window
    ON pensieve_analytics.semantic_longform_daily (product_id, day_epoch);
CREATE INDEX IF NOT EXISTS semantic_zap_window
    ON pensieve_analytics.semantic_zap_daily (product_id, day_epoch);
CREATE INDEX IF NOT EXISTS semantic_zap_distinct_window
    ON pensieve_analytics.semantic_zap_distinct_leaves
        (product_id, role, day_epoch);

-- Deliberately no current-product view or pointer. Slice 7 publication stays
-- dormant until the combined API serving gate succeeds.
