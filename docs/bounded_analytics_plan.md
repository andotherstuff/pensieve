# Bounded Analytics Migration Plan

*Status: Slices 0-4 implemented; Slice 4 ready for an operator-authorized production canary*
*Last updated: 2026-08-23*

This document turns the
[analytics endpoint migration ledger](analytics_endpoint_migration.md) into an
execution plan whose peak memory is independent of the total archive size. It
supersedes any production-scale full rebuild based on one global DuckDB
`DISTINCT` or population-sized hash aggregation.

The migration remains archive-first:

- canonical Parquet is the durable source of truth;
- large transformations run offline, never on the API request path;
- high-cardinality exact state lives in immutable files on `/archive`, not in
  Postgres;
- Postgres contains only compact serving products and an atomic current-run
  pointer; and
- the previous successful generation remains current until every validation
  and reconciliation gate for its replacement passes.

## 1. Why this plan is necessary

The first Slice B1 implementation added `pubkey` to `canonical_events` and
rebuilt it with a global:

```sql
SELECT DISTINCT id, pubkey, created_at, kind
FROM read_parquet(...)
```

On the frozen production snapshot this retained cardinality-sized state rather
than spilling usefully. A 20 GB DuckDB limit failed, and a 40 GB retry showed
the same linear growth pattern before it was stopped. Neither attempt
published Postgres state or changed the current analytics generation.

For a fixed snapshot, that query is technically bounded by the number of
logical events. Operationally it is unbounded: its memory requirement grows
with the archive. Raising the cgroup ceiling only postpones the same failure.

The replacement is a reusable external-aggregation engine. It limits in-memory
work by bytes and merge fan-in, writes immutable sorted runs, and finalizes
products with streaming merges.

## 2. Non-negotiable invariants

Every implementation slice must preserve these properties.

### 2.1 Bounded memory

Peak resident work must satisfy a measurable bound of the form:

```text
peak memory <= input batch budget
             + merge fan-in * per-run buffer
             + fixed product overhead
```

The bound must not contain total events, total pubkeys, or total input objects.
Increasing a fixture from one million to ten million identities may increase
runtime and output bytes, but must not increase peak memory beyond the declared
tolerance.

### 2.2 Explicit state growth

Bounded RAM does not imply fixed disk usage. Exact distinct and latest-by-key
products must preserve information proportional to their key population.
Their state may grow on disk behind an explicit capacity model.

Each product is classified as one of:

- **constant state**: scalars, fixed histograms, and bounded counters;
- **time/key state**: daily/hourly/kind rows whose size follows supported
  periods and key domains;
- **exact identity state**: sorted records proportional to distinct keys;
- **fixed sketch state**: explicitly approximate mergeable summaries; or
- **current replacement state**: one latest row per logical key.

No exact high-cardinality set may be hidden in an in-memory hash table merely
because the current host happens to fit it.

### 2.3 Immutable and resumable work

Every batch and merge output records:

- schema and runner version;
- selected snapshot ID and fixed `as_of`;
- product and key-space identity;
- exact input object/run identities;
- row count and byte size;
- minimum and maximum key when applicable; and
- SHA-256 of the completed immutable file.

Temporary files use a `.partial` suffix and never satisfy a checkpoint. A
completed run is reused only when all identities and checksums match. Inputs
and failed evidence remain intact until a verified successor is published and
the retention policy explicitly permits cleanup.

### 2.4 Fail-closed publication

Builders do not update Postgres incrementally while scanning. They first
finish and validate immutable products. Postgres staging rows, run metadata,
and the current pointer then change in one transaction.

Ingestion is never restarted for analytics work. Production jobs use explicit
CPU, I/O, memory, disk, and writable-path limits. The refresh timer may be
paused during a checkpoint-format cutover, but must be restored only after the
new checkpoint and publication are independently verified.

### 2.5 Exactness is a product contract

