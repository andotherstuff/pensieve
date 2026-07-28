# Analytics Endpoint Migration Ledger

*Status: draft design / P3 planning*
*Last updated: 2026-07-28*

This document freezes the current `pensieve-serve` analytics surface and maps
each ClickHouse-backed endpoint to a proposed DuckDB computation and Postgres
serving shape. It is the working contract for P3 of the lakehouse migration.

The first objective is behavioral parity with the existing API. This document
does not accept a Postgres schema, sketch implementation, batch frequency, or
intentional behavior change. Those decisions must be supported by endpoint
tests and old-versus-new evidence.

## 1. Scope

The current router exposes 30 GET routes:

- 2 public service routes: `/health` and `/docs`;
- 28 authenticated routes:
  - 24 ClickHouse-backed analytics routes covered by this ledger;
  - 3 relay-operational routes backed by the ingester's SQLite database; and
  - 1 authenticated `/ping`.

The old migration-plan count of 32 analytics endpoints is stale. The router in
`crates/pensieve-serve/src/routes/mod.rs` is authoritative.

The separate `pensieve-preview` event/profile lookup service is out of scope.
The migration plan retires it. If random event lookup remains a product
requirement, it needs a separate ID/address index and cannot be served from the
small analytics rollups described here.

## 2. Parity contract

For each route, parity covers:

1. accepted path and query parameters;
2. defaults, maximums, validation, and error behavior;
3. response JSON shape and ordering;
4. the meaning and time domain of every metric;
5. empty-data behavior;
6. numeric exactness or an explicitly accepted approximation tolerance; and
7. data freshness, HTTP cache policy, and server-side cache policy.

The comparison domain is the canonical, ID-deduplicated event set representable
by both systems. A ClickHouse result that counts duplicate physical rows,
narrows an unsigned timestamp, includes malformed future dates, or reflects a
known stale materialized view is evidence to classify, not automatically the
desired lakehouse result.

Every discrepancy must be assigned one of:

- **bug** — the new implementation is wrong;
- **expected approximation** — within the accepted per-metric tolerance;
- **intentional correction** — a documented old-stack behavior is not carried
  forward; or
- **old-stack uncertainty** — the ClickHouse result cannot be treated as a
  reliable oracle until separately resolved.

## 3. Proposed execution boundary

The API must not run object-storage DuckDB scans on the request path.

```text
active-file snapshot + new published work units
                    │
                    ▼
        DuckDB analytics builder
          derive → aggregate → validate
                    │
                    ▼
        Postgres staging rows keyed by run_id
                    │
             atomic publication
                    ▼
       Postgres current serving relations
                    │
                    ▼
        pensieve-serve + Grafana
```

DuckDB owns large scans and transformations. Postgres owns small indexed
serving relations, run metadata, and atomic publication. Parquet remains the
source of truth; all Postgres state is rebuildable.

Each analytics run must record at least:

- active-file snapshot ID;
- previous successful run ID, when incremental;
- input work-unit IDs and object checksums;
- code/query version;
- start, completion, and publication timestamps;
- row counts and validation results per output relation; and
- whether the run was a full rebuild, affected-period rebuild, or incremental
  update.

## 4. Shared semantic rules to freeze

### 4.1 Event identity

All lake computations count logical events by `id`, not physical Parquet rows.
Raw active snapshots may contain an ID more than once. Until a snapshot is
certified deduplicated, DuckDB must apply the canonical deterministic
signature-selection rule before computing metrics.

### 4.2 Time

The archive preserves the full unsigned Nostr `created_at` domain. The current
API and ClickHouse schema expose a narrower date/timestamp domain. The
analytics builder must:

- preserve the source value while reading;
- explicitly select the API-representable domain for date rollups;
- use UTC boundaries;
- keep `since` inclusive and `until` exclusive where the current handler does;
- record whether future timestamps are excluded for each metric; and
- never infer ingestion order from `created_at`.

