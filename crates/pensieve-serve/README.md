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
| `PENSIEVE_API_TOKENS` | *(required)* | Comma-separated API tokens |
| `RELAY_DB_PATH` | *(optional)* | Path to ingester's SQLite relay-stats.db for relay endpoints |

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

**Suggested cache TTL:** 5 minutes

---

#### `GET /api/v1/stats/pubkeys/total`

Returns total unique pubkeys.

**Response**

```json
{
  "count": 56499585
}
```

**Suggested cache TTL:** 5 minutes

---

#### `GET /api/v1/stats/kinds/total`

Returns distinct event kinds seen in the last 30 days.

**Response**

```json
{
  "count": 1020
}
```

**Suggested cache TTL:** 1 hour

---

#### `GET /api/v1/stats/events/earliest`

Returns earliest event timestamp (Nostr genesis: Nov 7, 2020).

**Response**

```json
{
  "timestamp": 1606719600
}
```

**Suggested cache TTL:** 1 hour

---

#### `GET /api/v1/stats/events/latest`

Returns latest event timestamp (excludes future timestamps).

**Response**

```json
{
  "timestamp": 1735689000
}
```

**Suggested cache TTL:** 10 seconds

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
| `days` | integer | Shorthand: events from last N days (overrides `since`/`until`) |
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
# Total events of kind 1 (text notes)
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

Active user metrics exclude throwaway keys (kinds 445 and 1059 per NIP-59).

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
| `has_profile` | Users who have published a kind 0 (profile metadata) |
| `has_follows_list` | Users who have published a kind 3 (follow list) |
| `has_profile_and_follows_list` | Users with both profile and follows (strongest signal of established user) |
| `total_events` | Total events in the period |

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
| `cohort_start` | date | 12 weeks ago | Start date for first cohort |
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

Statistics for long-form content (kind 30023).

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `days` | integer | - | Days to include (omit for all time) |

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

---

### Publishers

#### `GET /api/v1/stats/publishers`

Top publishers by event count.

**Query Parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `kind` | integer | - | Filter by event kind |
| `days` | integer | 30 | Days to include |
| `limit` | integer | 100 | Number of publishers (max: 1000) |

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
    "first_seen": "2020-01-17T00:00:00Z",
    "last_seen": "2024-12-22T15:30:00Z"
  },
  {
    "kind": 7,
    "event_count": 32000000,
    "unique_pubkeys": 350000,
    "first_seen": "2020-02-01T00:00:00Z",
    "last_seen": "2024-12-22T15:29:00Z"
  }
]
```

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
  "first_seen": "2020-01-17T00:00:00Z",
  "last_seen": "2024-12-22T15:30:00Z",
  "avg_content_length": 142.5,
  "events_24h": 54000,
  "events_7d": 380000,
  "events_30d": 1650000
}
```

**Errors**

| Status | Description |
|--------|-------------|
| 404 | Kind not found (no events of this kind exist) |

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

### Relays

Relay endpoints require the `RELAY_DB_PATH` environment variable to be set to the path of the ingester's SQLite relay stats database.

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

| Status | Error Code | Description |
|--------|------------|-------------|
| 400 | `bad_request` | Invalid request parameters |
| 401 | `unauthorized` | Missing or invalid Bearer token |
| 404 | `not_found` | Resource not found |
| 500 | `internal_error` | Server error |
| 500 | `database_error` | Database query failed |

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
