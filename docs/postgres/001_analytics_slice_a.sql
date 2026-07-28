-- Migration: 001_analytics_slice_a
-- Description: Versioned serving relations and atomic run pointer for the
--              first DuckDB/Postgres analytics slice.
-- Date: 2026-07-28
--
-- This schema is rebuildable from a selected active-file snapshot. The batch
-- builder applies this idempotently before transactional publication.

CREATE SCHEMA IF NOT EXISTS pensieve_analytics;

CREATE TABLE IF NOT EXISTS pensieve_analytics.runs (
    run_id TEXT PRIMARY KEY,
    snapshot_id TEXT NOT NULL,
    previous_run_id TEXT REFERENCES pensieve_analytics.runs (run_id),
    run_kind TEXT NOT NULL CHECK (run_kind IN (
        'full_rebuild',
        'affected_period_rebuild',
        'incremental'
    )),
    query_version TEXT NOT NULL,
    code_version TEXT NOT NULL,
    as_of_epoch BIGINT NOT NULL CHECK (
        as_of_epoch >= 0 AND as_of_epoch <= 4294967295
    ),
    started_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ NOT NULL,
    published_at TIMESTAMPTZ NOT NULL,
    physical_rows BIGINT NOT NULL CHECK (physical_rows >= 0),
    logical_events BIGINT NOT NULL CHECK (logical_events >= 0),
    duplicate_rows BIGINT NOT NULL CHECK (duplicate_rows >= 0),
    api_representable_events BIGINT NOT NULL CHECK (
        api_representable_events >= 0
    ),
    event_daily_rows BIGINT NOT NULL CHECK (event_daily_rows >= 0),
    event_daily_kind_rows BIGINT NOT NULL CHECK (
        event_daily_kind_rows >= 0
    ),
    kind_all_time_rows BIGINT NOT NULL CHECK (kind_all_time_rows >= 0),
    validation JSONB NOT NULL,
    CHECK (completed_at >= started_at),
    CHECK (physical_rows = logical_events + duplicate_rows)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.run_inputs (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    object_key TEXT NOT NULL,
    work_unit_id TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    byte_size BIGINT NOT NULL CHECK (byte_size >= 0),
    physical_rows BIGINT NOT NULL CHECK (physical_rows >= 0),
    PRIMARY KEY (run_id, object_key)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.overview (
    run_id TEXT PRIMARY KEY REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    total_events BIGINT NOT NULL CHECK (total_events >= 0),
    api_representable_events BIGINT NOT NULL CHECK (
        api_representable_events >= 0
    ),
    earliest_event BIGINT NOT NULL CHECK (
        earliest_event >= 0 AND earliest_event <= 4294967295
    ),
    latest_event BIGINT NOT NULL CHECK (
        latest_event >= 0 AND latest_event <= 4294967295
    ),
    events_7d BIGINT NOT NULL CHECK (events_7d >= 0),
    events_per_hour_7d DOUBLE PRECISION NOT NULL CHECK (
        events_per_hour_7d >= 0
    ),
    kinds_30d BIGINT NOT NULL CHECK (kinds_30d >= 0)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.event_daily (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    day DATE NOT NULL,
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    PRIMARY KEY (run_id, day)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.event_daily_kind (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    day DATE NOT NULL,
    kind INTEGER NOT NULL CHECK (kind >= 0 AND kind <= 65535),
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    PRIMARY KEY (run_id, day, kind)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.kind_all_time (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    kind INTEGER NOT NULL CHECK (kind >= 0 AND kind <= 65535),
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    PRIMARY KEY (run_id, kind)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.current_run (
    singleton BOOLEAN PRIMARY KEY CHECK (singleton),
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
);

CREATE OR REPLACE VIEW pensieve_analytics.current_run_metadata AS
SELECT run.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.runs run USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_overview AS
SELECT overview.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.overview overview USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_event_daily AS
SELECT event_daily.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.event_daily event_daily USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_event_daily_kind AS
SELECT event_daily_kind.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.event_daily_kind event_daily_kind USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_kind_all_time AS
SELECT kind_all_time.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.kind_all_time kind_all_time USING (run_id)
WHERE current.singleton;
