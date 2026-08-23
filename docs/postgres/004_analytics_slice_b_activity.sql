-- Migration: 004_analytics_slice_b_activity
-- Description: Exact fixed-grain distinct and active-user serving products.
-- Date: 2026-08-23

ALTER TABLE pensieve_analytics.runs
    ADD COLUMN IF NOT EXISTS distinct_pubkeys_period_rows BIGINT NOT NULL DEFAULT 0
        CHECK (distinct_pubkeys_period_rows >= 0),
    ADD COLUMN IF NOT EXISTS active_users_period_rows BIGINT NOT NULL DEFAULT 0
        CHECK (active_users_period_rows >= 0);

CREATE TABLE IF NOT EXISTS pensieve_analytics.distinct_pubkeys_period (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    grain TEXT NOT NULL CHECK (grain IN ('day', 'week', 'month')),
    period_start DATE NOT NULL,
    -- -1 is the stable all-kinds key; non-negative values are Nostr kinds.
    kind_key INTEGER NOT NULL CHECK (kind_key >= -1 AND kind_key <= 65535),
    unique_pubkeys BIGINT NOT NULL CHECK (unique_pubkeys >= 0),
    PRIMARY KEY (run_id, grain, period_start, kind_key)
);

CREATE TABLE IF NOT EXISTS pensieve_analytics.active_users_period (
    run_id TEXT NOT NULL REFERENCES pensieve_analytics.runs (run_id)
        ON DELETE CASCADE,
    grain TEXT NOT NULL CHECK (grain IN ('day', 'week', 'month')),
    period_start DATE NOT NULL,
    active_users BIGINT NOT NULL CHECK (active_users >= 0),
    has_profile BIGINT NOT NULL CHECK (has_profile >= 0),
    has_follows_list BIGINT NOT NULL CHECK (has_follows_list >= 0),
    has_profile_and_follows_list BIGINT NOT NULL
        CHECK (has_profile_and_follows_list >= 0),
    total_events BIGINT NOT NULL CHECK (total_events >= 0),
    CHECK (has_profile <= active_users),
    CHECK (has_follows_list <= active_users),
    CHECK (has_profile_and_follows_list <= has_profile),
    CHECK (has_profile_and_follows_list <= has_follows_list),
    PRIMARY KEY (run_id, grain, period_start)
);

CREATE OR REPLACE VIEW pensieve_analytics.current_distinct_pubkeys_period AS
SELECT distincts.run_id,
       distincts.grain,
       distincts.period_start,
       NULLIF(distincts.kind_key, -1) AS kind,
       distincts.unique_pubkeys
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.distinct_pubkeys_period distincts USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_active_users_period AS
SELECT active.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.active_users_period active USING (run_id)
WHERE current.singleton;

CREATE OR REPLACE VIEW pensieve_analytics.current_run_metadata AS
SELECT run.*
FROM pensieve_analytics.current_run current
JOIN pensieve_analytics.runs run USING (run_id)
WHERE current.singleton;
