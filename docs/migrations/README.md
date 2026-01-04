# ClickHouse Migrations

This directory contains incremental schema migrations for the Pensieve ClickHouse database.

## Running Migrations

Migrations can be run using the `just` commands:

```bash
# Run a single migration
just ch-migrate docs/migrations/001_active_user_views.sql

# Run all migrations in order
just ch-migrate-all
```

Or directly via Docker:

```bash
docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/001_active_user_views.sql
```

## Migration Index

| Migration | Description | Backfill Required |
|-----------|-------------|-------------------|
| 001_active_user_views.sql | Active user views excluding throwaway keys | No |
| 002_pubkey_first_seen.sql | Track first-seen timestamp per pubkey | **Yes** |
| 003_zap_amounts.sql | Parse bolt11 invoices from zap receipts | **Yes** |
| 004_active_users_materialized.sql | Convert active user views to MVs | **Yes** (run automatically) |
| 005_fix_active_users_views.sql | Fix active user views for large datasets | **Yes** (10-30 min) |
| 006_preaggregate_active_users.sql | Pre-aggregate for instant queries (BROKEN) | Superseded by 008 |
| 007_filter_future_dates_in_views.sql | Filter future dates from views | No |
| 008_fix_active_users_aggregation.sql | **FIX:** Proper AggregatingMergeTree for summaries | **Yes** (1-5 min) |
| 009_fix_zap_amounts.sql | Fix zap amount parsing | **Yes** |
| 010_fix_pubkey_first_seen_dates.sql | Filter invalid dates from new users view | No |
| 011_new_users_summary.sql | Pre-aggregate new users for fast queries | **Yes** (1-5 min) |
| 012_cohort_retention_summary.sql | Pre-aggregate cohort retention (auto-refreshes daily) | **Yes** (5-15 min) |

> **⚠️ IMPORTANT:** Migration 005 MUST be run if you applied migration 004. The views in 004 do
> expensive JOINs at query time that cause 100% CPU usage on large datasets (100M+ events).
> Stop all services before running migration 005.
>
> **⚠️ CRITICAL:** Migration 006 has a fundamental flaw and should NOT be used. If you ran it,
> run **migration 008** immediately to fix the data. See details below.

---

## Migration Details

### 002_pubkey_first_seen.sql

**Purpose:** Enables user retention cohort analysis and new user time series by tracking when each pubkey was first seen.

**New objects:**
- `pubkey_first_seen_data` - AggregatingMergeTree table storing min(created_at) per pubkey
- `pubkey_first_seen_mv` - Materialized view that populates data for new events
- `pubkey_first_seen` - Helper view for easy querying (finalizes the aggregate)

**Backfill command:**

```bash
just ch-query "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"
```

Or via Docker:

```bash
docker exec -i pensieve-clickhouse clickhouse-client --database nostr -q \
  "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"
```

**Note:** This backfill may take several minutes depending on the size of your events table. Progress can be monitored in the ClickHouse system.processes table.

---

### 003_zap_amounts.sql

**Purpose:** Parses bolt11 lightning invoices from zap receipt events (kind 9735) to extract payment amounts, enabling zap statistics.

**New objects:**
- `zap_amounts_data` - ReplacingMergeTree table storing parsed zap amounts
- `zap_amounts_mv` - Materialized view that parses bolt11 from new zap receipts

**Bolt11 parsing:**

The migration extracts amounts from bolt11 invoices using regex. The bolt11 format is:
- `lnbc<amount><multiplier>1<rest>`
- Multipliers: `m` (milli), `u` (micro), `n` (nano), `p` (pico)
- Example: `lnbc10u1...` = 10 micro-BTC = 1,000 sats

**Backfill command:**

```bash
docker exec -i pensieve-clickhouse clickhouse-client --database nostr -q "
INSERT INTO zap_amounts_data
SELECT
    id AS event_id,
    pubkey AS zapper_pubkey,
    created_at,
    -- Multipliers convert bolt11 amount to millisatoshis
    -- 1 BTC = 100,000,000 sats = 100,000,000,000 msats
    multiIf(
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'm',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000000,      -- milli: 10^8 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'u',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000,         -- micro: 10^5 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'n',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100,            -- nano: 10^2 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'p',
        intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')), 10),      -- pico: 0.1 msats (truncated)
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = '',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000000000,   -- BTC: 10^11 msats
        toUInt64(0)
    ) AS amount_msats,
    (arrayFilter(t -> t[1] = 'p', tags)[1])[2] AS recipient_pubkey,
    (arrayFilter(t -> t[1] = 'P', tags)[1])[2] AS sender_pubkey
FROM events_local
WHERE kind = 9735
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]'), tags)
"
```

