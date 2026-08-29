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

| Route | Required product and contract | Current status | Remaining gate |
|---|---|---|---|
| `GET /stats` | `current_overview` plus exact eligible-pubkey total; latest timestamp comes from the ingestion watermark | gated | Activate the implemented watermark and validate one response identity/freshness policy |
| `GET /stats/events/total` | `current_overview.total_events` | ready | Route comparison only; Postgres returns canonical logical events rather than ClickHouse part rows |
| `GET /stats/pubkeys/total` | `current_overview.total_pubkeys` | ready | Route comparison only |
| `GET /stats/kinds/total` | `current_overview.kinds_30d` | ready | Anchor the 30-day boundary to the selected run |
| `GET /stats/events/earliest` | `current_overview.earliest_event` | ready | Preserve the Nostr-genesis clamp in the route adapter |
| `GET /stats/events/latest` | latest sealed canonical event watermark | external | Activate the implemented ingestion-owned watermark and validate its freshness objective |
| `GET /stats/events` | `current_event_daily`, `current_event_daily_kind`, fixed-grain exact distincts, flexible-distinct leaves, and sparse hourly counts | gated | Slice 9.5 production publication and every supported `days`/kind comparison |
| `GET /stats/throughput` | exact sparse hourly event counts over the last 168 complete hours, optionally by kind | gated | Postgres adapter implemented; publish Slice 9.5 and compare the exact 168-hour boundary |
| `GET /stats/users/active` | latest rows from `current_active_users_period` for day/week/month | ready | Route comparison and empty-dataset behavior |
| `GET /stats/users/active/daily` | `current_active_users_period` with `grain='day'` | ready | Ordering, `since`, and limit comparison |
| `GET /stats/users/active/weekly` | `current_active_users_period` with `grain='week'` | ready | Ordering, `since`, and limit comparison |
| `GET /stats/users/active/monthly` | `current_active_users_period` with `grain='month'` | ready | Ordering, `since`, and limit comparison |
| `GET /stats/users/retention` | `current_cohort_retention_period` | ready | Preserve the intentional newest-cohort-first correction and period-zero semantics |
| `GET /stats/users/new` | `current_new_users_daily`, composed to day/week/month | ready | Ordering, `since`, and limit comparison |
| `GET /stats/activity/hourly` | Slice 9.5 hourly event counts plus flexible-distinct leaf unions grouped by UTC hour-of-day | gated | Production publication and accepted sketch comparison for every `days`/kind shape |
| `GET /stats/zaps` | semantic zap daily rows plus zap-distinct sender/recipient leaves | gated | Postgres adapter implemented; accept both products in one recurring run and compare every aggregate/grouped shape |
| `GET /stats/zaps/histogram` | semantic 17-bucket daily count/amount rows | gated | Postgres adapter and deterministic rounding implemented; accept the product and compare bucket boundaries |
| `GET /stats/engagement` | semantic engagement daily rows | gated | Postgres adapter implemented; accept the semantic product and compare positional-tag semantics |
| `GET /stats/longform` | semantic long-form daily rows, exact all-time kind-30023 authors from the enriched kind summary, and kind-30023 flexible-distinct leaves for bounded windows | gated | Postgres adapter implemented; accept all three products in one run and compare bounded/all-time shapes |
| `GET /stats/publishers` | exact predefined-window publisher ranking rows | gated | Slice 9 benchmark/build/publication; reject unsupported `days` unless a separate arbitrary-window contract lands |
| `GET /stats/relays/distribution` | dormant relay-distribution product | gated | Slice 8 replacement-semantics comparison and publication |
| `GET /kinds` | all-time kind count, exact/accepted unique-pubkey count, first/last timestamp, and content average | gated | Postgres adapter implemented; publish and compare the enriched per-kind summaries |
| `GET /kinds/{kind}` | enriched per-kind summary plus exact hourly recent counts | gated | Postgres adapter implemented; publish and compare all-time and complete-hour windows |
| `GET /kinds/{kind}/activity` | `current_event_daily_kind` plus exact fixed-grain distincts composed for day/week/month | ready | Postgres adapter implemented; compare ordering, limits, and every grain |

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

The all-time unique-pubkey field is exact. In particular,
`GET /stats/longform` reads the kind-30023 row for its unbounded author total.
A bounded `days=N` long-form request instead unions the accepted Slice 6
kind-30023 leaves over the published complete-hour window and therefore keeps
the approximation contract and error evidence of that product. The route must
not substitute the bounded sketch for the exact all-time value or reuse the
all-time value for a bounded request.

The serving-facts implementation now joins the exact event-fact anchor with a
bounded, event-ID-keyed UTF-8 content-length stream. Its fixed-width enriched
artifact proves the content average without retaining raw content. Duplicate
event IDs are removed before count and content accumulation. Production build,
publication, and recurring-successor evidence remain gated.

The independent comparison runner samples sparse, midpoint-density, and dense
all-kind hours, per-kind hours, and all-time kinds. It performs bounded
fixed-as-of reads from `events_local FINAL`; count-driven deltas retain the
documented cross-store population classification, while same-count differences
in exact publisher, timestamp, or UTF-8 content metrics fail closed.

### Latest sealed event watermark

The ingestion pipeline atomically publishes a tiny versioned document only
after a segment is durably sealed and accepted by the canonical archive path.
It records at least the segment identity, maximum eligible event timestamp,
publication timestamp, and schema version. The API reads a fully published
document and reports its observed age; a partial file or stale/malformed
identity fails closed. This watermark never changes an analytics run and must
not make a partial analytics generation current.

## Slice 9.5 implementation status

- [x] Canonical fixed-width enriched facts with exact event-ID/content join.
- [x] Versioned append-only event-fact bootstrap from a fully verified earlier
  artifact, with exact catalog ancestry and predecessor-evidence lineage.
- [x] Sparse complete-hour all-kind and per-kind event counts.
- [x] Exact enriched all-time per-kind summaries and content-byte denominator.
- [x] Retry-safe dormant Postgres publication through migration 010.
- [x] All-product recurring transaction and append-only successor integration.
- [x] Atomic ingestion-owned latest sealed-event watermark implementation.
- [x] Constant-memory fixed-as-of ClickHouse comparison harness for exact
  hourly counts and enriched all-time kind summaries.
- [ ] Frozen production event-fact and serving-fact builds.
- [ ] Dormant publication with no current-pointer change.
- [ ] Watermark activation and freshness validation.
- [ ] One moving recurring all-product publication and 24-route comparison.

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
