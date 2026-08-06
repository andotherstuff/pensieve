# Analytics Slice A — Builder and Publication Runbook

*Status: deployed as shadow analytics; not accepted for API cutover*
*Last updated: 2026-08-04*

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

The initial materializer remains available as a full-rebuild path. The
incremental executor handles append-only catalogs with exact cross-delta ID
matching and places late historical events into their correct historical day.
Catalog removals still require the future affected-period executor and fail
closed rather than silently applying an append-only update.

## Plan the next catalog delta

Planning reads only the selected catalog and Postgres metadata; it does not
download or scan Parquet objects:

```bash
just analytics-plan \
  --catalog /var/lib/pensieve-parquet/catalog/active-raw.json
```

The plan compares immutable key, work-unit, checksum, byte-size, and row-count
identity against `pensieve_analytics.applied_objects`. Existing deployments
without ledger rows fall back to the current run's `run_inputs`; retrying the
current completed publication populates the explicit ledger atomically.

An append-only catalog produces an `incremental` plan containing only new
objects and their exact bytes/rows. Removed objects produce an
`affected_period_rebuild` plan. Reuse of an immutable key with changed identity
is rejected. Object timestamp bounds describe the historical range touched by
the delta; `affected_range_complete = false` prevents a later executor from
assuming a bounded rebuild when legacy ledger rows lack those bounds.

Stage an accepted incremental plan into a snapshot-specific cache with explicit
object and byte ceilings:

```bash
MAX_STAGE_OBJECTS=1000 MAX_STAGE_BYTES=107374182400 \
  ops/scripts/stage-analytics-delta.sh \
  /var/lib/pensieve-analytics/plans/<snapshot-id>.json \
  /archive/analytics/deltas/<snapshot-id> \
  /var/lib/pensieve-analytics/staging/<snapshot-id>
```

The wrapper rejects non-incremental plans, removals, empty deltas, inconsistent
byte accounting, or limits exceeded before contacting object storage. It then
downloads only `added_objects`, verifies every size and SHA-256, rejects extra
local files, and writes a checksummed completion receipt. A dedicated local
root per plan makes retries resumable without allowing files from another
snapshot to satisfy verification.

## Apply one verified incremental delta

Use one fixed `as_of` for dry-run and application. The dry-run re-verifies all
staged SHA-256 values, deduplicates the delta, scans the existing event-ID
checkpoint for matches, validates committed fields, and rolls its DuckDB
transaction back:

```bash
just analytics-incremental \
  --catalog /var/lib/pensieve-analytics/catalog/<target>.json \
  --plan /var/lib/pensieve-analytics/plans/<target>.json \
  --work-database /archive/analytics/slice-a.duckdb \
  --delta-object-root /archive/analytics/deltas/<target> \
  --as-of 1785840000 \
  --dry-run
```

Remove `--dry-run` only after its counts and resource measurements are
accepted. The executor holds the Postgres publication lock from re-planning
through DuckDB completion, so another publisher cannot invalidate its
baseline. Persistent DuckDB changes are one transaction: exact new event IDs,
additive daily/kind rollups, the rolling overview, and checkpoint metadata
commit together. Postgres then records a complete `incremental` run and moves
the serving pointer atomically. A Postgres failure leaves the completed DuckDB
checkpoint reusable; rerunning with the same catalog, plan, `as_of`, and code
version skips the already-applied delta and retries publication.

The current implementation performs one sequential scan of `canonical_events`
per delta to match IDs and another to refresh rolling overview metrics. Measure
both on production before selecting a schedule. It never scans unchanged
Parquet objects.

## Recurring production refresh

`ops/scripts/run-analytics-refresh.sh` composes catalog export/advance,
content-addressed catalog publication, metadata-only planning, bounded delta
staging, a copy-on-write checkpoint backup, incremental execution, atomic
Postgres publication, generation-pointer advancement, and retention. The
systemd service applies the production-measured limits of two DuckDB threads,
a 16 GiB engine limit, a 20 GiB cgroup soft limit, a 24 GiB hard limit, and
reduced I/O weight.

The generation symlink moves only after Postgres publication succeeds. If the
process dies after publication but before that move, the next run advances from
the older selected generation, replans against the newer Postgres ledger, and
converges without reapplying published objects. A plan with removals or another
unsupported run kind fails and preserves all evidence for operator action.