Counts, sums, minima, maxima, and deterministic classifications are exact on
the canonical event-ID domain. A distinct or ranking field is exact only when
the plan retains sufficient exact state. Sketch-backed fields declare their
algorithm, parameters, serialization version, tolerance, and merge behavior.

Approximation is never introduced silently to make a build fit.

## 3. Shared bounded execution engine

### 3.1 Freeze the input

One run selects a canonical active-file snapshot and fixed `as_of`. Object
paths, sizes, row counts, and SHA-256 values are frozen before processing.
Only verified local objects or authenticated immutable object-store reads may
satisfy the manifest.

### 3.2 Generate sorted runs

The runner selects input by byte and row ceilings rather than only by object
count. For each batch it:

1. projects the minimum fields required by one product lane;
2. applies row-local parsing or classification;
3. sorts by the lane's merge key within the declared memory budget;
4. writes one immutable run file; and
5. validates and checkpoints the run before advancing.

Examples of lane keys are:

- event canonicalization: `(event_id)`;
- pubkey first seen: `(pubkey, created_at)`;
- activity identity: `(grain, period, pubkey)`;
- kind identity: `(period, kind, pubkey)`;
- current relay list: `(pubkey, created_at, event_id)`; and
- publisher facts: `(window_day, pubkey, kind)`.

### 3.3 Merge with bounded fan-in

A k-way merge keeps only one small buffer per input run. If run count exceeds
the configured fan-in, levelled compaction produces intermediate immutable
runs. Adjacent equal keys are reduced using the product's associative rule:

- event ID: choose one canonical row and reject committed-field conflicts;
- first seen: minimum timestamp;
- additive facts: checked sum;
- identity facts: set union by adjacent-key suppression;
- latest state: maximum `(created_at, event_id)`; or
- flags: bitwise OR.

The engine must not assume that DuckDB will spill a non-spillable operator.
DuckDB may still scan, project, parse, and sort a bounded batch; Rust owns the
checkpoint and streaming merge lifecycle.

### 3.4 Finalize compact serving products

Final merges stream into small relations such as daily counts, kind summaries,
active-user periods, cohort matrices, or top-K output. High-cardinality exact
state remains in versioned files and is not copied into every Postgres run.

### 3.5 Incremental maintenance

New immutable Parquet objects generate new level-zero runs. Product rules then
determine the smallest safe merge:

- additive rows merge by key;
- a late first-seen value may move one pubkey to an older bucket;
- identity state unions new keys into touched periods;
- latest-by-key state replaces an older winner; and
- unsupported removals or semantic-version changes force an affected or full
  rebuild rather than pretending to be append-only.

Compaction is scheduled by run count and bytes. It is not required for every
publication if the bounded merge fan-in remains satisfied.

## 4. Metric execution ledger

This section covers all ClickHouse-backed route families. The operational relay
routes remain in the ingester's SQLite database and do not move through this
pipeline.

