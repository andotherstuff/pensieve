# Pensieve API

HTTP API for querying aggregate Nostr data from the Pensieve event store.

## Authentication

All `/api/v1/*` endpoints require Bearer token authentication.

```bash
curl -H "Authorization: Bearer <your-token>" https://api.example.com/api/v1/stats
```

Tokens are configured via the `PENSIEVE_API_TOKENS` environment variable (comma-separated list).

Generate a token: `openssl rand -hex 32`

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `PENSIEVE_BIND_ADDR` | `0.0.0.0:8080` | Server bind address |
| `CLICKHOUSE_URL` | `http://localhost:8123` | ClickHouse connection URL |
| `CLICKHOUSE_DATABASE` | `nostr` | ClickHouse database name |
| `PENSIEVE_LATEST_EVENT_WATERMARK_PATH` | *(optional)* | Canonical ingestion-owned watermark file; when set, `latest_event` (in `/stats` and `/stats/events/latest`) is read from it instead of scanning `events_local` |
| `PENSIEVE_LATEST_EVENT_WATERMARK_MAX_AGE_SECS` | `900` | Maximum accepted watermark publication age |
| `PENSIEVE_API_TOKENS` | *(required)* | Comma-separated API tokens |
| `RELAY_DB_PATH` | *(optional)* | Path to the ingester's SQLite `relay-stats.db` for the relay endpoints, read through a fresh read-only connection per request (see [Relays](#relays)); ignored if the file does not exist at startup |
| `RUST_LOG` | `info` | Log filter (`tracing` `EnvFilter` syntax) |

On startup the server loads a `.env` file if one exists. Override its location with
`--dotenv <path>` or the `DOTENV_PATH` environment variable (default `.env`).
Variables already set in the process environment take precedence over the file.

Every analytics route is served from ClickHouse. The three operational relay
routes use `RELAY_DB_PATH`.

---

## Caching

Every successful `/api/v1/*` response carries an `ETag` (a hash of the body)
and a per-endpoint `Cache-Control`. A request whose `If-None-Match` matches the
current `ETag` gets `304 Not Modified` with the same `Cache-Control`. Error
responses (`4xx`, `5xx`, including `503 metric_unavailable`) carry neither.

| Endpoint | `Cache-Control` |
|----------|-----------------|
| `/api/v1/stats/events/latest` | `private, max-age=10, stale-while-revalidate=30` |
| `/api/v1/stats/events/total`, `/api/v1/stats/pubkeys/total` | `private, max-age=300, stale-while-revalidate=600` |
| `/api/v1/stats/activity/hourly`, `/api/v1/stats/engagement`, `/api/v1/kinds/{kind}/activity` | `private, max-age=600, stale-while-revalidate=1800` |
| `/api/v1/stats/kinds/total`, `/api/v1/stats/events/earliest` | `private, max-age=3600, stale-while-revalidate=7200` |
| Every other `/api/v1/*` endpoint | `private, max-age=60, stale-while-revalidate=300` |

The directive is `private` because every `/api/v1/*` response requires a
Bearer token: a browser or other client cache may keep it, but a shared cache
(CDN, reverse proxy) must not store it and serve it to a client without a
token. `/health` and `/docs` need no token and send no `Cache-Control`.

Releases up to and including `api-fe54f04` sent
`public, max-age=60, stale-while-revalidate=300` for every endpoint: the policy
was matched against the full `/api/v1/...` path, but the middleware runs inside
the `/api/v1` nest and only saw the path below it.

`max-age` is not the worst-case age of the data. The server also keeps its own
in-memory cache of ClickHouse results with per-endpoint TTLs, and while one
refresh runs it may serve an expired entry for up to one more TTL (never for
the latest-event watermark). A client cache can then keep the response for
`max-age` and serve it stale for `stale-while-revalidate`. The worst case is
therefore about 2 × the server TTL + `max-age` + `stale-while-revalidate`:
about 50 s for `/stats/events/latest` (10 s server TTL, never stale), 25
minutes for `/stats/events/total` (5 minutes server TTL) and 5 hours for
`/stats/kinds/total` (1 hour server TTL). The server TTLs are in
`crates/pensieve-serve/src/cache.rs` and the handlers. `private` keeps shared
caches (and their own revalidation windows) out of this chain.

