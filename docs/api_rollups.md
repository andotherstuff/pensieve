# ClickHouse rollups (migrations 019–022)

> **Status (2026-10-12):** migration 019 is applied, backfilled, validated (0 failed rows)
> and serving in production since 12:52 UTC (`releases/api-60c4bfe`). Publisher windows
> beyond ~90 days still exceed the 20 s cap; see
> [`ops/RUNBOOK.md` → API rollups](../ops/RUNBOOK.md#api-rollups-migration-019).
> Migration 020 ([below](#migration-020-incremental-derived-views)) is applied and cut over
> (2026-10-12 19:18 UTC); results in `ops/RUNBOOK.md` → Incremental derived views.
> Migration 021 ([below](#migration-021-kind-activity), `/kinds/{kind}` and
> `/kinds/{kind}/activity`) is applied, backfilled and validated (2026-10-13; the API
> reads it since 07:36 UTC); results in `ops/RUNBOOK.md` → Kind activity.
> Migration 022 ([below](#migration-022-hourly-pubkeys), `/stats/activity/hourly`) is
> prepared, **not yet applied** (2026-10-14). The API release that reads it also serves
> `/stats/events` and `/stats/throughput` from 021's `kind_activity`
> ([below](#statsevents-and-statsthroughput-from-kind_activity)).

This document covers the migrations that move derived data off full scans of
`events_local` and the shared operator tool `api-rollups`. Migration 019 is
described first; [migration 020](#migration-020-incremental-derived-views),
[migration 021](#migration-021-kind-activity) and
[migration 022](#migration-022-hourly-pubkeys) reuse its shard machinery (020
and 021 also its view boundary).

Without projections, `events_local` (~2.43B rows, `ReplacingMergeTree ORDER
BY id`) can only be scanned, and two `pensieve-serve` endpoints hit the API's
20 s query cap (ClickHouse error 159):

- `GET /api/v1/stats/publishers` grouped every event of the last N days by
  pubkey. It read ~300M rows in 20 s.
- `GET /api/v1/stats/longform` summed `length(content)` over every kind 30023
  row. It read 270M rows / 60 GiB of content in 20 s.

Production measurements on 2026-10-12: 30 days of `daily_user_stats`
(`ORDER BY date, pubkey`) read in 2.6 s. `addressable_latest` (migration
017) holds 465,673 kind 30023 articles from 22,026 authors and counts them in
0.16 s, but it stores no content lengths, and looking up 465k ids in
`events_local` timed out.

Migration 019 adds two small tables fed by insert-triggered views, in the
same style as the preview's compact lookups (017).

| Piece | Location |
|-------|----------|
| Tables and views | [`docs/migrations/prepared/019_api_publishers_longform.sql`](migrations/prepared/019_api_publishers_longform.sql) |
| Rollback | [`docs/migrations/rollback/019_drop_api_publishers_longform.sql`](migrations/rollback/019_drop_api_publishers_longform.sql) |
| API reads | `crates/pensieve-serve/src/routes/stats/rollup_reads.rs`, SQL in `crates/pensieve-serve/src/rollups/mod.rs` |
| Operator tool | `api-rollups` (`crates/pensieve-serve/src/bin/api-rollups.rs`) |
| Integration tests | `crates/pensieve-serve/tests/api_rollups.rs` (`just ch-test-up && just test-clickhouse`) |

## Tables

### `publisher_activity`

```sql
grain Enum8('day' = 1, 'month' = 2), kind Int32, period Date, pubkey String,
event_count SimpleAggregateFunction(sum, UInt64),
kinds SimpleAggregateFunction(groupUniqArrayArray, Array(UInt16)),
first_created_at / last_created_at SimpleAggregateFunction(min/max, DateTime),
live_min_indexed_at / last_indexed_at SimpleAggregateFunction(min/max, DateTime)
ENGINE = AggregatingMergeTree ORDER BY (grain, kind, period, pubkey)
TTL period + INTERVAL 400 DAY
```

One view writes every event four times: per UTC day and per UTC month, each
under its own kind and under the all-kinds rollup `kind = -1`.

**Read.** For `days = N`, `start_day` is the UTC day containing
`now() - N days`, and `split_day` is the first month boundary on or after
`start_day`. The read takes day rows in `[start_day, split_day)` and month
rows from `split_day` on, then groups by pubkey:
- `event_count = sum`;
- `kinds_count = length(groupUniqArrayArray(kinds))`;
- `first_event` and `last_event` are the min and max `created_at`.

Every event in the window is counted exactly once. A 365-day ranking reads
at most ~30 daily and ~12 monthly rows per active pubkey, not 365 daily
rows. `kind = K` and `kind = -1` are primary-key prefixes, so a kind filter
reads only that kind's rows.

**Why `AggregatingMergeTree`.** `SummingMergeTree` cannot keep min/max or the
kind set. The `SimpleAggregateFunction` columns merge with sum, min, max and
array union, and reads re-aggregate with the same functions. So results do
not depend on whether merges have run.

**Why unpartitioned.** Monthly partitions would let TTL drop whole parts, but
an insert touching many months (for example odd future timestamps) can exceed
`max_partitions_per_insert_block`. That would make the view throw and fail
the ingest insert. Unpartitioned, every backfill shard is exactly one part,
committed atomically (see below). TTL deletes run at most once a day
(`merge_with_ttl_timeout = 86400`).

**TTL 400 days.** The API accepted `days` 1–366 when this was designed; it now caps at 60 (one year took 39 s and 90 days ~20 s in production; 60 days ~9.5 s), and the 400-day history is kept so the cap can be raised again. Because month rows only start
at the first month boundary inside the window, no read needs a period older
than today − 366. A month row is complete only if the data behind it starts
before the month: the view keeps activity days newer than today − 400, and the
backfill window starts at today − 399 when planned. That is 366 days, plus up
to 31 days of month rounding, plus 3 days of slack.

**Event time.** `activity_at = least(created_at, indexed_at + 1 day)`. The
old query had no upper bound on `created_at`, so an event dated in 2099
stayed in every window forever and could not be expired. Now an event dated
more than a day after ingestion counts on the day after ingestion. It falls
out of windows like any other event, and one insert cannot spread over
arbitrary future periods. `first_event`/`last_event` still report the real
`created_at`, which can lie in the future as before.

**Excluded kinds.** Kinds 445 and 1059 are signed by throwaway keys (one
pubkey per event). They carry no publisher signal and would dominate the
table's cardinality. `daily_user_stats` (migration 005) excludes the same
kinds. Requests for `kind=445` or `kind=1059` get `400`.

### `longform_latest`

```sql
pubkey String, d_tag String,
latest AggregateFunction(argMin, Tuple(String, DateTime, UInt64), Tuple(Int64, String)),
last_indexed_at SimpleAggregateFunction(max, DateTime)
ENGINE = AggregatingMergeTree ORDER BY (pubkey, d_tag)
```

It follows 017's `addressable_latest`:
- **Winner:** the highest `created_at` wins; on a tie, the lowest id wins
  (`argMin` over `(-created_at, id)`).
- **Payload:** the winner's id, `created_at` and `length(content)` in bytes,
  the same measure as the old query.
- **`d_tag`:** the `events_local.d_tag` expression, so a missing `d` tag is
  the `''` coordinate.

The read collapses states with `argMinMerge ... GROUP BY pubkey, d_tag`, then
counts articles and distinct authors and sums lengths. With `days`, it keeps
only articles whose latest version was created in the last N days.

## Correctness

### Longform: idempotent

`argMin` states are idempotent and order-independent. Replays, out-of-order
versions, overlap between the view and the backfill, and re-run shards cannot
change the result. Reads are correct before merges. This is exactly 017's
model, and so is the backfill's gap preflight:
- the grace period has passed since the view's creation `T0`;
- no `events_local` insert that started before `T0` is still running;
- no async-insert buffer predates `T0`.

### Publishers: sums, counted exactly once

Sums are not idempotent. A source row counted by both the view and the
backfill, or by two runs of one shard, stays double-counted forever. Three
mechanisms prevent that.

**1. View boundary.** An `events_local` insert gets `indexed_at = now()` at
query start (one value per insert query; checked on 24.8). The view captures
the insert only if the insert's pipeline was built after the view was
attached, which happens within a second of its creation time `T0`.

The view writes `live_min_indexed_at = min(indexed_at)`; the backfill writes
the sentinel `2106-02-07 06:28:15`. So `C = min(live_min_indexed_at)` is the
first second the view captured. `plan` fixes `C` and refuses unless:
- `C >= T0 + 5 s`, so no insert can have missed the view and still have
  `indexed_at >= C`;
- no insert that started before `C` is still running;
- `C` is stable when read a second time.

Shards then read only `indexed_at < C`, and the view owns every row at or
after `C`. If an insert raced the view's creation, `plan` says so; recreate
the view on an empty table and plan again. The migration refuses to create
the view on a non-empty table.

**2. Exactly-once shards.** Each shard is one `INSERT ... SELECT` over an id
prefix range. It is squashed into a single block
(`min_insert_block_size_rows/bytes` unbounded, `max_insert_threads = 1`), so
it writes exactly one part and commits atomically. Before sending a shard,
the tool writes a `pending/` marker with the query id (fsynced). After a
lost response, crash or error, the next run settles the marker:
- `QueryFinish` in `system.query_log`, or a part in `system.part_log`:
  recorded as done;
- an exception and no part: run again;
- no trace at all: the tool stops and asks the operator to `resolve` the
  shard by hand.

A shard is therefore never run twice.

**3. Validation.** `validate` recomputes sampled pubkeys' rows from
`events_local` and fails on any count above the source's raw row count. It
always includes pubkeys with events indexed around `C`.

**Replayed inserts.** If the same event reaches `events_local` twice, both
copies are counted, and they stay counted after `events_local` merges them
away. The ingester avoids this: when recovering a batch with an unknown
outcome it first filters out ids already in ClickHouse, and RocksDB dedupes
before archiving. A duplicate that slips through inflates one pubkey's count
by one. This is the same trade-off as the existing summing tables
(`daily_user_stats`, `kinds_stats`). The old query also counted unmerged
duplicates, until `events_local` merged them. `validate` reports such rows
as `duplicate_inflation` while the duplicate is still visible.

## Behaviour changes

| Area | Before (scan) | After (019) |
|------|---------------|-------------|
| Longform `articles_count` | Every kind 30023 row, including each edit | Distinct articles: latest version per (pubkey, d tag) |
| Longform lengths | All versions | Latest version only |
| Longform `days` | Versions created in the window | Articles whose latest version was created in the window |
| Longform `unique_authors` | `uniq` (approximate) | `uniqExact` |
| Longform empty result | `avg_content_length` NaN (`null`) | `0.0` |
| Publishers window | `created_at >= now() - N days` (rolling) | Whole UTC days from the day containing `now() - N days` (N to N+1 days) |
| Publishers `days` | Any value | 1–60, else `400` (1–366 until 2026-10-12) |
| Publishers kinds 445/1059 | Ranked | `400` (not stored) |
| Publishers future-dated events | Counted forever | Count on the day after ingestion |
| Publishers `kinds_count` | `uniq` (approximate) | Exact |
| Publishers ties | Arbitrary order | `pubkey` ascending |
| Missing tables | n/a | `503 metric_unavailable` at once (no fallback scan) |

Response shapes and cache keys are unchanged.

## Operator tool: `api-rollups`

```bash
api-rollups [--clickhouse-url ... --clickhouse-database nostr] \
  [--max-threads 2 --max-memory-gib 8 --max-execution-secs 3600] \
  [--checkpoint-dir data/api-rollups] <command>
```

| Command | Purpose |
|---------|---------|
| `plan` | Fixes the view boundary of additive targets (publishers, kinds, kind-activity; hourly-pubkeys is idempotent and has none) and builds 16^n id-prefix shards (default n = 2: 256 shards of ~9.5M rows). It runs shard 0 as a `SELECT` to size the output and prints the full-table read (from `system.columns`), the pre-merge/peak disk estimate and a wall-clock estimate (sample shard time x shards, pauses, quiet minutes). |
| `backfill` | Runs remaining shards. Guards: quiet minutes (default :58–:07 UTC), disk (`--min-free-gib`), merge backlog (`--max-target-parts`), session runtime and shard count, delay between shards. Exit code 2 means stopped early; re-run to resume. |
| `status` | Shards done/pending, parts, rows, size, freshness, and whether the boundary still holds. |
| `validate` | Oracle comparison (see below). Exits non-zero on any failure or when nothing could be compared. |
| `resolve` | Records a pending shard as committed or not, after a manual check. |
| `cutover` | Migration 020 phase 2, guarded (see [below](#cutover)). |

`plan`, `backfill` and `validate` act on `--migration 019`, `--migration 020`,
`--migration 021`, `--migration 022` or explicit
`--target publishers|longform|user-flags|relay-lists|kinds|kind-activity|hourly-pubkeys`
(repeatable). Since 020 there is no implicit "all": running the 019 commands
again would only hit the replan guard.

There is no `bench` subcommand. The runbook times the two endpoint queries
directly, because the API runs them unchanged.

**`validate`:**
- **Publishers.** Samples pubkeys from three sources: the rollup (hashed),
  one random id-prefix slice of `events_local` (finds pubkeys missing from
  the rollup), and events indexed within ±2 minutes of `C`. For each pubkey,
  it recomputes every (grain, kind, period) row of the last 366 days from
  `events_local` (bloom filter on `pubkey`), skipping pubkeys with more than
  `--max-key-events` events. Counts must equal the distinct event ids.
- **Longform.** Cross-checks every coordinate's winner against
  `addressable_latest`, then recomputes sampled authors' latest versions,
  including content lengths.
- **Consistency.** It reads the rollup before the oracle and skips keys with
  source rows indexed after the cutoff, so concurrent ingest never shows up
  as a difference.

## Cost

These are estimates for production with 2.43B rows. `plan` prints the
measured values.

**Backfill read.** Every shard reads its id range of the whole table:
- publishers read id, pubkey, kind, created_at and indexed_at, about
  320 GiB uncompressed;
- longform also reads content and d_tag, about 860 GiB uncompressed.

At the API's observed ~13M rows/s with 4 threads, that is minutes of scan
time per target at 2 threads. The 5 s delay between 256 shards adds about
20 minutes per target. Plan for a few hours wall-clock across sessions.

**Disk.**
- `longform_latest`: ~466k rows, tens of MiB.
- `publisher_activity`: roughly 4× the daily active (pubkey, kind) pairs per
  day plus the monthly pairs, kept for 400 days. Expect on the order of
  daily_user_stats × 2–3, a few GiB to a few tens of GiB.
- **Pre-merge.** Before merges catch up, a backfill writes one row per shard
  for every active (pubkey, day, kind), so prolific publishers are duplicated
  across shards. The plan's peak (pre-merge ×2) is a conservative upper
  bound, and merges collapse parts while the backfill runs.

**Ingest.** Two more views per insert: one aggregation of the insert block,
4× expanded, and one `argMin` over its kind 30023 rows. Both are small next to
the existing views.

**Reads.** The 30-day publisher ranking reads about as many rows as 30 days
of `daily_user_stats` (2.6 s measured), plus at most one month row per
pubkey. Longform reads ~466k states.

## Migration 020: incremental derived views

| Piece | Location |
|-------|----------|
| Phase 1: tables, views, column | [`docs/migrations/prepared/020_incremental_derived_views.sql`](migrations/prepared/020_incremental_derived_views.sql) |
| Phase 2: cutover | [`docs/migrations/prepared/020_incremental_derived_views_cutover.sql`](migrations/prepared/020_incremental_derived_views_cutover.sql), run by `api-rollups cutover` |
| Rollback | [`rollback/020_undo_derived_views_cutover.sql`](migrations/rollback/020_undo_derived_views_cutover.sql), then [`rollback/020_drop_incremental_derived_views.sql`](migrations/rollback/020_drop_incremental_derived_views.sql) |
| SQL shared by the tool, API and tests | `crates/pensieve-serve/src/rollups/derived.rs` (unit tests check the files contain the same queries) |
| API reads | `crates/pensieve-serve/src/routes/stats/rollup_reads.rs` (`/kinds`, `/stats/kinds/total`, `/stats/events/earliest`, overview) |
| Validation | `crates/pensieve-serve/src/rollups/validate_derived.rs` |
| Integration tests | `crates/pensieve-serve/tests/derived_views.rs` (`just ch-test-up && just test-clickhouse`) |

### The problem (production, 2026-10-12, last 24 h)

From `system.query_views_log` and `system.view_refreshes`:

1. **`daily_user_stats_mv`** (005) cost 7.7 s of the ~8.4 s every ingest
   insert into `events_local` takes (2,833 inserts a day, ~6 h a day). Its
   SELECT computes `toUInt8(pubkey IN (SELECT pubkey FROM
   nostr.pubkeys_with_profile_data))` and the same for
   `pubkeys_with_follows_data`, reading ~30.4M rows per insert. All other
   insert-time views cost ~0 s.
2. **`relay_distribution_mv`** (014) refreshes every 6 h by rescanning all of
   `events_local` (1.32 TiB read per run) for each pubkey's latest kind 10002.
3. **`kinds_stats_mv`** (015) refreshes hourly by rescanning all of
   `events_local` (180 GiB per run) for per-kind `count()`,
   `uniqExact(pubkey)` and min/max `created_at`.

The two rescans saturate 24 cores at :00–:07 and push ingest insert p95 from
~17 s to ~31 s (69 s at 12:00 UTC, when both run).

### 1. User flags (`pubkey_flags`)

**Options considered.**
- **(a) Set/Join-engine or dictionary lookups** fed by insert-time views. The
  lookup becomes a hash probe, but Set and Join tables persist every INSERT
  as a new file (one per ingest insert, ~2,800 a day per table, all re-read
  at server start), a dictionary is stale between reloads and its load
  failure would fail the ingest insert, and EmbeddedRocksDB direct joins put a
  new engine in the insert path. The insert path is the thing to protect.
- **(b) No flags at insert time; the refreshes join `daily_user_stats` with
  the lookup tables.** Cheap and simple, but `pubkey IN
  pubkeys_with_profile_data` means "has a profile *now*": a pubkey that
  publishes its profile later would be flagged retroactively on earlier days,
  and published numbers for past days would creep up at every refresh.
- **Chosen: (b) with insert-time semantics kept.** The insert path stores
  `last_indexed_at` (the newest insert time of the row's events) instead of
  flags. `pubkey_flags` keeps `profile_since` / `follows_since`, the first
  `indexed_at` of a kind 0 / kind 3 per pubkey. An event inserted at `t` was
  flagged by the old view iff the pubkey's first kind 0 was inserted in an
  *earlier* insert, i.e. `profile_since < t`: an insert's own kind 0 is not
  visible to its `daily_user_stats_mv`, which runs before
  `pubkeys_with_profile_mv` (checked on 24.8), and ingest inserts are
  sequential, so no two share an `indexed_at` second. So a row is flagged iff
  `profile_since < last_indexed_at`. Rows written before the cutover keep
  their stored flags (`last_indexed_at` is 1970 for them). Pubkeys flagged
  before 020 are seeded from the lookup tables with `since = 1970`.

**Per-insert cost.** Measured on 24.8 with 30M lookup rows (production reads
30.4M): the 005 view took 6.0 s and 4.78 GiB per 14k-event insert (production:
7.7 s); the cutover's view takes 4 ms and 4.7 MiB. The three new insert-time
views take 1–4 ms each.

**Refresh cost.** The active-users refreshes now join each window with
`pubkey_flags`. A plain hash join on the 64-character pubkey needed more than
7 GiB for 25M flagged pubkeys and 30M (date, pubkey) rows, so the join is keyed
by `sipHash128(pubkey)` (16 bytes, collision probability ~1e-23) and uses
`grace_hash` with `max_bytes_in_join = 512 MiB`: 8–11 s and ~750 MiB peak at 4
threads on that data. That runs 27 times a day (hourly, 12-hourly, daily)
instead of two 30M-row set builds in each of ~2,800 ingest inserts.

**Exactly once across the switch.** `daily_user_stats` is a
`SummingMergeTree ORDER BY (date, pubkey)`: `event_count` is summed, so a
switch by "create new view, drop old view" would double count (or drop) the
inserts in between. The cutover uses `ALTER TABLE daily_user_stats_mv MODIFY
QUERY`: an insert builds its view pipeline once, from the old or the new
query, so every event is counted exactly once. The active-users refreshes are
switched *before* the insert path, and read both kinds of rows correctly.

**A bug this fixes (published numbers change).** SummingMergeTree sums every
numeric non-key column, including the UInt8 flags (they wrap at 256). 013's
refreshes count `uniqIf(pubkey, has_profile = 1)`, so a (date, pubkey) whose
events were flagged two or more times in one insert or across merged parts
drops out of `has_profile` (likewise `has_follows`, `has_both`). The numbers
also depended on merge timing. The cutover reads stored flags as `> 0`, so
**`has_profile`, `has_follows` and `has_both` rise** to what the flags meant;
`active_users` and `total_events` are unchanged. `api-rollups validate
--target user-flags` prints the last days under 013's rule, the stored flags
(`> 0`) and the cutover join side by side, so the change can be quantified
before the cutover. (A row whose flag sum wrapped to exactly 0 mod 256 stays
unflagged; that is pre-020 data only.)

**Unchanged:** cohort retention (reads only `date`, `pubkey`),
`/stats/events` (reads `daily_user_stats FINAL` counts), new users
(`pubkey_first_seen`), the tables and API routes for active users.

### 2. Relay lists (`relay_list_latest`)

`argMin` state per pubkey of (id, `created_at`, 'r' tags) keyed by
`(-created_at, id)`: newest wins, lowest id breaks ties (014's
`argMax(tags, created_at)` broke ties arbitrarily). The 'r' tags are filtered
with 014's own expression, `arrayFilter(t -> t[1] = 'r' AND length(t) >= 2,
tags)`, and the cutover's `relay_distribution_mv` runs 014's normalisation,
filters and `HAVING user_count >= 10` over them, into the same table and
columns, so `/api/v1/stats/relays/distribution` is unchanged (`FINAL ORDER BY
user_count DESC`). It refreshes hourly at :20 (reading a few million small
states) instead of rescanning 1.32 TiB every 6 hours. The backfill is
idempotent (017 model).

### 3. Kinds (`kind_totals`)

One row per (kind, origin) with `event_count` (sum), `pubkeys`
(`uniqCombined64` sketch), first/last `created_at` (min/max) and the 019
boundary columns. `event_count` follows the publisher model: the view writes
`live_min_indexed_at`, `plan` fixes the boundary `C`, backfill shards read
only `indexed_at < C` and run exactly once (one part per shard, pending
markers). `origin` keeps the backfill's rows and the view's apart forever, so
`validate` compares the backfill share with `countIf(indexed_at < C)` on
`events_local` exactly.

**Approximate distinct pubkeys (maintainer-approved).** `uniqCombined64` at the
default 2^17 registers: ~0.3% standard error (99.7% of kinds within ~0.9%),
exact for small kinds, at most ~96 KiB per row, and sketches merge without
loss, so view + backfill + merges equal one pass. The 64-bit hash matters:
kinds 1059 and 445 have hundreds of millions of single-use pubkeys, where
32-bit hashes (`uniqCombined`, `uniqHLL12`) collide by several percent.
`uniqHLL12` would also be ~1.6% off; `uniqExact` state cannot be bounded.

**API.** `/api/v1/kinds` reads `kind_totals` (same JSON shape;
`unique_pubkeys` is approximate; counts and first/last are current instead of
up to an hour old; `sort=count` ties go to the lower kind).
`/api/v1/stats/kinds/total` (and the overview's `total_kinds`) counts kinds
whose latest `created_at` is in the last 30 days, the same set as the old
`uniq(kind) ... WHERE created_at >= now() - 30 days` scan.
`/api/v1/stats/events/earliest` (and the overview) is the minimum
`first_created_at`, the same value as the old `min(created_at)` scan. These
three answer `503 metric_unavailable` if `kind_totals` is missing.
`/api/v1/kinds/{kind}` kept scanning `events_local` (it needs content lengths
and 24 h / 7 d / 30 d counts) until [migration 021](#migration-021-kind-activity),
which reads its totals from `kind_totals`.

### Cutover

`api-rollups cutover` runs the phase 2 file statement by statement after
checking: the three views and `daily_user_stats.last_indexed_at` exist; the
user-flags, relay-lists and kinds plans in `--checkpoint-dir` are complete;
no query outside the tool read `kinds_stats_mv` in the last 15 minutes
(`--reader-window-mins`; the old API reads it, the file drops it); not inside
the quiet minutes. `--dry-run` prints the statements. The statements are
idempotent. The time is recorded in `<checkpoint-dir>/cutover-020.json`;
`validate` then checks that rows inserted after it carry `last_indexed_at`.

### Validation (`validate --migration 020`)

- **User flags:** the lookup tables and `pubkey_flags` hold the same pubkeys
  (anti-joins on hashed keys; a disagreement must survive three reads);
  every stored flag of the last `--flag-days` (7) days is backed by
  `pubkey_flags`; sampled pubkeys' `daily_user_stats` rows of the last
  `--user-days` (3) days match `events_local` counts (pubkeys active around
  the recorded cutover are always sampled); before the cutover the join must
  reproduce the stored flags exactly. Informational: the last days of
  `active_users_daily` under 013's rule, stored flags and the join.
- **Relay lists:** sampled winners against all their source versions;
  informational comparison of the distribution the cutover would publish with
  the current `relay_distribution`.
- **Kinds:** the rollup is read, then one scan of `kind`, `indexed_at`,
  `created_at`: backfill share exact; view share and first/last between the
  rows indexed before the cutoff (committed, view rows included, before the
  read) and all rows at the scan. **`unique_pubkeys` against exact counts**
  (since 021; before, against the hourly `kinds_stats_mv` snapshot, which
  fast-moving ephemeral kinds outran — 6 false failures on 2026-10-12 — and
  which the cutover dropped): `--pubkeys-kinds` (12) kinds, half the largest
  by events and half a seeded random choice of the rest, leaving out kinds
  above `--pubkeys-max-exact` (20M approximate pubkeys; `--pubkeys-kind K`
  forces one in). One scan, `PREWHERE kind IN (…)`, keeps a 128-bit pubkey
  hash and the first `indexed_at` per (kind, pubkey) and counts the pubkeys
  first indexed before the cutoff (`settled`), before the boundary, and in
  total. The estimate must lie in `[settled, total]` within
  `max(--pubkeys-sigmas (4) × 0.287% × total, 2)` above 4,096 values (the
  sketch is exact below: slack 2 only), and the backfill rows' sketch within
  the same tolerance of the exact count before the boundary. The notes print
  the largest error. **Cost:** the scan reads `kind` for the whole table and
  `pubkey` + `indexed_at` of every granule holding a sampled kind — every
  granule once kind 1 is in the sample, i.e. one full read of `pubkey`
  (~150 GiB uncompressed): ~10–30 min at 2 threads; memory ~40 B per
  (kind, pubkey), spilling to disk beyond half of `--max-memory-gib`.
  `--pubkeys-kinds 0` skips it.

### Cost (production estimates)

`plan` prints measured values; these are estimates for ~2.45B rows.

| Target | Reads | Output | Wall-clock at 2 threads |
|--------|-------|--------|-------------------------|
| `user-flags` | ~30M rows of the two lookup tables (pubkey only, ~2 GiB) | ~30M rows, ~1 GiB | minutes (plan with `--id-prefix-len 1`) |
| `relay-lists` | id, pubkey, kind, created_at, indexed_at and **tags** of every granule (kind 10002 rows are in nearly all of them): about one 014 rescan, ~1.3–1.5 TiB uncompressed, once | one state per pubkey with a relay list; pre-merge ≈ kind 10002 rows | ~2–3 h (calibrate: the 014 refresh's `duration_ms` in `system.view_refreshes` reads the same data on all cores) |
| `kinds` | id, pubkey, kind, created_at, indexed_at: ~320 GiB uncompressed (as 019's publishers, which took ~2 h) | a few thousand rows per shard; ≤ a few GiB pre-merge | ~1.5–2 h |

Plus 256 x 5 s pauses per target and no starts at :58–:07. Validation scans
`kind`, `indexed_at` and `created_at` once (~25 GiB uncompressed) plus the
exact distinct-pubkeys scan above.
After the cutover the derived views cost: three insert-time views of a few
milliseconds each, the active-users refreshes ~10 s / ~750 MiB each (27 a
day), and an hourly relay distribution over a few million states; the hourly
180 GiB and 6-hourly 1.32 TiB rescans are gone.

## Migration 021: kind activity

| Piece | Location |
|-------|----------|
| Table and view | [`docs/migrations/prepared/021_kind_activity.sql`](migrations/prepared/021_kind_activity.sql) |
| Rollback | [`docs/migrations/rollback/021_drop_kind_activity.sql`](migrations/rollback/021_drop_kind_activity.sql) |
| SQL shared by the tool, API and tests | `crates/pensieve-serve/src/rollups/kind_activity.rs` (unit tests check the migration contains the same expressions) |
| API reads | `crates/pensieve-serve/src/routes/stats/rollup_reads.rs` (`/kinds/{kind}`, `/kinds/{kind}/activity`) |
| Validation | `crates/pensieve-serve/src/rollups/validate_kind_activity.rs` |
| Integration tests | `crates/pensieve-serve/tests/kind_activity.rs` (`just ch-test-up && just test-clickhouse`) |

### The problem

`GET /api/v1/kinds/{kind}` ran two queries over `events_local` filtered by
kind: `count()`, `uniqExact(pubkey)`, min/max `created_at` and
`avg(length(content))`, then `countIf(created_at >= now() - N days)` for 1, 7
and 30 days. Without projections a kind filter reads the `kind` column of all
~2.46B rows and then `pubkey` and `content` of every granule holding the kind,
which for a large kind is every granule (ids are random): kind 1 answered
`500 database_error` after the 20 s cap on 2026-10-12.
`GET /api/v1/kinds/{kind}/activity` (`count()` and `uniqExact(pubkey)` per
day, week or month) has the same shape and the same problem.

### Table: `kind_activity`

```sql
kind UInt16, grain Enum8('hour' = 1, 'day' = 2, 'month' = 3),
period DateTime('UTC'), origin Enum8('backfill' = 1, 'view' = 2),
event_count / content_bytes SimpleAggregateFunction(sum, UInt64),
pubkeys AggregateFunction(uniqCombined64, String),
live_min_indexed_at / last_indexed_at SimpleAggregateFunction(min/max, DateTime)
ENGINE = AggregatingMergeTree ORDER BY (kind, grain, period, origin)
TTL least(toDate(period, 'UTC') + INTERVAL 35 DAY, toDate('2106-01-01')) DELETE WHERE grain = 'hour'
```

One view writes every event to its UTC day and month and, while its hour is
newer than now − 35 days, its UTC hour. `content_bytes` is
`sum(length(content))` in bytes, what the old `avg(length(content))` measured.
`pubkeys` is a `uniqCombined64` sketch of the period's authors on day and
month rows (as `kind_totals`: ~0.3% standard error, exact below ~4k values);
hour rows carry an empty sketch. `event_count` and `content_bytes` are sums
and follow the publisher model exactly: the view writes
`live_min_indexed_at`, `plan` fixes the boundary `C`, shards read only
`indexed_at < C` and run exactly once, and `origin` keeps the backfill's rows
apart so `validate` can compare its share exactly. The backfill writes hour
rows for hours newer than `C − 35 days`. Unpartitioned, as 019 and 020.

Size: one row per (kind, day), (kind, month) and recent (kind, hour), times
two origins until merged — roughly 1–2M rows. Day and month rows of kinds with
more than ~4k authors per period hold dense ~96 KiB sketches: a few GiB in
total for the few dozen large kinds over ~2,100 days.

### Time basis and window semantics

**Periods use the event's own `created_at` in UTC, unclamped.** 019 counts
events dated more than a day after ingestion on the day after ingestion
(`least(created_at, indexed_at + 1 day)`), to expire them and to bound the
periods one insert touches. This table does not: both endpoints filter on
`created_at` relative to `now()`, so the unclamped period reproduces them at
period granularity, and the table is unpartitioned and has no TTL on day and
month rows, so far-future periods cost one small row each. Future-dated events
therefore stay in every "last N days" window of `/kinds/{kind}` (the old
queries had no upper bound either) and enter `/kinds/{kind}/activity` once
their period starts (the old query required `created_at <= now()`).

| Field / endpoint | Before (scan) | After (021) |
|------------------|---------------|-------------|
| `/kinds/{kind}` `event_count`, `first_seen`, `last_seen` | exact, scan | exact, `kind_totals` (same values) |
| `/kinds/{kind}` `unique_pubkeys` | `uniqExact` | approximate, `kind_totals` (`uniqCombined64`, ~0.3%; the same number `/kinds` shows) |
| `/kinds/{kind}` `avg_content_length` | `avg(length(content))` | `sum(content_bytes) / sum(event_count)` of the day rows: same value |
| `/kinds/{kind}` `events_24h/7d/30d` | `created_at >= now() - N days`, no upper bound | hour rows from the UTC hour containing `now() - N days`, no upper bound: N days plus up to one hour more |
| `/kinds/{kind}/activity` periods | `toDate/toMonday/toStartOfMonth(created_at)` in the server time zone | the same in UTC (identical when the server runs UTC, the ClickHouse image default; `SELECT timezone()` in the runbook preflight) |
| `/kinds/{kind}/activity` upper bound | `created_at <= now()` | period start `<= now()`: events dated later in the current period count |
| `/kinds/{kind}/activity` `unique_pubkeys` | `uniqExact` per period | approximate: merged day (weeks) or month sketches |
| Missing table | n/a | `503 metric_unavailable` at once (no fallback scan) |

Response shapes, `404` for a kind without events, cache keys and TTLs are
unchanged.

### Reads

- `/kinds/{kind}` is one query: `kind_totals WHERE kind = ?` (two sketches)
  cross-joined with `kind_activity WHERE kind = ? AND grain IN ('hour',
  'day')`, which reads the kind's day rows (~2,100 for kind 1) and hour rows
  (≤ 840) without their sketches. Primary-key ranges only: tens of
  milliseconds for kind 1 instead of > 20 s.
- `/kinds/{kind}/activity` reads at most 365 day rows (day, and 52 weeks of
  day rows) or 120 month rows and merges their sketches: at most ~35 MiB of
  dense sketches for the largest kinds, well under a second.

**Other readers that scanned `events_local` by kind or time:**
`/stats/throughput` and `/stats/events` (kind-filtered or ungrouped) read this
table since the 022 API release
([below](#statsevents-and-statsthroughput-from-kind_activity)), and
`/stats/activity/hourly` reads its hour rows plus 022's `hourly_pubkeys`
(the hour rows carry no author sketch). Still scanning:
`/stats/events/latest` without the watermark file (`max(created_at)`).

### Validation (`validate --migration 021`)

The rollup is read first, then `events_local` is scanned; rows indexed before
the cutoff were committed (view rows included) before the read, and the scan
sees a superset of what the rollup saw. Every summed share is bracketed: the
backfill share must equal the rows (bytes) indexed before `C`, the view share
must lie between the settled rows and all rows since `C`.

- **Counts, every kind and period:** one scan of `kind`, `created_at`,
  `indexed_at` grouped by kind, UTC day and recent UTC hour; every day and
  month row and the hour rows from 30 days before the cutoff on.
- **Content bytes and author sketches, sampled kinds:** seeded random kinds
  until their events reach `--content-budget-events` (5,000; each event can
  pull one 8,192-row granule of `content` into the scan). Per (kind, day):
  bytes bracketed as above; the sketch within `max(4σ, 2)` of the exact
  distinct pubkeys (first indexed before the cutoff .. after the read).

**Cost:** the count scan reads `kind`, `created_at`, `indexed_at` once (~25
GiB uncompressed; minutes at 2 threads) and returns ~1–3M groups; the content
scan reads `kind` once plus `content`, `pubkey`, `indexed_at` of at most
5,000 granules (~40M rows, ~10 GiB uncompressed).

### Cost (production estimates, ~2.46B rows)

| | |
|---|---|
| Backfill read | id, pubkey, kind, created_at, indexed_at and **content** of every granule: ~320 GiB + ~540 GiB uncompressed, the longform backfill's read (~50 min wall-clock in 019) plus a `GROUP BY kind, grain, period` with sketches |
| Backfill wall-clock | ~1–2 h at 2 threads including 256 × 5 s pauses and the quiet minutes (`plan` prints sample shard × 256) |
| Pre-merge disk | ≤ ~0.5M rows per shard, mostly tiny sketches; `plan` assumes 512 B/row (conservative); merges collapse to ~1–2M rows / a few GiB |
| Ingest | one more view: an `ARRAY JOIN` ×2–3 and `GROUP BY` over each insert block (~14k events), a few ms, like `kind_totals_mv` |

## `/stats/events` and `/stats/throughput` from `kind_activity`

No migration: both read 021's table (SQL in
`crates/pensieve-serve/src/rollups/event_stats.rs`).

### The problem (production, 2026-10-14, API on master)

| Request | Old query | Time |
|---------|-----------|------|
| `/stats/events` | `count(), uniq(pubkey) FROM events_local WHERE created_at <= now() AND created_at >= now() - 30 DAY` | 5.46 s |
| `/stats/events?kind=1` | the same with `kind = 1` | 4.39 s |
| `/stats/events?kind=7&days=30` | the same with `kind = 7` | 4.33 s |
| `/stats/throughput` | `count() FROM events_local WHERE created_at >= now() - 7 DAY [AND kind = K]` | < 1 ms, cached |

A `created_at` (or `kind`) filter on `events_local` reads that column for all
~2.47B rows and `pubkey` of every granule holding a matching row, which for any
recent window is nearly every granule (ids are random).

`/stats/throughput` was fast only because of the API's response cache.
`get_or_compute` keeps an answer for 5 minutes (`ttl::AGGREGATES`). For up to
one more TTL it serves the stale answer while a background task recomputes it,
and that refresh ran the 7-day scan. `hours` is not a parameter of this
endpoint: `?hours=168` is ignored and hits the same cache entry.
`/stats/activity/hourly` (default `days=7`) worked the same way with a
10-minute TTL ([migration 022](#migration-022-hourly-pubkeys)). None of the
three has a watermark or in-process precomputation.

### Reads

- **`/stats/events` without `group_by`** (all kinds or one kind):
  `sum(event_count)` and `uniqCombined64Merge(pubkeys)` over the window's day
  rows. Long windows read month rows for the full months inside them, and day
  rows for the partial months at either end (`DayWindow::month_split`). So
  all history reads at most ~60 day rows and one row per month per kind. The
  all-kinds union merges every kind's sketch per day. For 30 days that is
  ~30 dense (~96 KiB) sketches per large kind, a few hundred MiB at most.
- **`/stats/events?kind=K&group_by=day|week|month`**: the kind's day rows,
  grouped by UTC day or Monday week. Month grouping takes month rows for full
  months and day rows for months the window cuts, so a cut month counts only
  its days inside the window, as before.
- **`/stats/events?group_by=…` without `kind`** still reads
  `daily_user_stats`. It never scanned `events_local` and is unchanged.
- **`/stats/throughput`**: `sum(event_count)` of the hour rows from the UTC
  hour containing `now() - 7 days`, with no upper bound. This is the same
  number as `/kinds/{kind}`'s `events_7d`. `events_per_hour = total / 168`.

### Behaviour changes

| Field / endpoint | Before (scan) | After |
|------------------|---------------|-------|
| `/stats/events` window with `days=N` (default 30) | `created_at` in `[now() - N days, now()]` | Whole UTC days from the day containing `now() - N days` through today: N to N+1 days. The grouped `daily_user_stats` path already used this window |
| `/stats/events` `since`/`until` | `created_at >= since AND created_at < until` (server time zone) and `<= now()` | UTC days `[since, until)`, through today at most (identical when the server runs UTC) |
| `/stats/events` upper bound | `created_at <= now()` | Day rows through today: events dated later today count, later days do not |
| `/stats/events` `count` | `count()` (exact; unmerged duplicate rows included) | `sum(event_count)` (exact; each inserted row once) |
| `/stats/events` `unique_pubkeys` | `uniq(pubkey)` (approximate, adaptive sampling) | Merged `uniqCombined64` sketches (approximate, ~0.3% standard error, exact below ~4k authors per sketch). Values change slightly |
| `/stats/events?kind=K&group_by=…` periods | `toDate`/`toMonday`/`toStartOfMonth(created_at)` in the server time zone | The same in UTC |
| `/stats/throughput` window | `created_at >= now() - 7 days` | From the UTC hour containing `now() - 7 days` (up to one hour more); no upper bound, as before |
| Missing `kind_activity` | n/a | `503 metric_unavailable` at once (no fallback scan) |

Response shapes, cache keys, TTLs and the `400` for an unknown `group_by`
are unchanged.

### Validation

`validate --migration 021` already checks the rows these reads add up: every
(kind, day/month/hour) count, and sampled kinds' content bytes and day
sketches. The new read merges day sketches across kinds.
`validate --migration 022` compares that all-kinds union, and the day's event
count, with exact counts from `events_local` for every UTC day of the last
`--hour-days` days ([below](#validation-validate---migration-022)).

## Migration 022: hourly pubkeys

| Piece | Location |
|-------|----------|
| Table and view | [`docs/migrations/prepared/022_hourly_pubkeys.sql`](migrations/prepared/022_hourly_pubkeys.sql) |
| Rollback | [`docs/migrations/rollback/022_drop_hourly_pubkeys.sql`](migrations/rollback/022_drop_hourly_pubkeys.sql) |
| SQL shared by the tool, API and tests | `crates/pensieve-serve/src/rollups/hourly_pubkeys.rs` (unit tests check the migration contains the same expressions) |
| API read | `crates/pensieve-serve/src/routes/stats/rollup_reads.rs` (`/stats/activity/hourly`) |
| Validation | `crates/pensieve-serve/src/rollups/validate_hourly_pubkeys.rs` |
| Integration tests | `crates/pensieve-serve/tests/hourly_pubkeys.rs` (`just ch-test-up && just test-clickhouse`) |

### The problem

`GET /api/v1/stats/activity/hourly` ran `toHour(created_at)`, `count()` and
`uniq(pubkey)` over `events_local WHERE created_at >= now() - N days [AND
kind = K]`, with N up to 90. `days=30` took 6.72 s on 2026-10-14. The default
(`days=7`) answered in under a millisecond from the API cache, but every
refresh (10-minute TTL, stale-while-revalidate) scanned again.

021's hour rows hold the counts, but their author sketch is empty. No other
rollup keeps authors per hour: `daily_user_stats` and `publisher_activity`
are per day.

### Design

Two options were considered:

- **Make `kind_activity_mv` write hour sketches** (`ALTER ... MODIFY QUERY`).
  This adds no view. But the last 35 days of hour rows would stay empty until
  a backfill wrote sketch-only rows into a table whose sums must be written
  exactly once. The API could not tell a half-filled table from a full one.
  And the all-kinds read would merge one sketch per kind and hour, ~10k dense
  sketches for 30 days.
- **Chosen: a separate `hourly_pubkeys` table** that holds only sketches.
  Sketches are idempotent, so the backfill follows 017's model: no boundary,
  and overlap or re-runs are harmless. A missing table is a clean `503`. An
  all-kinds row per hour (`kind = -1`, as in `publisher_activity`) means the
  unfiltered read merges one sketch per hour.

```sql
kind Int32,  -- -1 = all kinds
period DateTime('UTC'),  -- UTC hour of created_at
pubkeys AggregateFunction(uniqCombined64, String),
last_indexed_at SimpleAggregateFunction(max, DateTime)
ENGINE = AggregatingMergeTree ORDER BY (kind, period)
TTL least(toDate(period, 'UTC') + INTERVAL 35 DAY, toDate('2106-01-01'))
```

The view writes every event twice, under its own kind and under all kinds,
and only for hours newer than `now() - 35 days`. The time basis is 021's:
`created_at` in UTC, unclamped. The table is unpartitioned, and the TTL is
applied at most once a day.

Size: one row per active (kind, hour), plus one per hour, for 35 days. That
is a few hundred thousand rows. Rows are dense (~96 KiB) only for hours with
more than ~4k authors of a kind: the few largest kinds and the all-kinds rows.
Expect 1–2 GiB.

### Read

```text
counts:  kind_activity hour rows   period >= toStartOfHour(now() - N days, 'UTC')  [kind = K]
authors: hourly_pubkeys            period >= toStartOfHour(now() - N days, 'UTC')  kind = K or -1
per toHour(period, 'UTC'): sum(event_count), uniqCombined64Merge(pubkeys), sum / N
```

`days = N` reads at most 24 × 30 + 1 sketches: 721 for an unfiltered 30-day
request, ~70 MiB if all are dense. It also reads the matching hour rows of
`kind_activity`, without their sketches.

### Behaviour changes

| | Before (scan) | After (022) |
|---|---|---|
| `days` | Default 7; values above 90 silently became 90 | Default 7; must be 1–30, else `400` (both tables keep 35 days, and day rows cannot give an hour of the day) |
| Window | `created_at >= now() - N days`, no upper bound | From the UTC hour containing `now() - N days` (up to one hour more); no upper bound, so future-dated events still count |
| `hour` | `toHour(created_at)` in the server time zone | UTC hour (identical when the server runs UTC) |
| `event_count` | `count()` | `sum(event_count)` of 021's hour rows (exact) |
| `unique_pubkeys` | `uniq(pubkey)` (approximate) | Merged `uniqCombined64` sketches (approximate, ~0.3%) |
| `avg_per_day` | `event_count / N` | Unchanged |
| Missing `hourly_pubkeys` or `kind_activity` | n/a | `503 metric_unavailable` at once |

The response shape, cache key and TTL are unchanged.

### Backfill

`api-rollups plan --migration 022` has no boundary to fix.
`backfill --migration 022` runs 256 id-prefix shards. Each is an
`INSERT ... SELECT` with the view's expressions over the rows whose hour is
newer than `now() - 35 days` at the shard's start. Hours that drop out of
that window during the backfill are older than anything the API reads.

The preflight is the one for longform and relay lists: the grace period after
the view's creation has passed, and no insert that started before it is still
running. Running a shard twice, or overlapping the view, changes nothing.

**Cost.** Recent rows are spread over every granule. So each shard reads
`created_at` of its id range and `kind`, `pubkey`, `indexed_at` of nearly
every granule in it. In total that is about the read of 020's kinds backfill:
id, pubkey, kind, created_at and indexed_at, ~320 GiB uncompressed. `plan`
prints the measured read and a sample-shard time. The output is small: at most
a few hundred thousand pre-merge rows per shard, and `plan` assumes 256 B per
row.

### Validation (`validate --migration 022`)

The rollups are read first, then `events_local` is scanned once. The scan
starts at the UTC day `--hour-days` days before the cutoff (default 2, at most
30): `PREWHERE created_at >=` that day. It keeps `sipHash128(pubkey)` and the
first `indexed_at` per (kind, hour), (all kinds, hour) and (all kinds, UTC
day).

Each estimate must lie within `max(4σ, 2)` of the exact interval
`[authors first indexed before the cutoff, all authors]`. This applies to:

- every sketch of `hourly_pubkeys` from that day on;
- per day, the union of all kinds' `kind_activity` day sketches, which is what
  `/stats/events` merges.

The day's `kind_activity` event count is bracketed the same way. Missing and
extra rows show as `missing` and `excess`.

**Cost:** `created_at` of the whole table (~10 GiB uncompressed), plus `kind`,
`pubkey` and `indexed_at` of nearly every granule (about one full read of
`pubkey`, ~150 GiB). Expect ~10–30 minutes at 2 threads, like
`validate --target kinds`. Memory is ~60 B per distinct (scope, period,
pubkey), spilling to disk beyond half of `--max-memory-gib`.

### Benchmark

Ready-to-run SQL that compares the old and new reads (estimated marks, then
measured read bytes from `system.query_log`) is in
[`ops/RUNBOOK.md` → Hourly authors → Benchmark](../ops/RUNBOOK.md#7-benchmark-old-vs-new).