| Endpoint family / fields | State class | Bounded computation | Recommended contract |
|---|---|---|---|
| Overview: total events | Exact identity | External sort/merge by event ID, then count canonical rows | Exact |
| Overview: earliest/latest | Constant | Streaming min/max over canonical timestamps with the fixed API domain and `as_of` | Exact |
| Overview: total/rolling kinds | Constant | A 65,536-kind bitset or compact kind counters | Exact |
| Event counts by hour/day/week/month/kind | Time/key | Add canonical events into checked hourly/daily counters; compose larger periods | Exact |
| Seven-day throughput | Time/key | Sum fixed hourly buckets over the exact 168-hour interval | Exact |
| Event unique pubkeys for fixed day/week/month grains | Exact identity | External distinct keyed by `(grain, period, optional_kind, pubkey)` | Exact |
| Event unique pubkeys for arbitrary `since`/`until`/`days` windows | Fixed sketch | Merge versioned daily HLL/Theta sketches for the selected window | Approximate with accepted tolerance |
| Kind list/detail counts, first/last, content average | Time/key | Counts, min/max, content-byte sum, and content count keyed by kind | Exact |
| Kind all-time/fixed-period unique pubkeys | Exact identity | External distinct keyed by `(kind, period, pubkey)` | Exact |
| Pubkey total, first seen, and new users | Exact identity | Sort by pubkey and stream minimum eligible timestamp; aggregate finalized minima by day | Exact |
| DAU/WAU/MAU and profile/follows subsets | Exact identity | External distinct activity keys; exact per-pubkey kind-0/kind-3 flags; sort-merge join | Exact |
| Cohort retention | Exact identity | Sort-merge first-seen cohorts with activity periods, then count distinct pubkeys per pair | Exact |
| Hour-of-day activity counts | Time/key | Additive `(date, hour, optional_kind)` counters | Exact |
| Hour-of-day unique pubkeys over arbitrary day windows | Fixed sketch | Merge daily hour/kind identity sketches | Approximate with accepted tolerance |
| Zap total/count/average | Time/key | Parse one canonical zap fact per event ID; retain daily count and millisatoshi sum | Exact |
| Zap unique senders/recipients for arbitrary windows | Fixed sketch | Merge daily sender and recipient sketches | Approximate, matching the current `uniq`-style contract |
| Zap histogram and percentages | Constant/time | Seventeen fixed daily buckets of count and millisatoshi sum; derive percentages | Exact inputs and deterministic rounding |
| Engagement replies/reactions/original notes | Time/key | Row-local tag classification followed by daily additive counters | Exact |
| Long-form count, content length, and estimated words | Time/key | Daily count and content-byte sum for kind 30023 | Exact |
| Long-form unique authors | Exact identity or fixed sketch | Exact all-time external distinct; mergeable daily sketch for arbitrary windows | Exact all-time, approximate arbitrary window |
| NIP-65 relay distribution | Current replacement | External maximum `(created_at, event_id)` per pubkey, then normalize/expand only winning tags | Exact |
| Publisher ranking and its kind count/min/max | Exact identity or heavy-hitter sketch | External aggregate by pubkey for predefined windows, bounded final top-K heap; benchmark sketches for arbitrary windows | Exact predefined windows; arbitrary-window contract pending |
| Latest-event freshness | Constant/live | Separate ingestion or sealed-Parquet watermark, not a daily full analytics build | Exact with an explicit freshness objective |

### 4.1 Why some distinct fields are approximate

An arbitrary-window exact distinct count cannot be represented by a fixed-size
summary for every possible window. The choices are:

1. retain exact identities on disk and perform a bounded but potentially slow
   merge for each request;
2. precompute every supported window, which multiplies storage; or
3. merge fixed-size sketches and accept a measured error bound.

The current API already uses approximate `uniq` for several flexible-window
fields. The recommended migration keeps exact results for fixed published
grains and uses versioned sketches only for flexible windows. Postgres stores
final counts; sketch union remains builder-side unless a separately tested
serving requirement justifies serialized sketches in Postgres.

### 4.2 Publisher ranking requires a contract decision

Daily top-K lists are not mergeable into an exact multi-day top-K. A publisher
outside every daily top-K may still rank globally across a longer window.

The preferred exact contract is a finite set of windows such as 1, 7, 30, 90,
and 365 days. For each window, external aggregation produces one exact row per
publisher and a final heap retains only the requested top 1,000. Supporting
arbitrary `days=N` requires either a background cached job over exact daily
publisher facts or an explicitly approximate heavy-hitter structure. This is
decided from production size and latency benchmarks before route cutover.

## 5. Recommended semantic decisions

Unless comparison evidence requires otherwise, implementation proceeds with
these defaults:

1. one fixed `as_of` per analytics publication;
2. UTC boundaries, inclusive `since`, and exclusive `until` where the current
   route specifies them;