---

### 004_active_users_materialized.sql

**Purpose:** Converts the slow active user views (which scan 800M+ events on every query) to materialized views for millisecond query times.

**Note:** This migration has a design flaw that causes timeouts on large datasets. **Run migration 005 after this one to fix it.**

**New objects:**
- `pubkeys_with_profile_data` / `pubkeys_with_profile_mv` - Tracks pubkeys with kind=0 events
- `pubkeys_with_follows_data` / `pubkeys_with_follows_mv` - Tracks pubkeys with kind=3 events
- `daily_active_users_data` / `daily_active_users_mv` - Daily aggregates
- `weekly_active_users_data` / `weekly_active_users_mv` - Weekly aggregates
- `monthly_active_users_data` / `monthly_active_users_mv` - Monthly aggregates
- `daily_pubkeys_data` / `daily_pubkeys_mv` - Daily pubkey tracking

**Backfill:** Included in the migration script (runs automatically).

---

### 005_fix_active_users_views.sql

**Purpose:** Fixes the performance issues from migration 004. The original views used FINAL on billion-row tables with JOINs, causing 100% CPU and 60+ second timeouts. This migration replaces them with a pre-aggregated approach.

**New objects:**
- `daily_user_stats` - Stores daily (date, pubkey, has_profile, has_follows, event_count)
- `daily_user_stats_mv` - Populates the stats table from new events
- Recreates `daily_active_users`, `weekly_active_users`, `monthly_active_users` views

**Key improvement:** Profile/follows flags are computed at insert time and stored, so queries just aggregate from pre-computed data (millisecond query times).

**Backfill:** Included in the migration script. Takes 10-30 minutes depending on dataset size.

**⚠️ CRITICAL: Stop all services before running this migration!**

```bash
# Step 1: Stop services that query these views
docker stop pensieve-grafana    # Grafana triggers the expensive queries
sudo systemctl stop pensieve-ingest

# Step 2: Verify ClickHouse is idle
docker exec pensieve-clickhouse clickhouse-client --query "SELECT count() FROM system.processes"
# Should return 0 or 1

# Step 3: Run the migration
docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/005_fix_active_users_views.sql

# Step 4: Verify it worked (should be instant, not 60+ seconds)
docker exec pensieve-clickhouse clickhouse-client --database nostr --query "SELECT * FROM daily_active_users LIMIT 5"

# Step 5: Restart services
sudo systemctl start pensieve-ingest
docker start pensieve-grafana
```

---

### 006_preaggregate_active_users.sql (SUPERSEDED)

**⚠️ WARNING: This migration has a fundamental design flaw and is superseded by migration 008.**

The problem: It used `ReplacingMergeTree` for the summary tables, but the MVs compute aggregates
per INSERT batch. When multiple batches are inserted for the same date, ReplacingMergeTree
**replaces** rows (keeps only the latest) instead of **combining** them. This causes massive
data loss where only one batch's worth of data survives per day.

**Symptoms:** Most days show 1-20 active users and tiny event counts, with random days showing
realistic numbers (those days happened to have only one batch inserted).

**Fix:** Run migration 008.

---

### 007_filter_future_dates_in_views.sql

**Purpose:** Filters out future-dated events (created_at in 2100, 2077, etc.) from active user views.
Some events have invalid timestamps far in the future; this migration excludes them at query time.

**Changes:** Updates the `daily_active_users`, `weekly_active_users`, and `monthly_active_users`
views to filter dates between Nostr genesis (2020-11-07) and today.

**Backfill:** None required.

---

### 009_fix_zap_amounts.sql

**Purpose:** Fixes zap amount parsing issues from migration 003.

---

### 010_fix_pubkey_first_seen_dates.sql

**Purpose:** Filters invalid dates from the `pubkey_first_seen` view, similar to the fix in
migration 007 for active user views.

**The Problem:** The `pubkey_first_seen` view was returning pubkeys with invalid `first_seen`
timestamps, including:
- Future dates (events with created_at like 2100, 2077, etc.)
- Pre-Nostr genesis dates (before 2020-11-07)

