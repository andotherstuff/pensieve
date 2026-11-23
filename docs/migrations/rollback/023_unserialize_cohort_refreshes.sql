-- Restore the prior monthly schedule without the dependency; no data changes.
ALTER TABLE nostr.cohort_retention_monthly_mv
    MODIFY REFRESH EVERY 1 DAY OFFSET 3 HOUR 15 MINUTE;