3. consistent exclusion of future timestamps from rolling/current metrics;
4. exact additive and fixed-grain identity products;
5. versioned mergeable sketches only for arbitrary-window distinct fields;
6. no silent zero when a serving query or product is unavailable;
7. a separate live watermark for `/stats/events/latest`; and
8. predefined exact publisher windows unless benchmarks justify another
   contract.

Intentional corrections to ClickHouse behavior require an entry in the
comparison ledger and an endpoint test. They are not smuggled into an engine
refactor.

## 6. Delivery slices and commit boundaries

Each slice is independently reviewable and bisectable. A slice may use more
than one commit when tests and implementation are clearer separately, but no
commit combines unrelated metric families or production cutover work.

### Slice 0 — restore the safe production baseline

Goal: separate unfinished identity work from the deployed Slice A checkpoint.

- restore the `slice-a-v1` checkpoint/query contract for the normal refresh;
- keep unaccepted B1 relations unreachable from the API;
- deploy and verify normal incremental Slice A refresh;
- resume the refresh timer; and
- retain all failed B1 databases and journals as evidence.

Gate: current generation advances normally, ingestion remains active with an
unchanged restart count, and no B1 publication is current.

Completed 2026-08-14 in commit `f0ac995`:

- restored the normal computation and publication contract to `slice-a-v1`
  while retaining the fail-closed query-version planner guard;
- kept the unaccepted B1 Postgres relations dormant and preserved all failed
  B1 DuckDB databases as evidence;
- deployed only the analytics planner and incremental binaries, without
  restarting ingestion or the API;
- validated a manual incremental publication from snapshot
  `sha256:1cf964f3e9368e3d5569c49254e9f7d258b9f6f59fb5f5e853371572c470caaf`
  to
  `sha256:3a5b90356854ebecec360035dbb9416b48098c0005629953b1831db562ec0b51`,
  including run checksums, exact Postgres row accounting, and the atomic
  current-generation pointer;
- re-enabled the persistent timer and validated its immediate catch-up
  publication from
  `sha256:3a5b90356854ebecec360035dbb9416b48098c0005629953b1831db562ec0b51`
  to
  `sha256:718846ca4a7a3c04db634478f76cbebe5006186a2718baf65e3de0e30ddd95df`;
- confirmed the timer is waiting for its next daily activation, Postgres is
  current on `slice-a-v1`, and ingestion remained active with zero restarts.

Canonical run evidence is retained under:

- `/var/lib/pensieve-analytics/refresh/runs/20260814T083243Z-3879967`; and
- `/var/lib/pensieve-analytics/refresh/runs/20260814T085859Z-3887760`.

### Slice 1 — bounded run/checkpoint primitives

Goal: implement the reusable infrastructure without changing any metric.

- byte/row-bounded batch planner;
- immutable run metadata and canonical JSON evidence;
- atomic `.partial` to completed publication;
- SHA-256 validation and exact identity reuse checks;
- fixed-fan-in streaming merge;
- levelled compaction planning;
- disk preflight and cleanup eligibility; and
- interruption/resume tests at every checkpoint boundary.

Gate: synthetic input growth of at least 10x stays inside the declared peak
memory tolerance, and repeated/resumed output is byte-identical.

Completed 2026-08-15:

- added canonical immutable run checkpoints with exact snapshot, `as_of`,
  product, key-space, input, output, byte, row, range, and SHA-256 identity;
- made `.partial` artifact and checkpoint publication resumable across every
  durability boundary without permitting identity reuse or replacement;
- added deterministic byte/row batch planning and explicit oversized-object
  accounting;
- added conflict-detecting fixed-width streaming merge with one buffered
  record per input and deterministic duplicate suppression;
- added deterministic fixed-fan-in levelled compaction planning, live
  filesystem capacity preflight, and conservative cleanup eligibility gates;
- proved byte-identical output across input order, fan-in, merge-tree, retry,
  and checkpoint-resume changes; and
