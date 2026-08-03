-- Migration: 002_analytics_applied_objects
-- Description: Current applied-object ledger for incremental analytics plans.
-- Date: 2026-08-03
--
-- Historical snapshot membership remains in run_inputs. This table is the
-- compact current-state ledger updated in the same transaction as publication.

CREATE TABLE IF NOT EXISTS pensieve_analytics.applied_objects (
    object_key TEXT PRIMARY KEY,
    work_unit_id TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    byte_size BIGINT NOT NULL CHECK (byte_size >= 0),
    physical_rows BIGINT NOT NULL CHECK (physical_rows >= 0),
    min_created_at TEXT CHECK (
        min_created_at IS NULL OR min_created_at ~ '^(0|[1-9][0-9]*)$'
    ),
    max_created_at TEXT CHECK (
        max_created_at IS NULL OR max_created_at ~ '^(0|[1-9][0-9]*)$'
    ),
    first_applied_run_id TEXT NOT NULL
        REFERENCES pensieve_analytics.runs (run_id),
    last_applied_run_id TEXT NOT NULL
        REFERENCES pensieve_analytics.runs (run_id),
    active BOOLEAN NOT NULL DEFAULT true,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (
        (min_created_at IS NULL AND max_created_at IS NULL)
        OR (min_created_at IS NOT NULL AND max_created_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS applied_objects_active_idx
    ON pensieve_analytics.applied_objects (active, object_key);