The default timer runs once per day because each delta still scans the durable
event-ID checkpoint and rolling overview. This cadence keeps staged downloads
small without pretending the Parquet-only shadow can satisfy the current API's
near-real-time latest-event contract. The newest three DuckDB backups and two
verified local delta caches are retained; compact run evidence is not pruned.

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
Set `PENSIEVE_ANALYTICS_DUCKDB_THREADS` (default `4`) as an engine-level worker
limit; lowering worker concurrency also reduces per-query memory reservations.

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
same deterministic run while it remains current leaves serving data unchanged
and idempotently repairs the applied-object ledger. Attempting to make an older
already-published run current again is rejected.

If DuckDB finishes but Postgres connection or publication fails, preserve the
work database and retry with `--reuse-completed-build`. This mode revalidates
all reconciled products against the exact catalog before publication and never
reruns Parquet materialization. Supply `POSTGRES_ANALYTICS_PASSWORD` from a
private environment file rather than embedding it in `DATABASE_URL`.

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

## Fixed-as-of comparison harness

`pensieve-analytics-compare` is a read-only parity probe for the Slice A
products already published to Postgres. It reads the current immutable
Postgres run, takes that run's `as_of_epoch`, and reads `events_local` in
ordered event-ID ranges. Within each range, `FINAL` collapses duplicate rows by
event ID at one fixed ingestion barrier. It compares:

- API-representable total, earliest, fixed-`as_of` latest, rolling seven-day
  events, and rolling 30-day kinds;
- daily event counts for a bounded set of complete UTC days;
- daily-kind counts over the same complete-day window; and
- per-kind totals through the last complete UTC day.

The partial UTC day containing `as_of` is deliberately excluded from daily
series. Slice A's daily tables contain complete event-date buckets, while an
endpoint comparison capped partway through that day would require an hourly or
live-delta product.

Run it with the same private environment used by the analytics publisher:

```bash
run_id="$(
  psql "$DATABASE_URL" -Atc \
    'SELECT run_id FROM pensieve_analytics.current_run_metadata'
)"
report="/var/lib/pensieve-analytics/comparisons/$run_id.json"
checkpoints="$report.checkpoints"

pensieve-analytics-compare \
  --output "$report" \
  --postgres-run-id "$run_id" \
  --completed-days 30 \
  --clickhouse-shards 256 \
  --clickhouse-checkpoint-dir "$checkpoints" \
  --clickhouse-shard-delay-seconds 30 \
  --clickhouse-max-threads 1 \
  --clickhouse-max-memory-usage 8589934592
```

The output is immutable evidence: publication uses a same-filesystem hard
link, refuses to replace an existing report, and stores no connection strings
or passwords. Every completed shard is also published as immutable JSON and
is validated against the database, table, Postgres snapshot, `as_of`, ingestion
barrier, day range, harness version, and exact ID bounds before reuse. An
interrupted invocation therefore resumes completed work and refuses stale or
cross-snapshot checkpoints. Pin `--postgres-run-id` for any run expected to
cross a daily publication boundary.

The default 256 shards cover the complete ClickHouse string keyspace without
gaps. ID predicates use the `events_local` primary key, each query is bounded,
and partial aggregates merge exactly: counts add, minima/maxima merge, and the
rolling kind set unions. The first query freezes `indexed_at` at comparison
start unless stronger alignment evidence supplies an attested barrier. Each
shard performs one all-time kind/scalar pass and one bounded daily-kind pass,
avoiding the former all-history `(day, kind)` result. The final report is only
published after every shard is present. Run production probes with one thread,
resource controls, a delay between new shards, and ingestion health under
observation.

A fixed ClickHouse ingestion barrier prevents shards from seeing newly indexed
events at different times, but does not prove that Postgres and ClickHouse had
the same input IDs. Without independent input-set evidence, an unequal value
is classified `old_stack_uncertainty` and the report gate is `incomplete`, even
if every compared value happens to match. Exit status `2` means the report is
valid but cannot approve parity.

To make mismatches actionable, provide JSON evidence from an exact ID-keyed
Parquet/ClickHouse barrier comparison:

```json
{
  "schema_version": 1,
  "evidence_type": "pensieve-clickhouse-parquet-id-parity-v1",
  "status": "passed",
  "snapshot_id": "sha256:<current-snapshot>",
  "clickhouse_database": "nostr",
  "clickhouse_table": "events_local",
  "clickhouse_indexed_at_max_epoch": 1786000000,
  "id_keyed_equal": true
}
```

Then invoke the harness with `--input-alignment-proven
--alignment-evidence <file>`. Every ClickHouse query applies the attested
`indexed_at` barrier. With aligned inputs, exact matches produce exit status
`0`; any scalar or keyed difference is classified `bug`, the gate is
`failed`, and the command exits `2`. Connection, query, or report failures
exit `1`.