- exercised 100, 1,000, and 10,000-record fixtures with a constant 40-byte
  encoded merge-buffer peak at fan-in four, including the fixed output record.

### Slice 2 — bounded canonical event facts

Goal: replace full-rebuild global event-ID `DISTINCT`.

- emit compact event facts sorted by ID from bounded batches;
- stream-merge duplicate IDs;
- fail on committed-field conflicts;
- reproduce Slice A total/daily/kind products; and
- compare byte-for-byte metric output with the accepted Slice A checkpoint.

Gate: exact physical/logical/duplicate accounting and flat memory on a frozen
production canary. Existing incremental publication remains available until
the replacement checkpoint is proven.

Implementation completed 2026-08-18; the production canary remains the final
gate:

- emits 42-byte `(event_id, created_at, kind)` facts from byte/row-bounded
  DuckDB scans, using either authenticated immutable object-store reads or
  local objects whose catalog size and SHA-256 are reverified;
- checkpoints every batch and fixed-fan-in merge immutably, resumes exact
  completed work, suppresses byte-identical IDs, and fails on committed-field
  conflicts;
- reconciles physical rows exactly into logical events plus batch/merge
  duplicates and validates final fixed-width file accounting;
- finalizes Slice A overview, daily, daily-kind, and all-time-kind products with
  event-count-independent scalar/kind state plus explicit time/key counters;
- performs a conservative disk preflight that includes retained intermediate
  runs and an operator-selected free-space reserve;
- produces canonical build and optional reference-comparison evidence; and
- passes byte-identical Slice A fixtures across different batch/merge trees,
  exact resume, empty snapshots, edge timestamps, and 100x fixed-domain scale.

The canary command is `pensieve-analytics-event-facts`. It writes to dedicated
work/evidence/database paths and cannot change the live incremental checkpoint
or Postgres unless a later, separately authorized publication step uses its
completed `AnalyticsBuild`.

### Slice 3 — B1 first-seen and new users

Goal: publish the first identity products without rebuilding canonical events
in memory.

- eligible per-pubkey first seen;
- total pubkeys;
- daily new users;
- late historical first-seen movement;
- Postgres serving rows and atomic publication; and
- exact ClickHouse/frozen-ID reconciliation.

Gate: finalized daily rows sum to eligible pubkeys, all run/checkpoint hashes
verify, and the production memory envelope remains flat.

Implementation completed on the Slice 3 branch:

- full initialization scans byte/row-bounded object batches and produces an
  immutable sorted `(pubkey, first_seen)` artifact;
- append-only refreshes scan only verified new objects and streaming-min merge
  them with the prior artifact, including exact late historical movement;
- publication revalidates the artifact bytes, SHA-256, ordering, row counts,
  daily metrics, snapshot, and fixed `as_of` before opening Postgres staging;
- Slice A and B1 metadata, overview totals, daily serving rows, validation
  hashes, object ledger, and current-run pointer commit atomically;
- deterministic run identity includes the B1 evidence, metric, and artifact
  hashes, making a completed build safely retryable; and
- the recurring refresh remains on `slice-a-v1` unless the operator explicitly
  enables the `slice-b1-v1` lane after the initial publication gate.

Production canary, fixed-snapshot ClickHouse reconciliation, and API cutover
remain required before Slice 3 is declared live.

The production gate uses two explicit commands. First,
`pensieve-analytics-identity-publish --dry-run` builds and revalidates the
immutable B1 artifact without opening a Postgres publication transaction. The
same command without `--dry-run` is reserved for the later authorized pointer
change and requires the current Postgres Slice A snapshot and `as_of` to still
match exactly. Second, `pensieve-analytics-identity-compare` compares the
candidate by pubkey-prefix shard and UTC first-seen day with ClickHouse's exact
`minMerge(first_seen_state)` semantics in one read-only query. It records that
ClickHouse is a continuously advancing head; a mismatch is not accepted as a
candidate bug or ignored as harmless until head-lag attribution or exact
event-ID alignment explains it.

