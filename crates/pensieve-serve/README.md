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

### Stats

#### `GET /api/v1/stats`

High-level overview of the event store.

**Response**

```json
{
  "total_events": 123456789,
  "total_pubkeys": 987654,
  "total_kinds": 142,
  "earliest_event": "2020-01-17T00:00:00Z",
  "latest_event": "2024-12-22T15:30:00Z"
}
```

---

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

### Active Users

Active users exclude throwaway keys (kinds 445 and 1059 per NIP-59).

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
  },
  {
    "period": "2024-12-21",
    "active_users": 44500,
    "has_profile": 31800,
    "has_follows_list": 27500,
    "has_profile_and_follows_list": 24800,
    "total_events": 148000
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