Late-arriving events may change old date buckets. Incremental runs therefore
must upsert or rebuild affected historical periods rather than update only
"today."

### 4.3 Active-user domain

The current active-user and first-seen pipeline excludes kinds `445` and `1059`
as throwaway-key activity. Profile and follows flags mean that a pubkey has a
kind `0` or kind `3` event in the current derived ClickHouse state. The exact
point-in-time versus ever-observed semantics of those flags must be preserved
for parity tests before any correction is proposed.

### 4.4 Distinct counts

Additive counts and sums should be exact on the common ID-deduplicated domain.
Distinct pubkey metrics require either exact state or mergeable sketches.

The current plan names Apache DataSketches, but P3 must first prove:

- compatible serialization across the selected DuckDB and Postgres/runtime
  components;
- stable union results across daily, weekly, and monthly windows;
- acceptable error on representative Nostr cardinalities; and
- repeatable deployment of any non-core database extension.

Postgres does not need to understand sketches if the batch job can merge them
and store final numeric results. Serialized sketch state should be stored only
where arbitrary-window query-time composition justifies it.

### 4.5 Atomic publication

One API response must not combine rollups from different dataset snapshots.
The builder writes a complete staged run, validates it, and then changes the
single current-run reference transactionally. Readers either see the previous
complete run or the next complete run.

## 5. Current cache and freshness baseline

There are two cache layers:

- the in-process Moka response cache; and
- HTTP `Cache-Control`/ETag policy.

The default in-process TTL is 300 seconds. Explicit TTLs are 10 seconds
(`REALTIME`), 600 seconds (`TIME_SERIES`), and 3600 seconds (`STABLE`), but most
handlers call the default helper even where comments describe another TTL.
HTTP policy separately uses 10 seconds for latest event, 300 seconds for event
and pubkey totals, 600 seconds for selected time series, 3600 seconds for kinds
total and earliest event, and 60 seconds otherwise.

Consequences:

- all five scalar fetch helpers convert ClickHouse query errors to zero, so the
  granular scalar routes as well as `/stats` can return a successful false-zero
  response;
- `/stats/events/latest` is documented and HTTP-cached as near-real-time, but
  currently uses the 300-second in-process default;
- `/stats` is documented as one minute but uses the 300-second default;
- active-user handlers are documented as ten minutes but use the 300-second
  default; and
- ClickHouse refresh schedules can be much slower than either response-cache
  layer.

Cache behavior is part of the migration audit, but accidental TTL mismatches do
not have to become permanent. Any correction must be explicit and tested.

## 6. Endpoint ledger

Candidate Postgres names below communicate grain and purpose. They are not
accepted DDL.

### 6.1 Overview and scalar endpoints