---

## Endpoints

### Health

#### `GET /health`

Health check endpoint. No authentication required.

**Response**

```json
{
  "status": "ok",
  "version": "0.1.0"
}
```

---

#### `GET /docs`

API documentation (this file). No authentication required.

Returns the API documentation as Markdown (`text/markdown`).

---

#### `GET /api/v1/ping`

Authenticated ping. Use to verify your token is valid.

**Response**

```json
{
  "message": "pong"
}
```

---

### Stats Overview

#### `GET /api/v1/stats`

High-level overview of the event store. Combines multiple metrics in a single call.

**Response**

```json
{
  "total_events": 790715576,
  "total_pubkeys": 56499585,
  "total_kinds": 1020,
  "earliest_event": 1606719600,
  "latest_event": 1735689000
}
```

| Field | Description |
|-------|-------------|
| `total_events` | Total events in the store |
| `total_pubkeys` | Unique pubkeys ever seen |
| `total_kinds` | Distinct event kinds (last 30 days) |
| `earliest_event` | Timestamp of earliest event (Unix seconds) |
| `latest_event` | Timestamp of latest event (Unix seconds) |

---

### Granular Stats

These endpoints return individual metrics for efficient dashboard caching.

#### `GET /api/v1/stats/events/total`

Returns approximate total event count.

**Response**

```json
{
  "count": 790715576
}
```

**Cache-Control:** `max-age=300` (5 minutes), `stale-while-revalidate=600`

---

#### `GET /api/v1/stats/pubkeys/total`

Returns total unique pubkeys.

**Response**

```json
{
  "count": 56499585
}
```

**Cache-Control:** `max-age=300` (5 minutes), `stale-while-revalidate=600`

---

#### `GET /api/v1/stats/kinds/total`

Returns distinct event kinds with at least one event created in the last 30
days. Read from the `kind_totals` rollup (migration
`020_incremental_derived_views`); `503 metric_unavailable` without it.

**Response**

```json
{
  "count": 1020
}
```

**Cache-Control:** `max-age=3600` (1 hour), `stale-while-revalidate=7200`

---

#### `GET /api/v1/stats/events/earliest`

Returns earliest event timestamp (Nostr genesis: Nov 7, 2020, at the
earliest). Read from the `kind_totals` rollup (migration 020), like
`total_kinds` and `earliest_event` in `GET /api/v1/stats`.

**Response**

```json
{
  "timestamp": 1606719600
}
```

**Cache-Control:** `max-age=3600` (1 hour), `stale-while-revalidate=7200`

---

#### `GET /api/v1/stats/events/latest`

Returns latest event timestamp (excludes future timestamps).

**Response**

```json
{
  "timestamp": 1735689000
}
```

**Cache-Control:** `max-age=10`, `stale-while-revalidate=30`

---

### Event Stats

#### `GET /api/v1/stats/events`

Event counts with optional filters and grouping.

**Query Parameters**

| Parameter | Type | Description |
|-----------|------|-------------|
| `kind` | integer | Filter by event kind |
| `since` | date | Events created on or after (YYYY-MM-DD) |
| `until` | date | Events created before (YYYY-MM-DD) |
| `days` | integer | Shorthand: events from last N days (overrides `since`/`until`). Defaults to 30 when none of `days`, `since`, or `until` is given |
| `group_by` | string | Group results: `day`, `week`, or `month` |
| `limit` | integer | Max results (default: 100, max: 1000) |

**Response (no grouping)**

```json
{
  "count": 5432100,
  "unique_pubkeys": 12345
}
```

**Response (with `group_by`)**

```json
[
  {
    "period": "2024-12-22",
    "count": 54321,
    "unique_pubkeys": 1234
  },
  {
    "period": "2024-12-21",
    "count": 52000,
    "unique_pubkeys": 1200
  }
]
```

**Examples**

```bash
# Kind 1 (text note) events in the last 30 days (default window)
GET /api/v1/stats/events?kind=1

# Events in the last 7 days
GET /api/v1/stats/events?days=7

# Daily event counts for kind 1 in the last 30 days
GET /api/v1/stats/events?kind=1&days=30&group_by=day

# Events between dates
GET /api/v1/stats/events?since=2024-01-01&until=2024-02-01
```

