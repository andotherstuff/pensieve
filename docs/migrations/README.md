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
| 005_fix_active_users_views.sql | Fix active user views for large datasets | **Yes** (run automatically) |

> **Note:** If migration 004 was previously applied, run 005 to fix the performance issues.

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

**Purpose:** Fixes the performance issues from migration 004. The original views used FINAL on billion-row tables with JOINs, causing 60+ second timeouts. This migration replaces them with a pre-aggregated approach.

**New objects:**
- `daily_user_stats` - Stores daily (date, pubkey, has_profile, has_follows, event_count)
- `daily_user_stats_mv` - Populates the stats table from new events
- Recreates `daily_active_users`, `weekly_active_users`, `monthly_active_users` views

**Key improvement:** Profile/follows flags are computed at insert time and stored, so queries just aggregate from pre-computed data (millisecond query times).

**Backfill:** Included in the migration script (runs automatically).

**To apply:**

```bash
# If you already ran migration 004 and have timeout issues:
just ch-migrate docs/migrations/005_fix_active_users_views.sql

# If you're deploying fresh, run both:
just ch-migrate docs/migrations/004_active_users_materialized.sql
just ch-migrate docs/migrations/005_fix_active_users_views.sql
```

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