| Endpoint | Current contract and ClickHouse source | Proposed DuckDB computation | Candidate Postgres surface | Parity notes |
|---|---|---|---|---|
| `GET /api/v1/stats` | No params. Combines total events, pubkeys, kinds, earliest, and latest. Individual query errors are currently converted to zero by the internal fetch helpers. | Read the five published scalar values from one completed analytics run. | `analytics_overview` keyed by `run_id`. | Preserve JSON fields. Decide whether silent zero on a failed component is retained; preferred correction is to fail rather than publish false zeroes. |
| `GET /api/v1/stats/events/total` | Returns `sum(rows)` for active `events_local` parts; described as approximate. | Count distinct canonical event IDs in the selected snapshot. Maintain incrementally only with duplicate-safe processing. | `analytics_overview.total_events`. | ClickHouse physical-row count may differ from the logical lake count. Treat this as an expected intentional correction after quantifying it. |
| `GET /api/v1/stats/pubkeys/total` | Counts rows in `pubkey_first_seen_data`; first-seen ingestion excludes kinds `445` and `1059`. | Build one first-seen record per eligible pubkey and count it. | `pubkey_first_seen` plus `analytics_overview.total_pubkeys`. | Exact metric. Validate old aggregate-state duplication and invalid-date filtering separately. |
| `GET /api/v1/stats/kinds/total` | Approximate `uniq(kind)` for `created_at >= now()-30d`; it does not explicitly exclude future timestamps. | Distinct kinds in the agreed rolling 30-day API domain. | Scalar derived from `event_daily_kind`. | Decide whether future events remain included. The likely correction is `created_at <= as_of`. |
| `GET /api/v1/stats/events/earliest` | Minimum `created_at` over all rows, converted to `u32`, then clamped to Nostr genesis. | Minimum API-representable timestamp, then the same genesis clamp. | `analytics_overview.earliest_event`. | Exact on the shared timestamp domain. Add empty, pre-genesis, `u32::MAX`, and larger unsigned fixtures. |
| `GET /api/v1/stats/events/latest` | Maximum `created_at <= now()`, converted to `u32`. Documented/HTTP TTL 10s; effective in-process TTL 300s. | Maintain a non-future event-time watermark from published events plus the selected live-delta source. | `analytics_overview.latest_event` or a dedicated hot watermark row. | Freshness is the key design decision. Parquet-only publication may lag the advertised endpoint by the live batch age. |

### 6.2 Event and kind activity

| Endpoint | Current contract and ClickHouse source | Proposed DuckDB computation | Candidate Postgres surface | Parity notes |
|---|---|---|---|---|
| `GET /api/v1/stats/events` | Optional `kind`, `since`, `until`, `days`, `group_by` (`day`, `week`, or `month`), and `limit` default 100/max 1000. Defaults to 30 days when no time filter is supplied. `days` overrides explicit dates. Ungrouped and kind-filtered queries scan `events_local`; unfiltered grouped queries use `daily_user_stats`. Future events are excluded only on the raw-event path. | Produce exact event counts and mergeable pubkey distinct state at daily+kind grain. Compose week/month and requested windows from daily rows. | `event_daily_kind`; optional all-kind rows or query-time sum/merge. | Preserve defaulting, inclusive `since`, exclusive `until`, descending order, dynamic JSON object/array shape, and group validation. The current unfiltered daily path uses `count()` of per-pubkey rows as unique pubkeys while other paths use approximate `uniq`. |
| `GET /api/v1/stats/throughput` | Optional `kind`. Counts events with `created_at >= now()-7d`, divides by 168, and does not explicitly cap future timestamps. | Sum the rolling seven-day event buckets at a fixed `as_of`; include a small current-period delta if required. | Derived from `event_hourly_kind` or daily rows plus current hourly rows. | Exact additive metric. Decide future handling and whether "7 days" means an exact 168-hour interval or seven UTC dates. |
| `GET /api/v1/kinds` | `limit` default 100/max 1000; `sort` accepts `count` or `kind`, invalid values are 400. Reads hourly `kinds_stats_mv`: all-time count, exact pubkeys, min/max timestamps. | Aggregate all-time by kind from deduplicated events; retain exact or agreed approximate distinct state. | `kind_all_time`. | Preserve sort and limits. Current refresh is hourly. |
| `GET /api/v1/kinds/{kind}` | `u16` path. Returns all-time count, exact pubkeys, first/last, average content length, and 1d/7d/30d counts. Missing kind returns 404. | Combine all-time kind summary with rolling date/hour buckets. Read `content` only for kind summary builds that need its length. | `kind_all_time` plus `event_daily_kind`/`event_hourly_kind`. | Preserve 404 and response types. Decide future-date treatment for rolling and last-seen values. |
| `GET /api/v1/kinds/{kind}/activity` | `group_by` accepts `day`, `week`, or `month` and defaults to day. `limit` default 30; max 365/52/120 by grain. All-time scan grouped by period with exact pubkeys. | Compose period rows from daily kind counts and distinct state. | `event_daily_kind`. | Exact counts; distinct tolerance to lock. No `since` parameter, so descending limit semantics must match. |
| `GET /api/v1/stats/activity/hourly` | `days` default 7/max 90; optional kind. Groups events in the rolling window by UTC hour-of-day; returns counts, approximate pubkeys, and count/days. | Aggregate daily event facts at `(date, hour_of_day, kind)` and merge the selected dates/window. | `event_daily_hour_kind`. | Rolling `now()-N days` is not identical to N full UTC dates. Either store hourly event-time buckets or document a boundary correction. |
| `GET /api/v1/stats/publishers` | `days` default 30; optional kind; `limit` default 100/max 1000. Groups the rolling raw event window by pubkey and returns count, distinct kinds, min/max event time. | Aggregate high-cardinality publisher facts over the requested window. | Candidate `publisher_daily_kind`, or a bounded-window top-K product with explicit supported windows. | This is not safely composable from daily top-K lists: a publisher outside every daily top-K can enter the multi-day top-K. This endpoint drives a potentially large serving relation and needs a measured design. |

