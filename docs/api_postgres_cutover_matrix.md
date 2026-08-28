# Postgres API cutover matrix

Status: pre-cutover audit  
Scope: the 24 ClickHouse-backed analytics routes under `/api/v1`  
Out of scope: `/ping` and the three operational `/relays` routes, which read
the ingester's SQLite relay database rather than ClickHouse.

## Readiness rules

A route is ready only when every response field and accepted parameter shape
is derivable from one atomically selected analytics run, an attached versioned
product, or the explicit ingestion watermark. A headline count does not make a
partially covered response ready.

The Postgres request path must never scan Parquet, DuckDB, raw event history,
or cardinality-sized identity state. Fixed-memory sketch unions are allowed
only for fields whose approximation gate passed. All time windows are anchored
to the selected run's `as_of_epoch` or a recorded complete-hour/day boundary;
wall-clock time must not silently mix a newer request boundary with an older
snapshot.

Status values below mean:

- **ready**: an accepted relation already covers the complete route contract;
- **gated**: the relation is implemented, but its production canary or atomic
  recurring publication has not passed yet;
- **partial**: some fields or parameter shapes are not represented by the
  current Postgres products; and
- **external**: the field deliberately comes from the ingestion watermark,
  not the analytics transaction.

## Route matrix

| Route | Required product and contract | Status before Slice 9.5 | Remaining gate |
|---|---|---|---|
| `GET /stats` | `current_overview` plus exact eligible-pubkey total; latest timestamp comes from the ingestion watermark | partial | Live latest-event watermark and one response identity/freshness policy |
| `GET /stats/events/total` | `current_overview.total_events` | ready | Route comparison only; Postgres returns canonical logical events rather than ClickHouse part rows |
| `GET /stats/pubkeys/total` | `current_overview.total_pubkeys` | ready | Route comparison only |
| `GET /stats/kinds/total` | `current_overview.kinds_30d` | ready | Anchor the 30-day boundary to the selected run |
| `GET /stats/events/earliest` | `current_overview.earliest_event` | ready | Preserve the Nostr-genesis clamp in the route adapter |
| `GET /stats/events/latest` | latest sealed canonical event watermark | external | Implement and validate the ingestion-owned atomic watermark and freshness objective |
| `GET /stats/events` | `current_event_daily`, `current_event_daily_kind`, fixed-grain exact distincts, and flexible-distinct leaves | partial | Exact hourly additive counts for moving `days`; complete-hour anchoring; accepted sketch union for flexible distincts |
| `GET /stats/throughput` | exact sparse hourly event counts over the last 168 complete hours, optionally by kind | partial | Slice 9.5 hourly-count product |
| `GET /stats/users/active` | latest rows from `current_active_users_period` for day/week/month | ready | Route comparison and empty-dataset behavior |
| `GET /stats/users/active/daily` | `current_active_users_period` with `grain='day'` | ready | Ordering, `since`, and limit comparison |
| `GET /stats/users/active/weekly` | `current_active_users_period` with `grain='week'` | ready | Ordering, `since`, and limit comparison |
| `GET /stats/users/active/monthly` | `current_active_users_period` with `grain='month'` | ready | Ordering, `since`, and limit comparison |
| `GET /stats/users/retention` | `current_cohort_retention_period` | ready | Preserve the intentional newest-cohort-first correction and period-zero semantics |
| `GET /stats/users/new` | `current_new_users_daily`, composed to day/week/month | ready | Ordering, `since`, and limit comparison |
| `GET /stats/activity/hourly` | Slice 9.5 hourly event counts plus flexible-distinct leaf unions grouped by UTC hour-of-day | partial | Exact additive product and accepted sketch comparison for every `days`/kind shape |
| `GET /stats/zaps` | semantic zap daily rows plus zap-distinct sender/recipient leaves | gated | Slice 7 build, comparison, dormant publication, and recurring publication |
| `GET /stats/zaps/histogram` | semantic 17-bucket daily count/amount rows | gated | Slice 7 build, boundary comparison, deterministic percentage rounding, and publication |
| `GET /stats/engagement` | semantic engagement daily rows | gated | Slice 7 build, positional-tag comparison, and publication |
| `GET /stats/longform` | semantic long-form daily rows plus kind-30023 flexible-distinct author leaves | gated | Slice 7 additive and Slice 6 sketch gates; exact all-time author contract or accepted approximation must be explicit |
| `GET /stats/publishers` | exact predefined-window publisher ranking rows | gated | Slice 9 benchmark/build/publication; reject unsupported `days` unless a separate arbitrary-window contract lands |
| `GET /stats/relays/distribution` | dormant relay-distribution product | gated | Slice 8 replacement-semantics comparison and publication |
| `GET /kinds` | all-time kind count, exact/accepted unique-pubkey count, first/last timestamp, and content average | partial | Slice 9.5 general per-kind summary; current `kind_all_time` contains only event count |
| `GET /kinds/{kind}` | general per-kind summary plus exact hourly recent counts | partial | Slice 9.5 kind summary and hourly-count products |
| `GET /kinds/{kind}/activity` | `current_event_daily_kind` plus exact fixed-grain distincts composed for day/week/month | ready | Route comparison, ordering, and limit behavior |

## Slice 9.5 product contract

### Exact hourly event counts

Store sparse rows keyed by `(run_id, hour_epoch, kind_key)`, using `kind_key =
-1` for all kinds. Each row contains an exact canonical event count. The
builder deduplicates by full event ID before aggregation and excludes events
outside the API timestamp domain or beyond the run's fixed `as_of_epoch`.

The product must support:

- exact half-open moving windows ending at `floor_hour(as_of_epoch)`;
- exact 168-hour throughput with and without a kind filter;
- exact hour-of-day counts for supported day windows; and
- exact 24-hour, 7-day, and 30-day kind-detail counts.

### Exact general kind summaries

Store one row for every represented kind with:

- canonical event count;
- exact distinct-pubkey count or an explicitly accepted approximation;
- first eligible timestamp;
- last eligible timestamp;
- checked total UTF-8 content bytes; and
- content-bearing event count used as the average denominator.

The current v1 event-fact artifact stores only event ID, timestamp, and kind;
the fixed-activity artifact adds pubkey but still omits content length. Neither
can prove the content average. The implementation therefore needs a versioned
enriched fact build or an equivalent bounded external join. Duplicate event
IDs must be removed before count and content accumulation.

### Latest sealed event watermark

The ingestion pipeline atomically publishes a tiny versioned document only
after a segment is durably sealed and accepted by the canonical archive path.
It records at least the segment identity, maximum eligible event timestamp,
publication timestamp, and schema version. The API reads a fully published
document and reports its observed age; a partial file or stale/malformed
identity fails closed. This watermark never changes an analytics run and must
not make a partial analytics generation current.

## Cutover evidence

For every row in the matrix, the Slice 10 gate records:

- authenticated request parameters and response SHA-256 for both backends;
- selected Postgres run, snapshot, query version, product IDs, and time
  boundary;
- exact-match, accepted-approximation, or intentional-correction status;
- absolute and relative difference where applicable;
- Postgres latency and bounded sketch-union memory where applicable; and
- rollback confirmation that changing one endpoint-family selector restores
  ClickHouse without changing ingestion or analytics publication.

ClickHouse retirement is not authorized until every matrix row passes and no
operational verifier or accepted route still queries it.