---

#### `GET /api/v1/stats/throughput`

7-day rolling average of events per hour.

**Query Parameters**

| Parameter | Type | Description |
|-----------|------|-------------|
| `kind` | integer | Filter by event kind |

**Response**

```json
{
  "events_per_hour": 42500.5,
  "total_events_7d": 4998000
}
```

---

### Active Users

Active user metrics exclude throwaway keys: kind 1059 (NIP-59 gift wraps) and kind 445 (Marmot/MLS group messages), both signed by ephemeral keys.

#### `GET /api/v1/stats/users/active`

Current DAU/WAU/MAU summary.

**Response**

```json
{
  "daily": {
    "active_users": 45000,
    "has_profile": 32000,
    "has_follows_list": 28000,
    "has_profile_and_follows_list": 25000,
    "total_events": 150000
  },
  "weekly": {
    "active_users": 120000,
    "has_profile": 85000,
    "has_follows_list": 72000,
    "has_profile_and_follows_list": 65000,
    "total_events": 980000
  },
  "monthly": {
    "active_users": 350000,
    "has_profile": 240000,
    "has_follows_list": 200000,
    "has_profile_and_follows_list": 180000,
    "total_events": 4500000
  }
}
```

**Field Descriptions**

| Field | Description |
|-------|-------------|
| `active_users` | Unique pubkeys with activity in the period |
| `has_profile` | Users who had published a kind 0 (profile metadata) before the activity was ingested |
| `has_follows_list` | Users who had published a kind 3 (follow list) before the activity was ingested |
| `has_profile_and_follows_list` | Users with both profile and follows (strongest signal of established user) |
| `total_events` | Total events in the period |

