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
    multiIf(
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'm',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000000000,
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'u',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000000,
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'n',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000,
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = 'p',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100,
        extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][2] = '',
        toUInt64OrZero(extractAll(tag[2], '^lnbc([0-9]+)([munp]?)1')[1][1]) * 100000000000000,
        0
    ) AS amount_msats,
    arrayElement(arrayFilter(t -> t[1] = 'p', tags), 1)[2] AS recipient_pubkey,
    arrayElement(arrayFilter(t -> t[1] = 'P', tags), 1)[2] AS sender_pubkey
FROM events_local
ARRAY JOIN tags AS tag
WHERE kind = 9735
    AND tag[1] = 'bolt11'
    AND length(tag) >= 2
    AND match(tag[2], '^lnbc[0-9]')
"
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

## New API Endpoints

These migrations enable the following new endpoints:

| Endpoint | Description | Required Migration |
|----------|-------------|-------------------|
| `GET /api/v1/stats/throughput` | Events per hour time series | None |
| `GET /api/v1/stats/users/retention` | Cohort retention analysis | 002 |
| `GET /api/v1/stats/users/new` | New users per period | 002 |
| `GET /api/v1/stats/activity/hourly` | Activity by hour of day | None |
| `GET /api/v1/stats/zaps` | Zap statistics | 003 |
| `GET /api/v1/stats/engagement` | Reply/reaction ratios | None |
| `GET /api/v1/stats/longform` | Long-form content stats | None |
| `GET /api/v1/stats/publishers` | Top publishers | None |

All endpoints include HTTP caching headers (`Cache-Control: public, max-age=60, stale-while-revalidate=300`).