If Slice A advances after a successful B1 canary, the identity publisher can
accept the predecessor evidence together with the exact persisted append-only
delta plan and verified delta-object root. It scans only the new objects,
streaming-min merges them with the predecessor artifact, and still requires the
successor snapshot and `as_of` to match the current Postgres baseline before
publication.

### Slice 4 — fixed-grain distinct and active users

Goal: exact identity products for published periods.

- daily/weekly/monthly event and kind unique pubkeys;
- exact active-user populations;
- exact ever-observed profile/follows flags;
- current DAU/WAU/MAU summary; and
- fixed-grain serving tables.

Gate: set-union fixtures prove weekly/monthly counts are not sums of daily
counts, and all excluded-kind/time-domain rules reconcile.

Implementation foundation completed on the Slice 4 branch:

- one immutable record per canonical `(pubkey, UTC day, kind, event ID)` keeps
  cross-object duplicate suppression exact while retaining the identities
  needed for every fixed grain;
- byte/row-bounded DuckDB batches and fixed-fan-in streaming merges make peak
  merge memory independent of archive cardinality;
- a streaming finalizer derives exact daily, calendar-week, and
  calendar-month all-kind and per-kind distinct populations without summing
  daily distinct counts;
- exact ever-observed kind-0/profile and kind-3/follows flags are retained in
  a separate immutable pubkey artifact and joined without a population-sized
  in-memory map;
- active-user products consistently exclude kinds 445 and 1059 while event
  distinct products continue to represent all API-domain kinds; and
- append-only successors scan only verified new objects, union identities with
  the predecessor artifact, and revalidate every artifact and serving metric;
- versioned Postgres relations expose all-kind/per-kind distinct populations
  and active-user period rows behind the same atomic current-run pointer as
  Slice A and first-seen products; and
- publication revalidates both high-cardinality artifacts, binds their hashes
  into the deterministic run ID, checks copied row counts and sums, and rolls
  back the entire transaction on any staging failure.

The recurring lane now has a fail-closed, opt-in Slice B2 path. When
`PENSIEVE_ANALYTICS_ACTIVITY_ENABLED=1`, it also requires the B1 lane, requires
both evidence files in the current generation, plans against `slice-b2-v1`,
advances each product from the same verified delta, and publishes Slice A, B1,
and B2 in one transaction. A failed build or publication leaves the prior
generation current.

The one-time canary command is `pensieve-analytics-activity-publish`. It builds
and validates immutable activity state with one DuckDB worker and a 4 GB
default scan limit, requires first-seen evidence for the same snapshot and
`as_of`, and refuses publication unless Postgres is still on that exact B1
run. Run it first with `--dry-run`; after a successful real publication,
install its `activity-evidence.json` in the current analytics generation before
enabling the recurring B2 flag. Production canary execution, evidence
installation, and route cutover remain separate operator-authorized steps.

### Slice 5 — cohort retention

Goal: exact bounded-memory retention from Slice 3 and Slice 4 state.

- weekly and monthly cohort assignments;
- activity-period join;
- compact cohort/activity count matrix;
- ordering and limit contract tests; and
- late first-seen cohort movement.

Gate: cohort size, period-zero behavior, percentages, and old/new ordering are
explicitly classified and reconciled.

### Slice 6 — flexible-window distinct sketches

Goal: serve dynamic event, kind, hourly, zap, and long-form identity counts.

- select one sketch library and serialization version;
- deterministic build and union tests;
- daily/hour/kind sketch products as required;
- absolute/relative tolerance evidence against exact samples; and
- builder-side merge into Postgres final counts.

Gate: errors remain within the accepted per-field tolerance across sparse,
dense, adversarial, and repeated rebuild fixtures.

### Slice 7 — additive semantic products

Goal: migrate transformations that do not require high-cardinality identity
state.