### 6.3 Active and new users

All current active-user relations exclude kinds `445` and `1059`. Current
profile/follows flags originate from kind `0` and `3` presence tables.

| Endpoint | Current contract and ClickHouse source | Proposed DuckDB computation | Candidate Postgres surface | Parity notes |
|---|---|---|---|---|
| `GET /api/v1/stats/users/active` | No params. Returns the latest daily, weekly, and monthly rows from scheduled summary tables, bounded at Nostr genesis and the current period. | Read the current rows from the corresponding completed period products. | `active_users_period` with `grain` and `period_start`. | The three ClickHouse refresh schedules differ, so the current response can mix computation times. New publication should use one run, with freshness recorded per product. |
| `GET /api/v1/stats/users/active/daily` | `limit` default 30/max 365; optional inclusive `since`. Fetches all daily summary rows, filters and truncates in Rust. | Build daily exact/sketched active pubkeys, profile/follows subsets, and exact event totals. | `active_users_period(grain='day')`. | Preserve descending order and genesis/current-date bounds. |
| `GET /api/v1/stats/users/active/weekly` | Same response; `limit` default 12/max 52. Monday period starts. | Merge daily identity state into calendar weeks. | `active_users_period(grain='week')`. | Daily unique counts cannot be summed; merge identity/sketch state or compute weekly directly. |
| `GET /api/v1/stats/users/active/monthly` | Same response; `limit` default 12/max 120. First-of-month starts. | Merge daily identity state into calendar months. | `active_users_period(grain='month')`. | Same non-additivity rule as weekly. |
| `GET /api/v1/stats/users/new` | `group_by` accepts `day`, `week`, or `month` and defaults to day; `limit` default 30 with max 365/52/120; optional inclusive `since`. Sums scheduled daily first-seen counts. | Compute one eligible first-seen timestamp per pubkey, filter to the API date domain, then aggregate. | `pubkey_first_seen` and `new_users_daily`. | Exact and additive after each pubkey has one canonical first-seen row. Late historical events can move a pubkey to an older period, requiring subtraction from the former bucket. |
| `GET /api/v1/stats/users/retention` | `cohort_size` accepts `week` or `month` and defaults to week; `limit` default 12/max 52; optional `cohort_start`. Reads daily-refreshed cohort/activity counts and calculates percentages in Rust. | Join eligible pubkey first-seen cohort to active periods and aggregate distinct pubkeys by `(cohort, activity_period)`. | `cohort_retention_period`. | Preserve period ordering and vector construction. Current query applies `.take(limit)` to an ascending `BTreeMap`, so it may return the oldest qualifying cohorts rather than the most recent `limit`; classify before migration. |

### 6.4 Zaps and semantic event analytics

