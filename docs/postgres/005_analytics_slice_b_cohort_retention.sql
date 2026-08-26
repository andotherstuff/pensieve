-- Migration: 005_analytics_slice_b_cohort_retention
-- Description: Exact bounded weekly and monthly cohort-retention products.
-- Date: 2026-08-26

ALTER TABLE pensieve_analytics.runs
    ADD COLUMN IF NOT EXISTS cohort_retention_rows BIGINT NOT NULL DEFAULT 0
        CHECK (cohort_retention_rows >= 0);

CREATE TABLE IF NOT EXISTS pensieve_analytics.cohort_retention_period (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    grain TEXT NOT NULL CHECK (grain IN ('week', 'month')),
    cohort_start DATE NOT NULL,
    activity_period DATE NOT NULL,
    active_pubkeys BIGINT NOT NULL CHECK (active_pubkeys > 0),
    CHECK (activity_period >= cohort_start),
    PRIMARY KEY (run_id, grain, cohort_start, activity_period)
);

CREATE INDEX IF NOT EXISTS cohort_retention_period_current_lookup
    ON pensieve_analytics.cohort_retention_period
        (run_id, grain, cohort_start DESC, activity_period ASC);

CREATE OR REPLACE VIEW pensieve_analytics.current_cohort_retention_period AS
SELECT retention.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.cohort_retention_period retention USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_run_metadata AS
SELECT run.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.runs run USING (run_id)
WHERE current.singleton;