The profile and follows flags are evaluated when the activity is ingested: a
user who publishes a profile later is not counted retroactively for earlier
days. Since migration 020's cutover the flags are joined from `pubkey_flags`
at refresh time instead of being looked up in every ingest insert, and a
user flagged on several events of one day is no longer dropped from
`has_profile` / `has_follows_list` / `has_profile_and_follows_list` (013's
`= 1` test missed rows whose summed flags exceeded 1), so those three fields
are higher than before the cutover. `active_users` and `total_events` are
unchanged. See [`docs/api_rollups.md`](../../docs/api_rollups.md#migration-020-incremental-derived-views).

---

#### `GET /api/v1/stats/users/active/daily`

Daily active users time series.

**Query Parameters**

| Parameter | Type | Default | Max | Description |
|-----------|------|---------|-----|-------------|
| `limit` | integer | 30 | 365 | Number of days to return |
| `since` | date | - | - | Only include data from this date onwards (YYYY-MM-DD) |

**Response**

```json
[
  {
    "period": "2024-12-22",
    "active_users": 45000,
    "has_profile": 32000,
    "has_follows_list": 28000,
    "has_profile_and_follows_list": 25000,
    "total_events": 150000
  }
]
```

---

#### `GET /api/v1/stats/users/active/weekly`

Weekly active users time series.

**Query Parameters**

| Parameter | Type | Default | Max | Description |
|-----------|------|---------|-----|-------------|
| `limit` | integer | 12 | 52 | Number of weeks to return |
| `since` | date | - | - | Only include data from this date onwards (YYYY-MM-DD) |

**Response**

Same structure as daily, with `period` representing the Monday of each week.

---

#### `GET /api/v1/stats/users/active/monthly`

Monthly active users time series.

**Query Parameters**

| Parameter | Type | Default | Max | Description |
|-----------|------|---------|-----|-------------|
| `limit` | integer | 12 | 120 | Number of months to return |
| `since` | date | - | - | Only include data from this date onwards (YYYY-MM-DD) |

**Response**

Same structure as daily, with `period` representing the first of each month.

**Examples**

```bash
# Daily active users for the last 7 days
GET /api/v1/stats/users/active/daily?limit=7

# Weekly active users since January 2024
GET /api/v1/stats/users/active/weekly?since=2024-01-01

# Monthly active users for all of 2024
GET /api/v1/stats/users/active/monthly?since=2024-01-01&limit=12
```

---

### User Analytics

#### `GET /api/v1/stats/users/new`

New users (first seen) per period.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `group_by` | string | `day` | Group by: `day`, `week`, or `month` |
| `limit` | integer | 30 | Max results (max: 365 for day, 52 for week, 120 for month) |
| `since` | date | - | Only include data from this date onwards |

**Response**

```json
[
  {
    "period": "2024-12-22",
    "new_users": 1234
  },
  {
    "period": "2024-12-21",
    "new_users": 1156
  }
]
```

---

#### `GET /api/v1/stats/users/retention`

Cohort retention analysis showing how users return over time.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `cohort_start` | date | `limit` × `cohort_size` ago | Start date for first cohort |
| `cohort_size` | string | `week` | Cohort size: `week` or `month` |
| `limit` | integer | 12 | Number of cohorts (max: 52) |

**Response**

```json
[
  {
    "cohort": "2024-12-02",
    "cohort_size": 5000,
    "retention": [5000, 2500, 1800, 1200],
    "retention_pct": [100.0, 50.0, 36.0, 24.0]
  },
  {
    "cohort": "2024-11-25",
    "cohort_size": 4800,
    "retention": [4800, 2400, 1700, 1150, 900],
    "retention_pct": [100.0, 50.0, 35.4, 24.0, 18.8]
  }
]
```

| Field | Description |
|-------|-------------|
| `cohort` | Start date of the cohort |
| `cohort_size` | Users who first appeared in this cohort |
| `retention` | Active user counts in each subsequent period |
| `retention_pct` | Retention as percentages (0-100) |

---

### Activity Patterns

#### `GET /api/v1/stats/activity/hourly`

Event activity grouped by hour of day (0-23 UTC).

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `days` | integer | 7 | Days to aggregate (max: 90) |
| `kind` | integer | - | Filter by event kind |

**Response**

```json
[
  {
    "hour": 0,
    "event_count": 125000,
    "unique_pubkeys": 8500,
    "avg_per_day": 17857.14
  },
  {
    "hour": 1,
    "event_count": 98000,
    "unique_pubkeys": 7200,
    "avg_per_day": 14000.0
  }
]
```

---

### Zaps

#### `GET /api/v1/stats/zaps`

Zap statistics (requires migration 003_zap_amounts).

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `days` | integer | 30 | Days to include |
| `group_by` | string | - | Group by: `day`, `week`, or `month`. If omitted, returns aggregate. |
| `limit` | integer | 30 | Max periods (max: 365) |

**Response (aggregate)**

```json
{
  "total_zaps": 125000,
  "total_sats": 45000000,
  "unique_senders": 8500,
  "unique_recipients": 12000,
  "avg_zap_sats": 360.0
}
```

**Response (with `group_by`)**

```json
[
  {
    "period": "2024-12-22",
    "total_zaps": 4500,
    "total_sats": 1500000,
    "unique_senders": 1200,
    "unique_recipients": 1800,
    "avg_zap_sats": 333.33
  }
]
```

---

#### `GET /api/v1/stats/zaps/histogram`

Zap amount distribution across meaningful buckets.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `days` | integer | 30 | Days to include |

**Response**

```json
[
  {
    "bucket": "1-10 sats",
    "min_sats": 1,
    "max_sats": 10,
    "count": 15000,
    "total_sats": 75000,
    "pct_count": 12.0,
    "pct_sats": 0.17
  },
  {
    "bucket": "11-21 sats",
    "min_sats": 11,
    "max_sats": 21,
    "count": 25000,
    "total_sats": 400000,
    "pct_count": 20.0,
    "pct_sats": 0.89
  }
]
```

**Buckets (17 total):**
- 1-10, 11-21 sats (micro zaps)
- 22-50, 51-100 sats (small tips)
- 101-250, 251-500 sats (medium)
- 501-750, 751-1K sats (larger tips)
- 1K-2.5K, 2.5K-5K sats (generous)
- 5K-7.5K, 7.5K-10K sats (big)
- 10K-25K, 25K-50K sats (very large)
- 50K-75K, 75K-100K sats (whale)
- 100K+ sats (mega zaps)

---

### Engagement

#### `GET /api/v1/stats/engagement`

Reply and reaction ratios relative to original notes.

Currently disabled: this endpoint always returns `503` with
`{"error": "metric_unavailable"}`, because computing it on ClickHouse needs an
unbounded raw tag scan. The parameters and response shape below describe the
contract it will serve once a bounded source exists.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `days` | integer | 30 | Days to include |

**Response**

```json
{
  "period_days": 30,
  "original_notes": 2500000,
  "replies": 750000,
  "reactions": 3200000,
  "replies_per_note": 0.3,
  "reactions_per_note": 1.28
}
```

| Field | Description |
|-------|-------------|
| `original_notes` | Kind 1 events that are NOT replies |
| `replies` | Kind 1 events with an e-tag (references another event) |
| `reactions` | Kind 7 events |
| `replies_per_note` | Average replies per original note |
| `reactions_per_note` | Average reactions per original note |

---

### Long-form Content

#### `GET /api/v1/stats/longform`

Statistics for long-form content (kind 30023). Each article, identified by
pubkey and `d` tag, counts **once, at its latest version** (newest
`created_at`, lowest event id on ties); edits are not counted separately.

This reads the `longform_latest` table from prepared migration
`019_api_publishers_longform` (see [`docs/api_rollups.md`](../../docs/api_rollups.md)).
Without that table the endpoint returns `503` with
`{"error": "metric_unavailable"}`.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `days` | integer | - | Only articles whose latest version was created in the last `days` days (omit for all time) |

**Response**

```json
{
  "articles_count": 125000,
  "unique_authors": 8500,
  "avg_content_length": 4250.5,
  "total_content_length": 531312500,
  "estimated_total_words": 106262500
}
```

| Field | Description |
|-------|-------------|
| `articles_count` | Distinct articles (pubkey, `d` tag) |
| `unique_authors` | Distinct authors of those articles |
| `avg_content_length` | Mean content length of the latest versions, in bytes (`0` when there are none) |
| `total_content_length` | Sum of the latest versions' content lengths, in bytes |
| `estimated_total_words` | `total_content_length / 5` |

> **Changed with migration 019:** `articles_count` and the content statistics
> used to include every version of every article.

---

### Publishers

#### `GET /api/v1/stats/publishers`

Top publishers by event count (ties ordered by pubkey).

This reads the `publisher_activity` table from prepared
migration `019_api_publishers_longform` (see
[`docs/api_rollups.md`](../../docs/api_rollups.md)). Without that table the
endpoint returns `503` with `{"error": "metric_unavailable"}`.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `kind` | integer | - | Filter by event kind (not 445 or 1059) |
| `days` | integer | 30 | Days to include, 1–60 (longer windows exceed the 20 s query cap) |
| `limit` | integer | 100 | Number of publishers (max: 1000) |

The window covers whole UTC days: from the start of the UTC day containing
`now - days` onwards, so between `days` and `days + 1` days. It has no upper
bound, but an event dated more than a day after it was ingested counts on the
day after ingestion. Kinds 445 and 1059 (throwaway-key gift wraps and group
messages) are not ranked and return `400`. `first_event`/`last_event` are the
earliest and latest `created_at` in the window.

**Response**

```json
[
  {
    "pubkey": "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d",
    "event_count": 15000,
    "kinds_count": 12,
    "first_event": 1606719600,
    "last_event": 1735689000
  },
  {
    "pubkey": "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2",
    "event_count": 12500,
    "kinds_count": 8,
    "first_event": 1620000000,
    "last_event": 1735680000
  }
]
```

---

### Kinds

#### `GET /api/v1/kinds`

List all event kinds with counts.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `limit` | integer | 100 | Max results (max: 1000) |
| `sort` | string | `count` | Sort order: `count` (descending) or `kind` (ascending) |

**Response**

```json
[
  {
    "kind": 1,
    "event_count": 45000000,
    "unique_pubkeys": 500000,
    "first_seen": 1579219200,
    "last_seen": 1735915800
  },
  {
    "kind": 7,
    "event_count": 32000000,
    "unique_pubkeys": 350000,
    "first_seen": 1580515200,
    "last_seen": 1735915740
  }
]
```

| Field | Description |
|-------|-------------|
| `event_count` | Events of this kind (exact) |
| `unique_pubkeys` | **Approximate** distinct authors (`uniqCombined64` sketch, ~0.3% standard error; exact for small kinds) |
| `first_seen` | Unix timestamp (seconds) of the first event of this kind |
| `last_seen` | Unix timestamp (seconds) of the most recent event of this kind |

Read from the insert-fed `kind_totals` rollup (migration
`020_incremental_derived_views`): counts and timestamps are current (the
hourly `kinds_stats_mv` rescan it replaces was up to an hour old and counted
`unique_pubkeys` exactly). With `sort=count`, kinds with equal counts are
ordered by kind. `503 metric_unavailable` if the rollup does not exist.
`GET /api/v1/kinds/{kind}` below reports the same `unique_pubkeys`.

**Examples**

```bash
# Top 10 kinds by event count
GET /api/v1/kinds?limit=10

# All kinds sorted by kind number
GET /api/v1/kinds?sort=kind&limit=1000
```

---

#### `GET /api/v1/kinds/{kind}`

Detailed statistics for a specific event kind.

**Path Parameters**

| Parameter | Type | Description |
|-----------|------|-------------|
| `kind` | integer | Event kind number (0-65535) |

**Response**

```json
{
  "kind": 1,
  "event_count": 45000000,
  "unique_pubkeys": 500000,
  "first_seen": 1579219200,
  "last_seen": 1735915800,
  "avg_content_length": 142.5,
  "events_24h": 54000,
  "events_7d": 380000,
  "events_30d": 1650000
}
```

| Field | Description |
|-------|-------------|
| `event_count` | Events of this kind (exact) |
| `unique_pubkeys` | **Approximate** distinct authors (`uniqCombined64`, ~0.3% standard error; exact for small kinds), the same figure as `GET /api/v1/kinds` |
| `first_seen` | Unix timestamp (seconds) of the first event of this kind |
| `last_seen` | Unix timestamp (seconds) of the most recent event of this kind |
| `avg_content_length` | Average `content` length in bytes |
| `events_24h`, `events_7d`, `events_30d` | Events whose `created_at` is at or after the start of the UTC hour containing `now() - 1/7/30 days` (so up to one hour more than a rolling window), with no upper bound: future-dated events count, as before |

Read from the insert-fed rollups (no scan of `events_local`): `event_count`,
`unique_pubkeys`, `first_seen` and `last_seen` from `kind_totals` (migration
020), the average content length and the window counts from `kind_activity`
(migration `021_kind_activity`). Until 021 this endpoint scanned
`events_local` (exact `unique_pubkeys`, rolling windows to the second) and
timed out for large kinds such as kind 1.

**Errors**

| Status | Description |
|--------|-------------|
| 404 | Kind not found (no events of this kind exist) |
| 503 | `metric_unavailable`: `kind_totals` or `kind_activity` does not exist |

---

#### `GET /api/v1/kinds/{kind}/activity`

Activity time series for a specific kind.

**Path Parameters**

| Parameter | Type | Description |
|-----------|------|-------------|
| `kind` | integer | Event kind number (0-65535) |

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `group_by` | string | `day` | Group by: `day`, `week`, or `month` |
| `limit` | integer | 30 | Number of periods (max: 365 for day, 52 for week, 120 for month) |

**Response**

```json
[
  {
    "period": "2024-12-22",
    "event_count": 54000,
    "unique_pubkeys": 12000
  },
  {
    "period": "2024-12-21",
    "event_count": 52000,
    "unique_pubkeys": 11800
  }
]
```

| Field | Description |
|-------|-------------|
| `period` | First day of the UTC day, Monday week or month (`YYYY-MM-DD`) |
| `event_count` | Events whose `created_at` falls in the period (exact) |
| `unique_pubkeys` | **Approximate** distinct authors in the period (merged `uniqCombined64` sketches, ~0.3% standard error; exact below ~4k authors) |

Read from `kind_activity` (migration `021_kind_activity`): the `limit` most
recent periods up to the current one, newest first. Periods are UTC (before
021: the ClickHouse server time zone), and every event of
the current period counts, including any dated later in it (before 021:
`created_at <= now()`). `503 metric_unavailable` if the rollup does not
exist; `400` for another `group_by`.

**Examples**

```bash
# Daily activity for kind 1 (text notes)
GET /api/v1/kinds/1/activity

# Weekly activity for kind 7 (reactions)
GET /api/v1/kinds/7/activity?group_by=week&limit=12

# Monthly activity for kind 30023 (long-form content)
GET /api/v1/kinds/30023/activity?group_by=month
```

---

### Relay Distribution (NIP-65)

#### `GET /api/v1/stats/relays/distribution`

Returns relay popularity distribution based on NIP-65 relay lists (kind 10002 events). Only includes each user's latest relay list event.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `limit` | integer | 100 | Maximum relays to return (max: 1000) |

**Response**

```json
[
  {
    "relay_url": "wss://relay.damus.io",
    "user_count": 322768,
    "read_count": 280000,
    "write_count": 310000
  },
  {
    "relay_url": "wss://nos.lol",
    "user_count": 216707,
    "read_count": 200000,
    "write_count": 210000
  }
]
```

| Field | Description |
|-------|-------------|
| `relay_url` | Normalized relay URL (lowercase, no trailing slash, wss:// only) |
| `user_count` | Number of users listing this relay in their latest NIP-65 event |
| `read_count` | Users listing this relay for reading (includes read+write) |
| `write_count` | Users listing this relay for writing (includes read+write) |

**Notes**

- Data is pre-computed: hourly (at :20) from the `relay_list_latest` rollup
  since migration 020's cutover, every 6 hours by rescanning `events_local`
  before it
- When a user's latest relay lists share a `created_at`, the one with the
  lowest event id counts (before 020: an arbitrary one)
- Only includes relays with 10+ users (filters noise)
- Excludes invalid URLs (localhost, malformed, ws://, etc.)
- Shows current relay preferences (latest event per user, not historical)

**Examples**

```bash
# Top 50 relays
GET /api/v1/stats/relays/distribution?limit=50

# Default top 100
GET /api/v1/stats/relays/distribution
```

**Cache-Control:** `max-age=60` (1 minute), `stale-while-revalidate=300` (the default)

---

### Relays

Relay endpoints require the `RELAY_DB_PATH` environment variable to be set to the path of the ingester's SQLite relay stats database. If it is unset or the file does not exist at startup, these endpoints return `500` (`internal_error`).

Each request opens its own read-only SQLite connection (`SQLITE_OPEN_READONLY`,
the flag behind `mode=ro`, plus `PRAGMA query_only`), so a response includes
everything the ingester has committed, transactions still in the WAL
included, and a replaced database file is picked up without a restart. The API
never writes the database, its WAL or its directory. Releases up to and
including `api-fe54f04` instead held one connection, opened at startup with
`immutable=1`: SQLite then ignores the WAL and takes no locks, so relay data
was only as fresh as the last checkpoint and a concurrent checkpoint could
tear a read.

In production `/data` is read-only for the API: `pensieve-api.service` runs
with `ProtectSystem=strict` and no `ReadWritePaths`. The API therefore can
never create the database's `-wal` and `-shm` files, and each request picks
one of two ways to read:

- **Ingester running** (`-wal` and `-shm` exist): a normal read-only WAL
  reader. It cannot write `-shm`, so SQLite opens it read-only and indexes the
  WAL in private memory: slower than shared memory, still consistent. (A
  reader that can write `-shm` updates its read marks there, as every WAL
  reader does.)
- **Ingester stopped cleanly** (both files gone; the last connection
  checkpointed the WAL into the main file and deleted them): an
  `immutable=1` read of the main file, which is complete. A plain read-only
  open would try to create `-wal` and fail. If the ingester starts during such
  a read, it only appends to its new WAL; the main file changes at the first
  checkpoint, about 4 MiB of commits later.

Other behaviour:

- **Journal mode** is read from the database header once at startup, before
  any connection exists. Reading it again later through a second file
  descriptor would release live connections' POSIX locks.
- **Race at stop:** if the ingester stops between the check for `-wal`/`-shm`
  and the open, the open re-checks and falls back to the snapshot. If the API
  *can* write the directory (not production), SQLite instead recreates the
  files, owned by the API's user with the database file's mode. Where that user
  is not the database's owner, the ingester may then be unable to write them
  until they are removed. Run the API as the database's owner, or with the
  directory read-only.
- **Busy:** a reader waits up to 2 s (`busy_timeout`) on `SQLITE_BUSY`, then
  the request fails with `503 analytics_busy`. At most 16 relay reads run at
  once; further requests get `503 analytics_busy` immediately.
- **Unreadable files:** if SQLite cannot open or create a file it needs
  (`SQLITE_CANTOPEN`, `SQLITE_READONLY_DIRECTORY`,
  `SQLITE_READONLY_CANTINIT`), the request fails with
  `503 metric_unavailable`.

#### `GET /api/v1/relays/summary`

Returns aggregate relay statistics.

**Response**

```json
{
  "total_discovered": 3500,
  "total_active": 50,
  "total_blocked": 120,
  "total_failing": 45,
  "total_idle": 85,
  "total_pending": 3200,
  "avg_events_per_minute": 2345.67,
  "events_last_hour": 140740,
  "novel_events_last_hour": 85234
}
```

---

#### `GET /api/v1/relays`

Returns a list of relays with optional filtering and sorting.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `status` | string | - | Filter by status: `active`, `idle`, `pending`, `failing`, `blocked` |
| `tier` | string | - | Filter by tier: `seed`, `discovered` |
| `sort_by` | string | `score` | Sort by: `score`, `events`, `url` |
| `order` | string | `desc` | Sort order: `asc`, `desc` |
| `limit` | integer | 100 | Maximum results (max 1000) |
| `offset` | integer | 0 | Offset for pagination |

**Response**

```json
{
  "relays": [
    {
      "url": "wss://relay.damus.io",
      "status": "active",
      "tier": "seed",
      "last_connected_at": 1735600000,
      "consecutive_failures": 0,
      "blocked_reason": null,
      "score": 0.95,
      "events_last_hour": 12500,
      "novel_events_last_hour": 8200
    }
  ],
  "total": 3500
}
```

---

#### `GET /api/v1/relays/throughput`

Returns hourly event throughput for the last N hours.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `hours` | integer | 24 | Number of hours to include (max 168) |

**Response**

```json
[
  {
    "hour_start": 1735596000,
    "events_received": 145000,
    "events_novel": 89000,
    "active_relays": 48
  }
]
```

---

## Error Responses

All errors return JSON with the following structure:

```json
{
  "error": "error_code",
  "message": "Human-readable description"
}
```

`message` is omitted for `unauthorized`, `analytics_busy`, and `metric_unavailable`.

| Status | Error Code | Description |
|--------|------------|-------------|
| 400 | `bad_request` | Invalid request parameters |
| 401 | `unauthorized` | Missing or invalid Bearer token |
| 404 | `not_found` | Resource not found |
| 500 | `internal_error` | Server error |
| 500 | `database_error` | Database query failed |
| 500 | `serialization_error` | Response serialization failed |
| 503 | `analytics_busy` | Analytics query capacity exhausted or query timed out, or the relay database stayed busy for 2 s or 16 relay reads were already running; retry later |
| 503 | `metric_unavailable` | Metric temporarily disabled (currently `/stats/engagement`), or `/stats/publishers` and `/stats/longform` before migration 019 is applied, or a relay endpoint when SQLite cannot open or create a file the relay database needs |

---

## Common Nostr Kinds

For reference, here are some common event kinds:

| Kind | Description | NIP |
|------|-------------|-----|
| 0 | Profile Metadata | NIP-01 |
| 1 | Text Note | NIP-01 |
| 3 | Follow List | NIP-02 |
| 4 | Encrypted DM | NIP-04 |
| 5 | Deletion | NIP-09 |
| 6 | Repost | NIP-18 |
| 7 | Reaction | NIP-25 |
| 1059 | Gift Wrap | NIP-59 |
| 10002 | Relay List | NIP-65 |
| 30023 | Long-form Content | NIP-23 |
| 34235 | Video | NIP-71 |
| 34236 | Short Video | NIP-71 |

See the [NIPs repository](https://github.com/nostr-protocol/nips) for the complete list.