This caused the "New Users" dashboard charts and API endpoints to show incorrect data.

**The Fix:** Updates the view to filter: `first_seen >= '2020-11-07' AND first_seen <= now()`

**Backfill:** None required. The underlying data in `pubkey_first_seen_data` is unchanged;
only the view query is updated.

**To run:**

```bash
just ch-migrate docs/migrations/010_fix_pubkey_first_seen_dates.sql
```

---

### 011_new_users_summary.sql

**Purpose:** Creates a pre-aggregated summary table for new users per day, enabling fast queries
for the "new users" time series.

**The Problem:** The `pubkey_first_seen` view requires scanning and merging all rows in
`pubkey_first_seen_data` (~6M+ pubkeys) on every query, making queries take many seconds.

**The Solution:** Pre-aggregate new user counts per day into an `AggregatingMergeTree` summary table.

**New objects:**
- `daily_new_users_summary` - AggregatingMergeTree table storing uniqState(pubkey) per date
- `daily_new_users_summary_mv` - Materialized view from events_local inserts
- `daily_new_users` - Query view with date filtering and uniqMerge finalization

**Backfill:** Included in migration (1-5 minutes depending on data size).

**To run:**

```bash
just ch-migrate docs/migrations/011_new_users_summary.sql
```

---

### 012_cohort_retention_summary.sql

**Purpose:** Creates pre-aggregated summary tables for cohort retention analysis, enabling fast
queries for the `/stats/users/retention` endpoint. Uses **Refreshable Materialized Views**
(ClickHouse 23.12+) for automatic daily updates.

**The Problem:** The retention endpoint performs an extremely expensive query that:
1. Scans `pubkey_first_seen` (6M+ rows with aggregation finalization)
2. JOINs against `events_local` (800M+ rows)
3. Groups by cohort period and activity period

This query can take 60+ seconds or timeout entirely.

**The Solution:** Use Refreshable MVs to pre-aggregate cohort retention data. The MVs:
- Perform a full table replacement on each refresh (handles shifting cohort assignments)
- Run automatically on schedule (no external cron needed)
- Use `daily_user_stats` and `pubkey_first_seen` as source data

**New objects:**
- `cohort_retention_weekly` - Target table for weekly cohort retention
- `cohort_retention_weekly_mv` - Refreshable MV (daily at 03:00 UTC)
- `cohort_retention_monthly` - Target table for monthly cohort retention
- `cohort_retention_monthly_mv` - Refreshable MV (daily at 03:15 UTC)
- `cohort_retention_weekly_view` - Query view for API compatibility
- `cohort_retention_monthly_view` - Query view for API compatibility

**Dependencies:** Requires migrations 002 (pubkey_first_seen) and 005 (daily_user_stats).

**Initial refresh:** The migration triggers initial data population (5-15 minutes).

**To run:**

```bash
just ch-migrate docs/migrations/012_cohort_retention_summary.sql
```

**Automatic refresh:** Data refreshes daily at 03:00/03:15 UTC. No manual intervention needed.

**To manually trigger a refresh:**

```bash
just ch-query "SYSTEM REFRESH VIEW cohort_retention_weekly_mv"
just ch-query "SYSTEM REFRESH VIEW cohort_retention_monthly_mv"
```

**To check refresh status:**

```bash
just ch-query "SELECT view, status, last_refresh_time, next_refresh_time FROM system.view_refreshes WHERE database = 'nostr'"
```

**To modify refresh schedule:**

```bash
# Change to weekly refresh
just ch-query "ALTER TABLE cohort_retention_weekly_mv MODIFY REFRESH EVERY 1 WEEK"
```

---

### 008_fix_active_users_aggregation.sql

**Purpose:** Fixes the critical bug in migration 006 by using `AggregatingMergeTree` with proper
`*State`/`*Merge` functions instead of `ReplacingMergeTree`.

**The Problem:** Migration 006 used `ReplacingMergeTree` for summary tables. When multiple batches
are inserted for the same date, ClickHouse keeps only ONE row (the latest), losing all previous
batches' data. This caused ~99% data loss on most days.

**The Fix:** Uses `AggregatingMergeTree` with:
- `uniqState(pubkey)` / `sumState(event_count)` in the materialized views
- `uniqMerge()` / `sumMerge()` in the query views

This properly combines aggregate states across batches instead of replacing them.

**To run:**

