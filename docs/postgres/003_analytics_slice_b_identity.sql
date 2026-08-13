-- Migration: 003_analytics_slice_b_identity
-- Description: Exact eligible-pubkey totals and daily first-seen products.
-- Date: 2026-08-13

ALTER TABLE pensieve_analytics.runs
    ADD COLUMN IF NOT EXISTS eligible_pubkeys BIGINT NOT NULL DEFAULT 0
        CHECK (eligible_pubkeys >= 0),
    ADD COLUMN IF NOT EXISTS new_users_daily_rows BIGINT NOT NULL DEFAULT 0
        CHECK (new_users_daily_rows >= 0);

ALTER TABLE pensieve_analytics.overview
    ADD COLUMN IF NOT EXISTS total_pubkeys BIGINT NOT NULL DEFAULT 0
        CHECK (total_pubkeys >= 0);

CREATE TABLE IF NOT EXISTS pensieve_analytics.new_users_daily (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    day DATE NOT NULL,
    new_pubkeys BIGINT NOT NULL CHECK (new_pubkeys >= 0),
    PRIMARY KEY (run_id, day)
);

-- PostgreSQL expands `table.*` when a view is created, so recreate the views
-- that must expose columns added above.
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

CREATE OR REPLACE VIEW pensieve_analytics.current_new_users_daily AS
SELECT new_users_daily.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.new_users_daily new_users_daily USING (run_id)
WHERE current.singleton;