| Endpoint | Current contract and ClickHouse source | Proposed DuckDB computation | Candidate Postgres surface | Parity notes |
|---|---|---|---|---|
| `GET /api/v1/stats/zaps` | `days` default 30 with no current maximum; optional `group_by` accepting `day`, `week`, or `month`; `limit` default 30/max 365. Uses parsed positive `zap_amounts_data`; returns exact count/sats, approximate senders/recipients, and average. | Reproduce the accepted bolt11/tag parsing rules, materialize one validated zap fact per event ID, and aggregate by day with sender/recipient distinct state. | `zap_daily` plus optional `zap_event_fact` outside the serving schema. | Parsing parity with migration 009 is the first gate. Preserve millisatoshi-to-satoshi truncation and grouped/aggregate JSON shapes. Decide a sane `days` maximum separately. |
| `GET /api/v1/stats/zaps/histogram` | `days` default 30 with no maximum. Seventeen fixed inclusive sat buckets, positive amounts only; returns count/sats and percentages rounded to two decimals. | Bucket the same canonical zap facts, preferably at daily+bucket grain. | `zap_bucket_daily`. | Exact additive counts/sats; percentages derive at query time. Preserve boundaries, final `100K+` display maximum, truncation, and rounding. |
| `GET /api/v1/stats/engagement` | `days` default 30 with no maximum. A reply is any kind `1` event containing an `e` tag; reactions are kind `7`; original notes are total kind `1` minus replies. | Derive `is_reply` from canonical nested tags and aggregate daily original/reply/reaction counts. | `engagement_daily`. | Exact counts. The definition does not distinguish root/reply markers or validate referenced IDs; preserve that simple rule for parity. |
| `GET /api/v1/stats/longform` | Optional `days`; omission means all time. Kind `30023` only. Returns event count, approximate authors, byte length average/sum, and words estimated as total bytes/5. | Aggregate kind `30023` content byte lengths and author distinct state by day plus all-time. | `longform_daily` and/or `longform_all_time`. | Rust/ClickHouse/DuckDB string length semantics must be tested with non-ASCII content. Preserve the existing byte/character behavior only after fixtures identify it. |
| `GET /api/v1/stats/relays/distribution` | `limit` default 100/max 1000. Reads a six-hour refresh of each pubkey's latest kind `10002`, normalizes `wss://` relay URLs, interprets read/write markers, filters invalid URLs and counts below 10. | Select the latest canonical kind `10002` per pubkey with deterministic tie-breaking, unnest tags, reproduce URL normalization and mode rules, and aggregate. | `relay_distribution_current`. | This is current-state replacement semantics, not an additive event rollup. The exact normalization/filter contract lives in migration 014 and needs shared fixtures. |

## 7. Routes that do not move through DuckDB

The following remain backed by the ingester's operational relay database during
the first analytics migration:

- `GET /api/v1/relays/summary`;
- `GET /api/v1/relays`;
- `GET /api/v1/relays/throughput`.

They describe relay discovery, connection state, scores, events received, and
novel events observed by this Pensieve instance. Those facts are not derivable
from canonical Nostr event Parquet. Moving them to Postgres would be a separate
operational-database replication decision, not part of ClickHouse retirement.

`/health`, `/docs`, and `/api/v1/ping` likewise require no analytical migration.

## 8. Initial product table families

The endpoint ledger currently implies these logical products:

| Product | Grain / key | Consumers |
|---|---|---|
| Analytics run catalog | `run_id`, active snapshot ID | every endpoint and verifier |
| Overview | one row per run | overview and scalar stats |
| Pubkey first seen | `pubkey` | total pubkeys, new users, retention |
| Event activity | day/hour, optional kind | events, throughput, kinds, hourly activity |
| Kind all-time | kind | kinds list/detail |
| Active users | grain + period | active-user summary and series |
| Cohort retention | grain + cohort + activity period | retention |
| Zap facts/rollups | day + optional bucket | zap totals and histogram |
| Engagement | day | engagement |
| Long-form | day/all-time | long-form |
| Publisher activity | day + pubkey + optional kind, design pending | publishers |
| Relay distribution | current relay URL | NIP-65 distribution |

