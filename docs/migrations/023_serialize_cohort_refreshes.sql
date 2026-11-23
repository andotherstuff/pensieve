-- Migration: 023_serialize_cohort_refreshes
-- Date: 2026-11-03
-- Serialize the two memory-heavy cohort refreshes, including startup catch-up.
-- Preserve the daily 03:00/03:15 UTC schedules, SELECTs and memory limits.
-- Idempotent; no table replacement or data loss. A failed weekly refresh now
-- also delays monthly refresh until weekly succeeds.
-- Fresh installations must apply this after migration 012; the base schema
-- does not create these views. Do not replay all migrations on production.
ALTER TABLE nostr.cohort_retention_monthly_mv
    MODIFY REFRESH EVERY 1 DAY OFFSET 3 HOUR 15 MINUTE
    DEPENDS ON nostr.cohort_retention_weekly_mv;
SHOW CREATE TABLE nostr.cohort_retention_monthly_mv;
SELECT view, status, last_success_time, exception
FROM system.view_refreshes WHERE database = 'nostr';
