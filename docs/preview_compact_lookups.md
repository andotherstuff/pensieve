# Compact ClickHouse lookups for pensieve-preview

nstr.to (`pensieve-preview`) answers profile, zap and naddr lookups by
filtering `events_local` on `kind`. `events_local` is
`ReplacingMergeTree(indexed_at) ORDER BY id` with no partitions, so without
the full-row `events_by_kind` projection (about 1.33 TiB) those reads become
table scans:
- sampled profile lookups took 21–30 s;
- an event-zap lookup took more than 60 s.

This change adds three small lookup tables, keyed the way preview reads them.
A reversible flag selects them, and the legacy reads stay available for
comparison and rollback. Removing the projection is **not** part of this
change. It stays a separate manual operation after validation (see
[`ops/RUNBOOK.md`](../ops/RUNBOOK.md#preview-compact-lookups)).

| Piece | Location |
|-------|----------|
| Tables and materialized views | [`docs/migrations/prepared/017_preview_compact_lookups.sql`](migrations/prepared/017_preview_compact_lookups.sql) |
| Rollback | [`docs/migrations/rollback/017_drop_preview_compact_lookups.sql`](migrations/rollback/017_drop_preview_compact_lookups.sql) |
| Preview reads and flag | `crates/pensieve-preview/src/lookups/{mod,compact}.rs`, `PREVIEW_COMPACT_LOOKUPS` |
| Operator tool | `preview-lookups` (`crates/pensieve-preview/src/bin/preview-lookups.rs`) |
| Integration tests | `crates/pensieve-preview/tests/compact_lookups.rs` (`just ch-test-up && just test-clickhouse`) |

## Which preview reads depended on the projection

| Read | Legacy query | Compact replacement |
|------|--------------|---------------------|
| `fetch_profile`, `fetch_profiles`, `fetch_display_names` | `argMax(..., created_at)` over `kind = 0 AND pubkey IN (...)` | `profile_latest` |
| `fetch_engagement` zap totals | `events_local` (kind 9735, `arrayExists` e-tag) `JOIN zap_amounts_data` | `zap_receipts_by_target` |
| `fetch_replaceable` (naddr) | `kind = ? AND pubkey = ? AND d_tag = ? ORDER BY created_at DESC LIMIT 1` | `addressable_latest` + `events_local` by id |

Reaction, comment and repost counts already use dedicated tables.
`fetch_event_by_id` is a primary-key read. Neither needs the projection.

The legacy naddr query selects the MATERIALIZED `d_tag` column. A
`SELECT *` projection does not store MATERIALIZED columns. On ClickHouse
24.8 that query is therefore served by the base table and the
`idx_pubkey` bloom filter, not by `events_by_kind`. Production should confirm
this with `system.query_log.projections`. Either way, a pubkey bloom filter
over an id-ordered table reads every granule holding any row from that
author, which is slow for prolific authors.

## Design

### Correctness model

Correctness is guaranteed before merges as well as after, and none of it
depends on background merges. Each read collapses duplicate rows or states
itself: `argMinMerge` for profiles and naddr, and `GROUP BY receipt_id` for
zaps. As a result:
- replayed inserts, which the ingester produces when it retries a segment, never inflate results;
- events arriving out of order never change the winner;
- overlap between backfill and live ingestion is harmless.

`ReplacingMergeTree`/`AggregatingMergeTree` merges only make the tables
smaller. The integration tests check this by asserting every lookup twice:
once with merges stopped and duplicates physically present, and once after
`OPTIMIZE ... FINAL`.

No view joins another table. An insert-triggered view only reacts to inserts
into its source table, so it would miss later changes on the other side of a
join.

### `profile_latest`

`AggregatingMergeTree ORDER BY pubkey`, one aggregate state per pubkey:

```sql
latest = argMinState((id, content, created_at), (-toInt64(created_at), id))
```

- **Winner:** the highest `created_at` wins. On a tie, the lowest event id wins (NIP-01 replaceable semantics).
- **Idempotent and order-independent:** `argMin` is both, so duplicates, out-of-order arrivals and repeated backfill shards cannot change the result.
- **Consistent payload:** `id`, `content` and `created_at` always come from the same event.
- **Pinned types:** explicit `CAST`s pin the tuple types so the view's output matches the column type exactly. A mismatch would make the view throw on every insert. A test asserts the column types.

### `zap_receipts_by_target`

`ReplacingMergeTree(parser_version) ORDER BY (target_event_id, receipt_id)`.
It stores one row per distinct non-empty `e` tag of each kind 9735 receipt.

**How the amount is computed:**
- The amount comes from the receipt's own `bolt11` tag, using **exactly the migration 009 rules**:
  - the amount is taken from the first `bolt11` tag;
  - multipliers `m`/`u`/`n`/`p` only;
  - values above 1 000 000 000 msats become 0.
- `amount_parsed` records 009's inclusion rule: some `bolt11` tag matches `^lnbc[0-9]+[munp]1`.

The read collapses rows per receipt and takes the newest parser version:

```sql
SELECT countIf(p = 1), sumIf(a, p = 1) FROM (
  SELECT receipt_id, argMax(amount_msats, parser_version) a, argMax(amount_parsed, parser_version) p
  FROM zap_receipts_by_target WHERE target_event_id = ? GROUP BY receipt_id)
```

**Amounts arriving before or after receipts.** Today there is no separate
amount stream: `zap_amounts_data` is filled by an insert view over the same
`events_local` insert block as the receipt. The amount is part of the signed
receipt, so deriving it from the receipt row removes any ordering between
amount and receipt. Later rows in `zap_amounts_data` have no effect; a test
covers this.

**Corrections.** A future parser fix ships as a new view writing
`parser_version = 2` alongside v1, followed by a re-backfill. `argMax` picks
the newest version whichever arrives first. Tests cover v2 arriving before
and after v1, both before and after merges. Once v2 is fully backfilled, drop
the v1 view.

### `addressable_latest`

`AggregatingMergeTree ORDER BY (pubkey, kind, d_tag)` covering kinds
10000–19999 and 30000–39999.
- **Winner rule:** the same `argMin` rule as profiles, but storing only the winning event id. The preview then reads the row from `events_local` in one query (`WHERE id IN (SELECT argMinMerge(latest) ...)`), which is a primary-key read.
- **`d_tag`:** uses the same expression as the `events_local.d_tag` MATERIALIZED column, so lookups match the legacy `d_tag = ?` filter exactly.
- **Uncovered kinds:** naddr kinds outside the covered ranges use the legacy query.

### Ingest safety

The new views run inside every `events_local` insert, so a view that throws
fails the whole insert.

Production (checked 2026-10-10) inserts **synchronously**:
- `async_insert = 0` for the ingest user;
- about 70 sealed-segment inserts per hour;
- `system.asynchronous_insert_log` is empty.

The `async_insert` profile settings in `ops/production/clickhouse/config.xml`
are not in effect: ClickHouse reads profiles from the users configuration,
not from a server `config.d` file. A failing view would therefore fail the
INSERT. The ingester would see the error, retry the segment and then queue it
for reindexing. Rows are not lost silently, but ingest into ClickHouse stalls
until the view is fixed or dropped. If async inserts with
`wait_for_async_insert=0` are ever enabled, a failing view would instead drop
rows silently.

Precautions:
- Every view expression is non-throwing: `OrZero` conversions, array indexing that returns defaults, and no fallible casts.
- Integration tests push malformed tags through both synchronous and async (`wait_for_async_insert=0`) inserts and assert that every row lands.
- The runbook monitors `system.query_views_log` (per-view status) and insert exceptions in `system.query_log`. It has an abort rule that drops the views.

## Behaviour differences from the legacy reads

| Area | Legacy | Compact |
|------|--------|---------|
| Profile `created_at` ties | Arbitrary event; `id` and `content` come from separate `argMax` calls and can come from **different** events | Lowest event id wins; `id` and `content` always from one event |
| naddr `created_at` ties | Arbitrary (`ORDER BY created_at DESC LIMIT 1`) | Lowest event id wins |
| Zap counts and amounts | Unmerged duplicate rows in `events_local` or `zap_amounts_data` multiply `count()` and `sum()` | Each receipt is counted once per target |
| Zap inclusion and amount rules | Migration 009 | Identical (tested for parity after merges) |
| Deletions (kind 5) | Ignored | Ignored (unchanged) |
| Freshness | Insert-time | Insert-time (the views run in the same flush) |

## Operator tool: `preview-lookups`

```bash
preview-lookups [--clickhouse-url ... --clickhouse-database nostr] \
  [--max-threads 2 --max-memory-gib 8 --max-execution-secs 3600] \
  [--checkpoint-dir data/preview-lookups] <command>
```

Connection settings also come from `CLICKHOUSE_URL`, `CLICKHOUSE_DATABASE`,
`CLICKHOUSE_USER` and `CLICKHOUSE_PASSWORD`. Every query carries the
`max_threads`, `max_memory_usage`, external aggregation/sort spill limits and
`max_execution_time` settings.

| Command | Purpose |
|---------|---------|
| `plan` | Reads source statistics and an hourly histogram through `events_by_kind`. It writes `<checkpoint-dir>/<target>/plan.json` with contiguous `created_at` shards; the first and last are open-ended, so `created_at = 0` and future timestamps are covered. It also prints the disk estimate. |
| `backfill` | Runs remaining shards (resumable, idempotent). See the guards below. |
| `status` | Shards done, parts, rows, bytes and freshness lag per table. |
| `validate` | Compares compact and legacy reads with a Rust oracle computed from source events, at a fixed cutoff. Exits non-zero on any unexplained difference. |
| `bench` | Lookup latency with `optimize_use_projections=0` and `use_query_cache=0`, plus storage. |

### Backfill guards

- **Gap preflight.** The plan records the view's creation time `T0`. A
  backfill refuses to start unless all of these hold:
  - the view still has that creation time;
  - `now >= T0 + grace` (default 10 minutes);
  - no `INSERT` into `events_local` older than `T0` is still running;
  - no async-insert buffer for `events_local` was first written before `T0`.

  So every row whose insert could have missed the view is already committed
  before the first shard reads. Every later insert is captured by the view,
  including inserts carrying old `created_at` values for shards that are
  already done. Overlap is harmless, as described above. `--gap-pass` adds an
  optional belt-and-braces re-read of rows with `indexed_at` within
  `--gap-margin-mins` of `T0`; it reads the whole target-kind slice and is
  subject to the same runtime, disk and merge-backlog guards as a shard.
- **Projection enforcement.** Shards run with `optimize_use_projections=1`
  and `force_optimize_projection=1`. After each shard the tool also checks
  `system.query_log`:
  - `events_by_kind` must appear in `projections`;
  - `read_rows` must stay under `--max-read-amplification` × the planned rows
    (plus 1M rows of slack).

  This catches parts that lack the projection and would silently be scanned
  in full. A shard with no `query_log` entry cannot be verified, so it is not
  checkpointed. The run stops, and re-running redoes that shard. `--allow-full-scan` with `--shard-by id-prefix` is only for rebuilds
  after the projection is gone; each shard then reads a primary-key range of
  the whole table.
- **Disk guard.** Before each shard, the unreserved space on the table's disk
  must cover the remaining share of the peak estimate plus `--min-free-gib`
  (default 100). Otherwise the run stops cleanly and can be resumed.
- **Merge backlog.** While the target has more than `--max-target-parts`
  active parts (default 300), the tool waits for merges. After 30 minutes it
  stops cleanly.
- **Runtime.** `--max-runtime-mins` is an absolute session deadline. It also
  bounds merge waits and is re-checked right before every INSERT, so no
  INSERT starts after the window closes. `--max-shards` and `--delay-secs`
  further bound each session. Each finished shard writes a JSON checkpoint containing its
  `system.query_log` entry.
- **Progress.** Each shard logs done/total, percent, rows read and written,
  duration and ETA.

### Disk estimate

`plan` prints the following for each target, plus a combined peak compared
with the disk's unreserved space:

- **bytes per row:** `fixed_bytes × 0.6 + payload_bytes × r`.
  - `fixed_bytes` is the hex keys and fixed-width columns. 0.6 is the assumed
    compression ratio for random hex.
  - `payload_bytes` is the average `content` (profiles) or `d` tag (naddr)
    length, measured from the source.
  - `r` is `events_local.content`'s actual compression ratio from
    `system.columns`.
- **final size:** `output_rows × bytes_per_row`, where output rows are the
  merged keys (`uniq`).
- **pre-merge size:** the states written before merges collapse them, per
  shard `min(shard_rows, distinct_keys)`. For zaps this equals the target rows.
- **peak:** `2 × pre-merge`. A merge writes its output while its inputs, and
  then the outdated parts until `old_parts_lifetime` passes, still exist.

This is an estimate. Calibrate it during the first production shards by
comparing `status` (bytes on disk per row) with the plan, and keep
`--min-free-gib` as a hard floor.

## Validation and benchmark

- **`validate`**
  - Samples keys from **`events_local`**, so keys missing from the compact
    tables are detected. It uses a hashed random subset plus the hottest keys.
  - Compares each key at a consistent cutoff: any key with a source row
    indexed after `now - --cutoff-margin-secs` is skipped. Freshness is
    checked again after the compact and legacy reads, so a concurrent insert
    is skipped rather than reported as a difference.
  - A target is reported as **inconclusive**, and the command exits non-zero,
    when nothing was compared but the source has keys. That covers every
    sampled key being skipped, and sampling selecting no keys. Disabling
    sampling (`--sample 0 --hot 0`) is rejected.
  - The compact read must equal the oracle exactly.
  - Legacy differences are classified as `legacy_tie_break`,
    `legacy_id_content_mix`, `legacy_duplicate_inflation`,
    `legacy_amount_row_missing` or `unexplained`.
- **`bench`**
  - Measures single profile, 50-pubkey batch, display names, zaps, the
    hottest zap targets and naddr. Measured queries run with
    `optimize_use_projections=0` and `use_query_cache=0`.
  - Every run uses fresh keys (a new seed per run, with disjoint key pools).
  - `--drop-caches` drops the mark and uncompressed caches before every query.
    This affects the whole server; use it only in a quiet window.
  - Server duration and rows/bytes read come from `system.query_log`
    (`log_comment` tags).
  - `--include-legacy` measures the legacy reads under the same settings with
    a capped timeout.
  - The report states `under target` per lookup against `--target-ms`
    (default 1000) from measured numbers only. The OS page cache cannot be
    dropped from SQL, so state whether production runs were warm.

Preview queries carry `log_comment = 'pensieve-preview:<lookup>:<legacy|compact>'`,
so `system.query_log` gives production latency per lookup and source during
rollout.

### Projections-off rehearsal

`PREVIEW_DISABLE_PROJECTIONS=true` runs **every** preview query with
`optimize_use_projections = 0`, which is how the preview will behave once the
projection is gone. It lets real nstr.to traffic prove the change before the
irreversible drop. To keep the rehearsal from piling up scans, each query is
also capped:
- `max_execution_time = 10` with `timeout_overflow_mode = throw`, matching the
  HTTP request timeout;
- `cancel_http_readonly_queries_on_client_close = 1`, so a query whose request
  was dropped is cancelled.

Any lookup still on its legacy path fails fast instead of scanning for a
minute, and startup logs a warning unless `PREVIEW_COMPACT_LOOKUPS=all`. Turn
it off with `PREVIEW_DISABLE_PROJECTIONS=false` and a restart.

## Requires production validation

The checks below can only be done on the production server. Locally, the
integration suite and an end-to-end CLI run against ClickHouse 24.8.14.39 (the
production build) prove correctness and mechanics only.

1. **Schema:** `SHOW CREATE TABLE nostr.events_local` and
   `system.projection_parts` show which projections exist. Is
   `events_by_author` present? Confirm that every active part has
   `events_by_kind` before relying on it for the backfill.
2. **Sizing:** row and key counts per target from `plan`, especially
   `addressable_latest` (kind 30078 app data may be large), the peak estimate
   against `/data` unreserved space, and calibration after the first shards.
3. **Backfill impact:** runtime and impact on live ingest and queries at the
   chosen `--max-threads`/`--max-memory-gib`.
4. **Ingest overhead:** flush latency and errors after the views are created
   (`system.query_views_log`, insert exceptions in `system.query_log`).
5. **Latency:** `bench` with projections off, query cache off and
   `--drop-caches` in a quiet window. The sub-second target is met only if
   this run shows it.
6. **Other projection consumers:** the `system.query_log` audit in the
   runbook, covering `pensieve-serve` kinds/stats endpoints, negentropy
   seeding and Grafana.
