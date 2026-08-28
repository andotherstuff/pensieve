-- Slice 9.5: versioned exact hourly and enriched kind serving facts.
-- Idempotent and dormant until an accepted run records a product.

CREATE TABLE IF NOT EXISTS pensieve_analytics.serving_fact_products (
    product_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs(run_id) ON DELETE CASCADE,
    snapshot_id TEXT NOT NULL,
    as_of_epoch BIGINT NOT NULL CHECK (as_of_epoch >= 0),
    complete_through_epoch BIGINT NOT NULL CHECK (
        complete_through_epoch >= 0 AND complete_through_epoch <= as_of_epoch
    ),
    product_version TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL,
    activity_evidence_sha256 TEXT NOT NULL,
    enriched_artifact_sha256 TEXT NOT NULL,
    hourly_artifact_sha256 TEXT NOT NULL,
    kind_artifact_sha256 TEXT NOT NULL,
    logical_events BIGINT NOT NULL CHECK (logical_events >= 0),
    hourly_rows BIGINT NOT NULL CHECK (hourly_rows >= 0),
    kind_rows BIGINT NOT NULL CHECK (kind_rows >= 0),
    complete_hour_events BIGINT NOT NULL CHECK (complete_hour_events >= 0),
    eligible_kind_events BIGINT NOT NULL CHECK (eligible_kind_events >= 0),
    eligible_content_bytes BIGINT NOT NULL CHECK (eligible_content_bytes >= 0),
    published_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS serving_fact_products_run_idx
    ON pensieve_analytics.serving_fact_products(run_id);

CREATE TABLE IF NOT EXISTS pensieve_analytics.serving_hourly_counts (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.serving_fact_products(product_id)
        ON DELETE CASCADE,
    hour_epoch BIGINT NOT NULL CHECK (hour_epoch >= 0),
    kind INTEGER NOT NULL CHECK (kind >= -1 AND kind <= 65535),
    event_count BIGINT NOT NULL CHECK (event_count > 0),
    PRIMARY KEY (product_id, hour_epoch, kind)
);

CREATE INDEX IF NOT EXISTS serving_hourly_counts_lookup_idx
    ON pensieve_analytics.serving_hourly_counts(product_id, kind, hour_epoch);

CREATE TABLE IF NOT EXISTS pensieve_analytics.serving_kind_summaries (
    product_id TEXT NOT NULL REFERENCES pensieve_analytics.serving_fact_products(product_id)
        ON DELETE CASCADE,
    kind INTEGER NOT NULL CHECK (kind >= 0 AND kind <= 65535),
    event_count BIGINT NOT NULL CHECK (event_count > 0),
    unique_pubkeys BIGINT NOT NULL CHECK (unique_pubkeys > 0 AND unique_pubkeys <= event_count),
    first_seen BIGINT NOT NULL CHECK (first_seen >= 0),
    last_seen BIGINT NOT NULL CHECK (last_seen >= first_seen),
    content_bytes BIGINT NOT NULL CHECK (content_bytes >= 0),
    content_rows BIGINT NOT NULL CHECK (content_rows = event_count),
    PRIMARY KEY (product_id, kind)
);