This is a logical inventory, not permission to create all tables immediately.
In particular, publisher activity and query-time distinct-state storage require
size measurements before DDL is accepted.

## 9. Build and verification order

### Slice A — additive and scalar

Implemented in the first shadow builder:

1. [x] analytics run catalog and atomic publication;
2. [x] total logical events;
3. [x] earliest/latest event;
4. [x] daily event counts and seven-day throughput; and
5. [x] all-time and daily kind counts.

This slice establishes catalog consumption, deduplication, time filtering,
late-event updates, Postgres publication, and the old/new comparison harness
without sketch or nested-tag dependencies.

The implementation and operator procedure are in
[`analytics_slice_a.md`](analytics_slice_a.md). Completion here means the
builder, versioned serving DDL, atomic pointer, and local exactness fixtures
exist. It does not mean production shadow deployment, ClickHouse parity,
endpoint cutover, or acceptance of the provisional time semantics.

### Slice B — identity and distinct state

1. eligible pubkey first seen;
2. total and new pubkeys;
3. event/kind unique pubkeys;
4. active users; and
5. cohort retention.

This slice selects and validates exact-versus-sketch behavior.

### Slice C — semantic transformations

1. engagement tag classification;
2. long-form byte-length rules;
3. zap parsing and buckets; and
4. latest NIP-65 relay-list reduction.

### Slice D — high-cardinality serving

1. publisher ranking storage/compute benchmark;
2. bounded-window and arbitrary-window correctness proof; and
3. final endpoint implementation if the measured relation is acceptable.

## 10. Comparison harness

The harness sends the same authenticated request to the ClickHouse-backed and
Postgres-backed implementations and compares normalized JSON plus metadata.

At minimum it must cover:

- empty data and one-event fixtures;
- duplicate event IDs in different files;
- two valid signatures for one event ID;
- late-arriving historical events;
- future timestamps, pre-genesis timestamps, `u32::MAX`, and larger unsigned
  timestamps;
- ASCII and multi-byte `content`;
- one-element and multi-element tags;
- excluded kinds `445` and `1059`;
- all supported day/week/month boundaries;
- DST-independent UTC calculations;
- default, minimum, maximum, and invalid query parameters;
- no-group and grouped dynamic response shapes;
- top-K ties and deterministic ordering;
- zero-denominator percentages; and
- a production request matrix sampled from actual dashboard traffic.

For approximate fields the harness records absolute difference, relative
difference, configured tolerance, and repeatability across rebuilds. Exact
fields compare bit-for-bit after the documented old-stack divergence rules.

Every result row must identify:

- endpoint and normalized parameters;
- ClickHouse observation time;
- Postgres analytics run and snapshot IDs;
- ClickHouse and Postgres data freshness;
- comparison classification; and
- linked expected-divergence entry, when applicable.

## 11. Decisions required before cutover

1. Is `/stats/events/latest` required to remain meaningfully fresher than the
   maximum Parquet batch age? If yes, select the live-delta source.
2. Does the public contract follow current future-timestamp behavior endpoint
   by endpoint, or adopt one consistent `created_at <= as_of` rule?
3. Which distinct metrics must remain exact?
4. Should serialized sketches ever be merged by Postgres, or only by the
   analytics builder?
5. Is the current retention cohort ordering a bug to fix?
6. Is the publisher endpoint important enough to justify a high-cardinality
   serving relation, or can its supported windows be narrowed?
7. Should database/query failures continue returning zero in the overview?
8. What freshness objectives apply to each endpoint family independently of
   response-cache TTL?

These decisions should be made from the first comparison runs and relation-size
benchmarks, not from DDL speculation.