- engagement tag classification;
- long-form byte-length/count rules;
- zap parsing, count/sum/average, and fixed histogram buckets; and
- Unicode, malformed-tag, amount-boundary, and zero-denominator fixtures.

Gate: parsed fact counts reconcile to canonical input IDs and every semantic
difference from ClickHouse is classified.

### Slice 8 — current NIP-65 relay distribution

Goal: exact latest-by-pubkey replacement semantics.

- deterministic latest event selection;
- shared URL normalization fixtures;
- read/write marker expansion;
- invalid URL and minimum-count filtering; and
- incremental replacement when a newer list arrives.

Gate: winning event IDs and final relay counts reconcile independently.

### Slice 9 — publisher benchmark and contract

Goal: choose a product that is correct, bounded, and fast enough to serve.

- measure daily publisher fact size and key cardinality;
- benchmark exact predefined windows;
- benchmark candidate heavy-hitter sketches if arbitrary windows remain a
  requirement;
- measure request latency and refresh cost; and
- freeze limits, supported windows, ties, and deterministic ordering.

Gate: the contract is documented before DDL or route cutover. A daily top-K
shortcut is explicitly prohibited.

### Slice 10 — endpoint cutover and ClickHouse retirement gate

Goal: move routes independently after their products are accepted.

- add Postgres route implementations behind explicit configuration;
- run the authenticated comparison matrix for every parameter shape;
- record freshness and snapshot/run identities in evidence;
- cut over one endpoint family at a time;
- retain rapid per-family rollback; and
- remove a ClickHouse dependency only after no accepted route or operational
  verifier still consumes it.

Gate: all 24 analytics routes are classified as exact match, accepted
approximation, or documented intentional correction. Ingestion has remained
healthy throughout the cutover window.

## 7. Test and evidence requirements

### 7.1 Memory and scale

Every high-cardinality runner test records:

- input rows, objects, and bytes;
- run and merge counts;
- configured batch bytes and fan-in;
- peak RSS and cgroup memory;
- temporary and final disk bytes; and
- wall and CPU time.

At least three increasing cardinalities are tested. Peak RSS must plateau at
the configured bound rather than track input cardinality.

### 7.2 Correctness

Fixtures cover duplicate event IDs across objects, conflicting committed
fields, late historical events, future and unrepresentable timestamps,
excluded kinds, period boundaries, exact set non-additivity, Unicode content,
nested/malformed tags, zap bucket boundaries, top-K ties, and empty products.

Associative reducers are tested under different batch boundaries, merge trees,
input orderings, interruptions, and retries. Final results and evidence hashes
must be invariant.

### 7.3 Production shadow gates

Before publication:

- the selected snapshot and every input/run checksum verify;
- free space remains above the product-specific safety floor;
- ingestion is active with an unchanged restart count and recent successful
  sealing, Parquet publication, and ClickHouse indexing;
- the old current generation is still readable; and
- Postgres staging validation passes inside the publication transaction.

After publication:

- the current run, snapshot, query version, and object ledger agree;
- Postgres row totals match immutable build evidence;
- exact frozen-population reconciliation passes;
- expected approximations are within tolerance; and
- the refresh timer and retention policy are restored and observed through at
  least one successful incremental run.

## 8. Recovery rules

- A failed run never changes the current Postgres pointer.
- A valid completed run is reusable after a Postgres failure.
- A `.partial` run is evidence, not a resumable checkpoint unless its owning
  batch journal explicitly supports resumption.
- Increasing memory is not an accepted fix for cardinality-sized state.
- Input removal, object identity drift, schema drift, or query-version drift
  fails closed and requires a newly planned run.
- Cleanup is a separate, evidence-backed operation after successful
  publication and reconciliation.

## 9. Immediate next action

Implement Slice 0 first so the accepted Slice A shadow refresh is independent
of unfinished B1 work. Then land Slice 1 as infrastructure-only commits before
reintroducing any identity metric. No further production-scale B1 rebuild
should run until the bounded-memory scale gate passes.
