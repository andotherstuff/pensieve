# Analytics Slice A — Builder and Publication Runbook

*Status: implemented for local/shadow validation; not accepted for API cutover*
*Last updated: 2026-07-28*

This is the first executable DuckDB/Postgres analytics slice described by the
[analytics endpoint migration ledger](analytics_endpoint_migration.md). It
does not change `pensieve-serve`, Grafana, ClickHouse, ingestion, or the
canonical Parquet lake.

## What it builds

`pensieve-analytics` consumes one explicitly selected, canonically encoded
active-raw snapshot. It resolves the snapshot's immutable object keys, scans
only `id`, `created_at`, `kind`, and `sig`, and materializes one deterministic
event row per ID in a persistent DuckDB work database. When an ID occurs more
than once, the lexicographically smallest raw signature wins, matching the
canonical V1 rule.

One full rebuild creates:

- `rollup_overview`: logical total, API-domain total, earliest/latest event,
  rolling seven-day count/rate, and rolling 30-day kind count;
- `rollup_event_daily`: exact logical event counts by UTC date;
- `rollup_event_daily_kind`: exact logical counts by UTC date and kind; and
- `rollup_kind_all_time`: exact logical counts by kind.

The build fails before publication unless:

- the catalog's physical row total is at least the deduplicated logical total;
- both daily products sum to the API-representable logical total; and
- all-time kind counts sum to the complete logical total.

This first version is deliberately a full rebuild. Late historical events are
therefore placed into their correct historical day automatically. Incremental
and affected-period rebuild modes remain future work.

## Frozen Slice A time semantics

These rules are versioned as `slice-a-v1` and are candidates for shadow
comparison, not yet a public API acceptance decision:

- `total_events` and all-time kind counts cover every logical event in the full
  unsigned `created_at` domain, including future and API-unrepresentable rows;
- daily products include `created_at <= u32::MAX` and use UTC epoch-day
  boundaries, including representable future dates;
- earliest event uses that representable domain and retains the existing Nostr
  genesis clamp;
- latest event includes only `created_at <= as_of`;
- seven-day throughput is the inclusive interval
  `[as_of - 604800, as_of]`, capped at `as_of`, divided by exactly 168; and
- rolling 30-day kind count is likewise capped at `as_of`.

Every comparison result must still classify differences from ClickHouse,
especially its inconsistent future-event behavior.

## Build-only canary

Use a fixed `as_of` value so the run is reproducible:

```bash
just analytics-build \
  --catalog /var/lib/pensieve-parquet/catalog/active-raw.json \
  --work-database /var/lib/pensieve-analytics/slice-a.duckdb \
  --as-of 1785232800
```

Omitting `DATABASE_URL` builds and validates DuckDB products without touching
Postgres. The command prints one JSON summary suitable for a run log.

For local/offline fixtures or a locally mounted immutable lake, add:

```bash
--local-object-root /path/to/lake
```

Without that override, the snapshot `store_id` must have the form
`s3+https://<endpoint>/<bucket>`. Object keys become `s3://` URIs. DuckDB loads
its signed `httpfs` and `aws` extensions and obtains credentials only through
the standard AWS environment credential chain. The first S3 run therefore
needs extension-repository network access; later runs use DuckDB's local
extension cache.

For a production-scale snapshot, stage the exact immutable object set before
starting DuckDB. This separates resumable network transfer from the single
large materialization transaction:

```bash
ops/scripts/stage-active-raw-snapshot.sh \
  /var/lib/pensieve-analytics/catalog/active-raw.json \
  /archive/analytics/lake/<snapshot-id> \
  /var/lib/pensieve-analytics/staging/<snapshot-id>
```

The staging wrapper uses the catalog's object-key list, bounded concurrent
`rclone` transfers, and retry/backoff. It then verifies every staged object's
exact byte size and SHA-256 against the snapshot. Rerunning after an interrupted
transfer reuses completed files. Only after `SHA256SUMS` exists should the
analytics build use `--local-object-root` with that staged root.

Direct S3 reads retain defensive DuckDB HTTP retries, but they are a canary
path rather than the preferred production-scale build path.

Set `PENSIEVE_ANALYTICS_DUCKDB_MEMORY_LIMIT` (default `48GB`) below the memory
needed by colocated ClickHouse and ingestion. This is an engine-level limit,
not merely a systemd guard: DuckDB spills earlier instead of driving the host
into swap. Retain a higher systemd `MemoryMax` as a final fail-closed boundary.

## Shadow Postgres publication

Install a private environment file based on `ops/analytics.env.example`, set
the deployed Git commit as `PENSIEVE_ANALYTICS_CODE_VERSION`, and run the same
command with `DATABASE_URL` present.

The binary idempotently applies
`docs/postgres/001_analytics_slice_a.sql`, then opens one Postgres transaction.
It:

1. takes a transaction-scoped advisory publication lock;
2. records the snapshot, prior run, code/query versions, fixed `as_of`, input
   object keys/checksums, row counts, and validation result;
3. streams the four products into run-keyed serving tables with `COPY`;
4. changes the singleton `current_run` pointer; and
5. commits everything together.

Readers use the `pensieve_analytics.current_*` views. They see either the
previous complete run or the next complete run, never a mixture. Retrying the
same deterministic run while it remains current is a no-op. Attempting to make
an older already-published run current again is rejected.

The current client uses a non-TLS Postgres connection. Deploy it against a
colocated Unix socket or trusted localhost. Remote Postgres TLS support must be
added before any networked deployment.

## Shadow gate before API work

Do not point `pensieve-serve` or Grafana at these views yet. The next gate is:

1. build from a small catalog fixture and inspect all relations;
2. build from a real bounded active-file snapshot;
3. publish twice to prove idempotency and atomic pointer movement;
4. compare Slice A endpoint results against ClickHouse with a fixed `as_of`;
5. measure DuckDB work-database size, peak memory, S3 bytes read, build time,
   Postgres relation sizes, and publication time; and
6. record every old/new difference using the ledger classifications.