```bash
# Step 1: Stop services
docker stop pensieve-grafana
sudo systemctl stop pensieve-ingest

# Step 2: Run the migration (drops old tables, creates new ones, backfills)
just ch-migrate docs/migrations/008_fix_active_users_aggregation.sql

# Step 3: Verify the fix worked
just ch-query "SELECT date, active_users, total_events FROM daily_active_users ORDER BY date DESC LIMIT 14"
# Should now show consistent, realistic numbers (not 1-20 users per day)

# Step 4: Restart services
sudo systemctl start pensieve-ingest
docker start pensieve-grafana
```

**Backfill:** Included in the migration script (1-5 minutes depending on dataset size).

---

## Full Deployment Checklist

When deploying new API endpoints that depend on these migrations:

### 1. Run migrations

```bash
just ch-migrate docs/migrations/002_pubkey_first_seen.sql
just ch-migrate docs/migrations/003_zap_amounts.sql
```

### 2. Backfill historical data

```bash
# Backfill pubkey first-seen (required for /stats/users/retention and /stats/users/new)
just ch-query "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"

# Backfill zap amounts (required for /stats/zaps)
# See the full command in the 003_zap_amounts.sql section above
```

### 3. Verify backfill completed

```bash
# Check pubkey_first_seen row count
just ch-query "SELECT count() FROM pubkey_first_seen"

# Check zap_amounts row count
just ch-query "SELECT count() FROM zap_amounts_data"

# Verify sample data
just ch-query "SELECT * FROM pubkey_first_seen LIMIT 5"
just ch-query "SELECT * FROM zap_amounts_data ORDER BY amount_msats DESC LIMIT 5"
```

### 4. Rebuild and deploy the API server

```bash
just build-release
# Then deploy pensieve-serve via your deployment method (systemd, docker, etc.)
```

---

## Backfill Time Estimates

For large production databases, backfills can take significant time. Here are rough estimates:

| Backfill | Events Scanned | Estimated Time (800M events) |
|----------|----------------|------------------------------|
| pubkey_first_seen | All events | 5-30 minutes |
| zap_amounts | Only kind 9735 | 30 seconds - 5 minutes |

**Factors affecting performance:**
- Disk type (SSD vs HDD)
- Available CPU cores
- RAM for aggregation buffers
- Concurrent query load

### Monitoring Backfill Progress

Run in a separate terminal to watch progress:

```bash
watch -n 5 'docker exec pensieve-clickhouse clickhouse-client --database nostr -q "
SELECT
    query_id,
    round(elapsed, 1) as elapsed_sec,
    formatReadableQuantity(read_rows) as rows_read,
    formatReadableQuantity(total_rows_approx) as total_rows,
    round(100 * read_rows / total_rows_approx, 1) as pct_complete
FROM system.processes
WHERE query LIKE '\''%INSERT INTO%'\''
FORMAT Pretty
"'
```

### Check Zap Count Before Backfill

To estimate zap backfill time, first check how many zap receipts exist:

```bash
just ch-query "SELECT count() FROM events_local WHERE kind = 9735"
```

### Batched Backfill (Optional)

For very large tables, you can batch the pubkey_first_seen backfill by date range to reduce memory pressure:

```sql
-- Example: backfill 2024 data only
INSERT INTO pubkey_first_seen_data
SELECT pubkey, minState(created_at)
FROM events_local
WHERE kind NOT IN (445, 1059)
  AND created_at >= '2024-01-01'
  AND created_at < '2025-01-01'
GROUP BY pubkey
```

---

## New API Endpoints

These migrations enable the following new endpoints:

| Endpoint | Description | Required Migration |
|----------|-------------|-------------------|
| `GET /api/v1/stats/throughput` | Events per hour time series | None |
| `GET /api/v1/stats/users/retention` | Cohort retention analysis | 002 |
| `GET /api/v1/stats/users/new` | New users per period | 002 |
| `GET /api/v1/stats/activity/hourly` | Activity by hour of day | None |
| `GET /api/v1/stats/zaps` | Zap statistics | 003 |
| `GET /api/v1/stats/zaps/histogram` | Zap amount distribution histogram | 003 |
| `GET /api/v1/stats/engagement` | Reply/reaction ratios | None |
| `GET /api/v1/stats/longform` | Long-form content stats | None |
| `GET /api/v1/stats/publishers` | Top publishers | None |

All endpoints include HTTP caching headers (`Cache-Control: public, max-age=60, stale-while-revalidate=300`).

