# ClickHouse Migrations

This directory contains incremental schema migrations for the Pensieve ClickHouse database.

## Running Migrations

Migrations can be run using the `just` commands:

```bash
# Run a single migration (the only form to use on production)
just ch-migrate docs/migrations/NNN_description.sql

# Fresh, empty database only: base schema, then every top-level migration
just ch-init
just ch-migrate-all
just ch-migrate docs/migrations/prepared/017_preview_compact_lookups.sql
just ch-migrate docs/migrations/prepared/019_api_publishers_longform.sql
just ch-migrate docs/migrations/prepared/020_incremental_derived_views.sql
just ch-migrate docs/migrations/prepared/020_incremental_derived_views_cutover.sql
just ch-migrate docs/migrations/prepared/021_kind_activity.sql
```

On an empty database 020's two phases can run back to back, and 021 after
them: there is no history to backfill (`api-rollups plan/backfill --migration 020` still record
the plans that `cutover` checks; run them before the cutover file, or apply the
cutover file by hand as above).

> **⚠️ `just ch-migrate-all` is for empty databases only.** Nothing records
> which migrations have run, so it re-executes every top-level
> `docs/migrations/*.sql` (001–015) in order. On a database with data that
> means: 004, 005, 006, 008, 012, 013 and 014 drop and rebuild derived
> tables; 009 runs `TRUNCATE TABLE zap_amounts_data` and re-inserts every zap
> receipt; 005 and 008 expect services to be stopped first. Never run it
> against production. Prepared operations under `prepared/` are excluded and
> need an explicit `just ch-migrate <file>` after their documented preflight.
>
> "Idempotent" in these files means "does not fail if objects exist", not
> "safe to re-run": read a migration before running it twice.

`just ch-init` needs the `nostr` database to exist (the production Compose
file creates it with `CLICKHOUSE_DB=nostr`; on a bare server run
`CREATE DATABASE nostr` first). The sequence above, run on ClickHouse 24.8 on
2026-10-11, produced exactly the tables, materialized views and views that
production has. The one difference is `events_local`: the fresh install keeps
the `events_by_kind` and `events_by_author` projections, which production has
dropped (see [Production state](#production-state)).

Or directly via Docker:

```bash
docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/001_active_user_views.sql
```

## Production state

As of 2026-10-11 (from `system.tables` and `system.mutations` on the host):

- 001–015 are applied, along with prepared 016, 017 and 018.
- `nostr.events_local` has **no projections**. Each projection's
  `DROP PROJECTION` mutation finished (`is_done = 1`):

  | Projection | Dropped | How |
  |---|---|---|
  | `events_by_author` | 2026-06-13 | by hand; no migration in this repo |
  | `events_by_time` | 2026-07-27 | `prepared/016` |
  | `events_by_kind` | 2026-10-11 | `prepared/018` |

- The compact lookup tables from 017 are backfilled, and the preview reads
  them (`PREVIEW_COMPACT_LOOKUPS=all`).
- Prepared 019 (API rollups for `/stats/publishers` and `/stats/longform`)
  was applied at 09:14 UTC on 2026-10-12, backfilled and validated (0 failed
  rows); the API reads it since 12:52 UTC.
- Prepared 020 (incremental derived views) was applied, backfilled and
  validated on 2026-10-12 and cut over at 19:18 UTC (`kinds_stats_mv` dropped;
  post-cutover `validate` 0 failed rows); see its section below and the RUNBOOK.
- Prepared 021 (`kind_activity` for `/kinds/{kind}` and
  `/kinds/{kind}/activity`) is prepared, not applied.

`docs/clickhouse_self_hosted.sql` is the fresh-install starting point, not a
snapshot of production. Its header explains how the two differ.

## Migration Index

| Migration | Description | Backfill Required |
|-----------|-------------|-------------------|
| 001_active_user_views.sql | Active user views excluding throwaway keys | No |
| 002_pubkey_first_seen.sql | Track first-seen timestamp per pubkey | **Yes** |
| 003_zap_amounts.sql | Parse bolt11 invoices from zap receipts | **Yes** |
| 004_active_users_materialized.sql | Convert active user views to MVs | **Yes** (run automatically) |
| 005_fix_active_users_views.sql | Fix active user views for large datasets | **Yes** (10-30 min) |
| 006_preaggregate_active_users.sql | Pre-aggregate for instant queries (BROKEN) | Superseded by 008 |
| 007_filter_future_dates_in_views.sql | Filter future dates from views | No |
| 008_fix_active_users_aggregation.sql | **FIX:** Proper AggregatingMergeTree for summaries | **Yes** (1-5 min) |
| 009_fix_zap_amounts.sql | Fix zap amount parsing | **Yes** |
| 010_fix_pubkey_first_seen_dates.sql | Filter invalid dates from new users view | No |
| 011_new_users_summary.sql | Pre-aggregate new users for fast queries | **Yes** (1-5 min) |
| 012_cohort_retention_summary.sql | Pre-aggregate cohort retention (auto-refreshes daily) | **Yes** (5-15 min) |
| 013_schema_optimizations.sql | Replace slow AggregateFunction tables with pre-computed values | **Yes** (runs automatically) |
| 014_relay_distribution.sql | Pre-aggregate NIP-65 relay distribution data | **Yes** (runs automatically) |
| 015_kinds_stats.sql | Pre-aggregate event kind statistics | **Yes** (runs automatically) |
| prepared/016_drop_events_by_time_projection.sql | Removal of the full-row time projection; excluded from `ch-migrate-all`. **Applied in production 2026-07-27** | No (background removal mutation) |
| prepared/017_preview_compact_lookups.sql | Compact profile, zap and naddr lookup tables for `pensieve-preview`; excluded from `ch-migrate-all`. **Applied and backfilled in production** | **Yes** (`preview-lookups backfill`, bounded and resumable) |
| prepared/018_drop_events_by_kind_projection.sql | Removal of the full-row kind projection after 017; excluded from `ch-migrate-all`. **Applied in production 2026-10-11** | No (background removal mutation) |
| prepared/019_api_publishers_longform.sql | Insert-fed rollups for `pensieve-serve` `/stats/publishers` (`publisher_activity`) and `/stats/longform` (`longform_latest`); excluded from `ch-migrate-all`. **Applied, backfilled and validated in production 2026-10-12** | **Yes** (`api-rollups backfill`, bounded, resumable, exactly once) |
| prepared/020_incremental_derived_views.sql (+ `_cutover.sql`) | Phase 1: insert-fed `pubkey_flags`, `relay_list_latest`, `kind_totals` and `daily_user_stats.last_indexed_at`. Phase 2 (cutover, via `api-rollups cutover`): lookup-free `daily_user_stats_mv`, active-users refreshes join the flags, `relay_distribution` from `relay_list_latest` hourly, `kinds_stats_mv` dropped. Excluded from `ch-migrate-all`. **Prepared, not applied** **Applied and cut over in production 2026-10-12** | **Yes** (`api-rollups backfill --migration 020`; kinds exactly once) |
| prepared/021_kind_activity.sql | Insert-fed `kind_activity` (per kind and UTC hour (35 days), day and month of `created_at`: event count, content bytes, `uniqCombined64` author sketch) for `pensieve-serve` `/kinds/{kind}` (with `kind_totals`) and `/kinds/{kind}/activity`. Excluded from `ch-migrate-all`. **Prepared, not applied** | **Yes** (`api-rollups backfill --migration 021`, exactly once) |

> **⚠️ IMPORTANT:** Migration 005 MUST be run if you applied migration 004. The views in 004 do
> expensive JOINs at query time that cause 100% CPU usage on large datasets (100M+ events).
> Stop all services before running migration 005.
>
> **⚠️ CRITICAL:** Migration 006 has a fundamental flaw and should NOT be used. If you ran it,
> run **migration 008** immediately to fix the data. See details below.

---

## Migration Details

### 002_pubkey_first_seen.sql

**Purpose:** Enables user retention cohort analysis and new user time series by tracking when each pubkey was first seen.

**New objects:**
- `pubkey_first_seen_data` - AggregatingMergeTree table storing min(created_at) per pubkey
- `pubkey_first_seen_mv` - Materialized view that populates data for new events
- `pubkey_first_seen` - Helper view for easy querying (finalizes the aggregate)

**Backfill command:**

```bash
just ch-query "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"
```

Or via Docker:

```bash
docker exec -i pensieve-clickhouse clickhouse-client --database nostr -q \
  "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"
```

**Note:** This backfill may take several minutes depending on the size of your events table. Progress can be monitored in the ClickHouse system.processes table.

---

### 003_zap_amounts.sql

**Purpose:** Parses bolt11 lightning invoices from zap receipt events (kind 9735) to extract payment amounts, enabling zap statistics.

**New objects:**
- `zap_amounts_data` - ReplacingMergeTree table storing parsed zap amounts
- `zap_amounts_mv` - Materialized view that parses bolt11 from new zap receipts

**Bolt11 parsing:**

The migration extracts amounts from bolt11 invoices using regex. The bolt11 format is:
- `lnbc<amount><multiplier>1<rest>`
- Multipliers: `m` (milli), `u` (micro), `n` (nano), `p` (pico)
- Example: `lnbc10u1...` = 10 micro-BTC = 1,000 sats

**Backfill command:**

```bash
docker exec -i pensieve-clickhouse clickhouse-client --database nostr -q "
INSERT INTO zap_amounts_data
SELECT
    id AS event_id,
    pubkey AS zapper_pubkey,
    created_at,
    -- Multipliers convert bolt11 amount to millisatoshis
    -- 1 BTC = 100,000,000 sats = 100,000,000,000 msats
    multiIf(
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'm',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000000,      -- milli: 10^8 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'u',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000,         -- micro: 10^5 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'n',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100,            -- nano: 10^2 msats
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = 'p',
        intDiv(toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')), 10),      -- pico: 0.1 msats (truncated)
        extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc[0-9]+([munp]?)1') = '',
        toUInt64OrZero(extract((arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2], '^lnbc([0-9]+)')) * 100000000000,   -- BTC: 10^11 msats
        toUInt64(0)
    ) AS amount_msats,
    (arrayFilter(t -> t[1] = 'p', tags)[1])[2] AS recipient_pubkey,
    (arrayFilter(t -> t[1] = 'P', tags)[1])[2] AS sender_pubkey
FROM events_local
WHERE kind = 9735
    AND arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' AND match(t[2], '^lnbc[0-9]'), tags)
"
```

---

### 004_active_users_materialized.sql

**Purpose:** Converts the slow active user views (which scan 800M+ events on every query) to materialized views for millisecond query times.

**Note:** This migration has a design flaw that causes timeouts on large datasets. **Run migration 005 after this one to fix it.**

**New objects:**
- `pubkeys_with_profile_data` / `pubkeys_with_profile_mv` - Tracks pubkeys with kind=0 events
- `pubkeys_with_follows_data` / `pubkeys_with_follows_mv` - Tracks pubkeys with kind=3 events
- `daily_active_users_data` / `daily_active_users_mv` - Daily aggregates
- `weekly_active_users_data` / `weekly_active_users_mv` - Weekly aggregates
- `monthly_active_users_data` / `monthly_active_users_mv` - Monthly aggregates
- `daily_pubkeys_data` / `daily_pubkeys_mv` - Daily pubkey tracking

**Backfill:** Included in the migration script (runs automatically).

---

### 005_fix_active_users_views.sql

**Purpose:** Fixes the performance issues from migration 004. The original views used FINAL on billion-row tables with JOINs, causing 100% CPU and 60+ second timeouts. This migration replaces them with a pre-aggregated approach.

**New objects:**
- `daily_user_stats` - Stores daily (date, pubkey, has_profile, has_follows, event_count)
- `daily_user_stats_mv` - Populates the stats table from new events
- Recreates `daily_active_users`, `weekly_active_users`, `monthly_active_users` views

**Key improvement:** Profile/follows flags are computed at insert time and stored, so queries just aggregate from pre-computed data (millisecond query times).

**Backfill:** Included in the migration script. Takes 10-30 minutes depending on dataset size.

**⚠️ CRITICAL: Stop all services before running this migration!**

```bash
# Step 1: Stop services that query these views
docker stop pensieve-grafana    # Grafana triggers the expensive queries
sudo systemctl stop pensieve-ingest

# Step 2: Verify ClickHouse is idle
docker exec pensieve-clickhouse clickhouse-client --query "SELECT count() FROM system.processes"
# Should return 0 or 1

# Step 3: Run the migration
docker exec -i pensieve-clickhouse clickhouse-client --database nostr < docs/migrations/005_fix_active_users_views.sql

# Step 4: Verify it worked (should be instant, not 60+ seconds)
docker exec pensieve-clickhouse clickhouse-client --database nostr --query "SELECT * FROM daily_active_users LIMIT 5"

# Step 5: Restart services
sudo systemctl start pensieve-ingest
docker start pensieve-grafana
```

---

### 006_preaggregate_active_users.sql (SUPERSEDED)

**⚠️ WARNING: This migration has a fundamental design flaw and is superseded by migration 008.**

The problem: It used `ReplacingMergeTree` for the summary tables, but the MVs compute aggregates
per INSERT batch. When multiple batches are inserted for the same date, ReplacingMergeTree
**replaces** rows (keeps only the latest) instead of **combining** them. This causes massive
data loss where only one batch's worth of data survives per day.

**Symptoms:** Most days show 1-20 active users and tiny event counts, with random days showing
realistic numbers (those days happened to have only one batch inserted).

**Fix:** Run migration 008.

---

### 007_filter_future_dates_in_views.sql

**Purpose:** Filters out future-dated events (created_at in 2100, 2077, etc.) from active user views.
Some events have invalid timestamps far in the future; this migration excludes them at query time.

**Changes:** Updates the `daily_active_users`, `weekly_active_users`, and `monthly_active_users`
views to filter dates between Nostr genesis (2020-11-07) and today.

**Backfill:** None required.

---

### 009_fix_zap_amounts.sql

**Purpose:** Fixes zap amount parsing issues from migration 003.

---

### 010_fix_pubkey_first_seen_dates.sql

**Purpose:** Filters invalid dates from the `pubkey_first_seen` view, similar to the fix in
migration 007 for active user views.

**The Problem:** The `pubkey_first_seen` view was returning pubkeys with invalid `first_seen`
timestamps, including:
- Future dates (events with created_at like 2100, 2077, etc.)
- Pre-Nostr genesis dates (before 2020-11-07)

This caused the "New Users" dashboard charts and API endpoints to show incorrect data.

**The Fix:** Updates the view to filter: `first_seen >= '2020-11-07' AND first_seen <= now()`

**Backfill:** None required. The underlying data in `pubkey_first_seen_data` is unchanged;
only the view query is updated.

**To run:**

```bash
just ch-migrate docs/migrations/010_fix_pubkey_first_seen_dates.sql
```

---

### 011_new_users_summary.sql

**Purpose:** Creates a pre-aggregated summary table for new users per day, enabling fast queries
for the "new users" time series.

**The Problem:** The `pubkey_first_seen` view requires scanning and merging all rows in
`pubkey_first_seen_data` (~6M+ pubkeys) on every query, making queries take many seconds.

**The Solution:** Pre-aggregate new user counts per day into an `AggregatingMergeTree` summary table.

**New objects:**
- `daily_new_users_summary` - AggregatingMergeTree table storing uniqState(pubkey) per date
- `daily_new_users_summary_mv` - Materialized view from events_local inserts
- `daily_new_users` - Query view with date filtering and uniqMerge finalization

**Backfill:** Included in migration (1-5 minutes depending on data size).

**To run:**

```bash
just ch-migrate docs/migrations/011_new_users_summary.sql
```

---

### 012_cohort_retention_summary.sql

**Purpose:** Creates pre-aggregated summary tables for cohort retention analysis, enabling fast
queries for the `/stats/users/retention` endpoint. Uses **Refreshable Materialized Views**
(ClickHouse 23.12+) for automatic daily updates.

**The Problem:** The retention endpoint performs an extremely expensive query that:
1. Scans `pubkey_first_seen` (6M+ rows with aggregation finalization)
2. JOINs against `events_local` (800M+ rows)
3. Groups by cohort period and activity period

This query can take 60+ seconds or timeout entirely.

**The Solution:** Use Refreshable MVs to pre-aggregate cohort retention data. The MVs:
- Perform a full table replacement on each refresh (handles shifting cohort assignments)
- Run automatically on schedule (no external cron needed)
- Use `daily_user_stats` and `pubkey_first_seen` as source data

**New objects:**
- `cohort_retention_weekly` - Target table for weekly cohort retention
- `cohort_retention_weekly_mv` - Refreshable MV (daily at 03:00 UTC)
- `cohort_retention_monthly` - Target table for monthly cohort retention
- `cohort_retention_monthly_mv` - Refreshable MV (daily at 03:15 UTC)
- `cohort_retention_weekly_view` - Query view for API compatibility
- `cohort_retention_monthly_view` - Query view for API compatibility

**Dependencies:** Requires migrations 002 (pubkey_first_seen) and 005 (daily_user_stats).

**Initial refresh:** The migration triggers initial data population (5-15 minutes).

**To run:**

```bash
just ch-migrate docs/migrations/012_cohort_retention_summary.sql
```

**Automatic refresh:** Data refreshes daily at 03:00/03:15 UTC. No manual intervention needed.

**To manually trigger a refresh:**

```bash
just ch-query "SYSTEM REFRESH VIEW cohort_retention_weekly_mv"
just ch-query "SYSTEM REFRESH VIEW cohort_retention_monthly_mv"
```

**To check refresh status:**

```bash
just ch-query "SELECT view, status, last_refresh_time, next_refresh_time FROM system.view_refreshes WHERE database = 'nostr'"
```

**To modify refresh schedule:**

```bash
# Change to weekly refresh
just ch-query "ALTER TABLE cohort_retention_weekly_mv MODIFY REFRESH EVERY 1 WEEK"
```

---

### 008_fix_active_users_aggregation.sql

**Purpose:** Fixes the critical bug in migration 006 by using `AggregatingMergeTree` with proper
`*State`/`*Merge` functions instead of `ReplacingMergeTree`.

**The Problem:** Migration 006 used `ReplacingMergeTree` for summary tables. When multiple batches
are inserted for the same date, ClickHouse keeps only ONE row (the latest), losing all previous
batches' data. This caused ~99% data loss on most days.

**The Fix:** Uses `AggregatingMergeTree` with:
- `uniqState(pubkey)` / `sumState(event_count)` in the materialized views
- `uniqMerge()` / `sumMerge()` in the query views

This properly combines aggregate states across batches instead of replacing them.

**To run:**

```bash
# Step 1: Stop services
docker stop pensieve-grafana
sudo systemctl stop pensieve-ingest

# Step 2: Run the migration (drops old tables, creates new ones, backfills)
just ch-migrate docs/migrations/008_fix_active_users_aggregation.sql

# Step 3: Verify the fix worked
just ch-query "SELECT date, active_users, total_events FROM daily_active_users ORDER BY date DESC LIMIT 14"
# Should now show consistent, realistic numbers (not 1-20 users per day)

# Step 4: Restart services
sudo systemctl start pensieve-ingest
docker start pensieve-grafana
```

**Backfill:** Included in the migration script (1-5 minutes depending on dataset size).

---

### prepared/016_drop_events_by_time_projection.sql

**Purpose:** Reclaim the disk and write amplification of the full-row
`events_by_time` projection while keeping `events_local`, the
`idx_created_at` minmax skipping index, and the more heavily used
`events_by_kind` projection (itself removed later by 018).

**Production evidence captured before preparation:**

- ClickHouse version: `24.8.14.39`
- Projection definition: `SELECT * ORDER BY (created_at, kind, pubkey)`
- Active projection rows: `1,874,719,482`
- Projection size: approximately `986.15 GiB`
- `/data` free before removal: approximately `346 GiB`
- Representative queries remained operational with
  `optimize_use_projections = 0`; the slowest sampled query changed from about
  2.0 seconds to 4.5 seconds and is protected by the API cache.

**Status:** applied to production. The drop mutation was created 2026-07-27
and has finished; no `events_by_time` parts remain. The steps below are the
procedure as it was run, kept for other installations.

#### Before applying

1. Capture the current table definition and projection size:

   ```bash
   just ch-query "SHOW CREATE TABLE nostr.events_local"
   just ch-query "SELECT name, count() parts, sum(rows) rows, formatReadableSize(sum(bytes_on_disk)) bytes FROM system.projection_parts WHERE database = 'nostr' AND table = 'events_local' AND active GROUP BY name ORDER BY name"
   ```

2. Confirm there is no failed or long-running mutation on `events_local`:

   ```bash
   just ch-query "SELECT mutation_id, command, create_time, parts_to_do, is_done, latest_fail_reason FROM system.mutations WHERE database = 'nostr' AND table = 'events_local' AND is_done = 0 ORDER BY create_time"
   ```

3. Confirm API and Grafana health. No service stop is required, but schedule the
   change during an observed period because queries will immediately fall back
   to the base table and its skipping indexes.

#### Apply

```bash
just ch-migrate docs/migrations/prepared/016_drop_events_by_time_projection.sql
```

The `ALTER` is asynchronous. The command completing does not mean the disk has
already been reclaimed.

#### Monitor and verify

```bash
just ch-query "SELECT mutation_id, command, create_time, parts_to_do, is_done, latest_fail_reason FROM system.mutations WHERE database = 'nostr' AND table = 'events_local' AND command LIKE '%DROP PROJECTION%events_by_time%' ORDER BY create_time DESC LIMIT 5"

just ch-query "SELECT count() active_parts, sum(rows) rows, formatReadableSize(sum(bytes_on_disk)) bytes FROM system.projection_parts WHERE database = 'nostr' AND table = 'events_local' AND name = 'events_by_time' AND active"

just ch-query "SELECT name, path, formatReadableSize(free_space) free, formatReadableSize(unreserved_space) unreserved, formatReadableSize(total_space) total FROM system.disks ORDER BY name"
```

Success requires all of the following:

- The newest drop mutation has `is_done = 1`, `parts_to_do = 0`, and no failure.
- No active `events_by_time` projection parts remain.
- Free disk has increased as inactive parts age out.
- API error rate and ClickHouse query latency remain acceptable.

Do not run `OPTIMIZE TABLE nostr.events_local FINAL`; it would create avoidable
I/O and temporary-space pressure.

#### Rollback

Rollback is available at
`docs/migrations/rollback/016_restore_events_by_time_projection.sql`.
It restores the exact former definition and then schedules a full historical
materialization.

Before rollback:

1. Wait for the drop mutation to finish.
2. Confirm no other `events_local` mutation is pending.
3. Require at least `1.2 TiB` of unreserved disk headroom. This is a conservative
   floor based on the former `986.15 GiB` projection plus working space; more is
   preferable.
4. Keep monitoring `/data` throughout materialization.

Then run:

```bash
just ch-migrate docs/migrations/rollback/016_restore_events_by_time_projection.sql
```

Monitor the `MATERIALIZE PROJECTION events_by_time` mutation in
`system.mutations`. The projection is not fully restored for historical
queries until that mutation reaches `is_done = 1`. If materialization threatens
the disk reserve, stop and investigate rather than forcing a final optimize.

---

### prepared/017_preview_compact_lookups.sql

**Purpose:** Let nstr.to's profile, zap and naddr previews read three compact
tables instead of filtering `events_local` by kind through the full-row
`events_by_kind` projection.

It creates three tables, each fed by an insert-triggered view on
`events_local`:
- `profile_latest`: latest kind 0 per pubkey;
- `zap_receipts_by_target`: one row per (target event, zap receipt), using
  the migration 009 amount rules;
- `addressable_latest`: latest event id per (pubkey, kind, d_tag) for the
  addressable kinds nstr.to previews via naddr (articles, video, live events,
  wiki, listings, calendar and similar; bulk machine kinds are excluded).

Reads stay correct before merges, with replays, and with out-of-order
arrivals. The migration does **not** backfill or touch the projection.

**Apply, backfill, validate and roll out:** follow
[`ops/RUNBOOK.md` → Preview compact lookups](../../ops/RUNBOOK.md#preview-compact-lookups).
Design and behaviour differences:
[`docs/preview_compact_lookups.md`](../preview_compact_lookups.md).

**Rollback:** set `PREVIEW_COMPACT_LOOKUPS=none`, restart `pensieve-preview`, then:

```bash
just ch-migrate docs/migrations/rollback/017_drop_preview_compact_lookups.sql
```

> **⚠️ Only while `events_by_kind` exists.** Production ran 018 on
> 2026-10-11. Without the projection, `none` makes the preview's lookups
> 10–30 s table scans, and the projection cannot practically be restored
> (rollback/018 needs 1.6 TiB unreserved). On production, fix the compact
> path forward instead of rolling 017 back.


---

### prepared/018_drop_events_by_kind_projection.sql

**Purpose:** Reclaim the ~1.34 TiB full-row `events_by_kind` projection once
`pensieve-preview` reads the compact lookup tables from 017. This was the last
projection on `events_local` in production.

**Production evidence captured before preparation (2026-10-11):**

- ClickHouse version: `24.8.14.39`
- Projection definition: `SELECT * ORDER BY (kind, created_at, pubkey)`
- Active projection rows: `2,412,442,249`, size about `1.34 TiB`
- `/data` at 93.5% used, `225 GiB` free, growing ~40 GiB/day
- 7-day `system.query_log` audit: the only queries that used the projection
  were the preview's legacy profile/display-name/zap lookups, replaced by 017.
  No `pensieve-serve`, Grafana or negentropy query used it.
- 017 backfilled and validated (0 unexplained differences); compact lookups
  benchmarked at p95 ≤ 183 ms with projections and query cache off; preview
  switched to `PREVIEW_COMPACT_LOOKUPS=all`.

**Status:** applied to production. The drop mutation was created 2026-10-11
16:54 (server time) and has finished: no projection parts remain, and `/data`
went from 93.5% to 57% used.

**Guards in the file:** it flushes the query log, then refuses to run if the
017 views are missing, if any query outside the `preview-lookups` tool used
`events_by_kind` in the last 6 hours, or if another `events_local` mutation is
pending.

**Fresh installs** keep `events_by_kind` in `docs/clickhouse_self_hosted.sql`:
the preview's default legacy reads and the 017 backfill rely on it. Remove it
only through this migration, after 017 is applied, backfilled and enabled.

**Apply and monitor:** see
[`ops/RUNBOOK.md` → Projection removal](../../ops/RUNBOOK.md#projection-removal-migration-018).

**Rollback:** `rollback/018_restore_events_by_kind_projection.sql` refuses to
start without 1.6 TiB unreserved, so it is effectively unavailable on the
current production disk. After removal, `PREVIEW_COMPACT_LOOKUPS=none` would
bring back 10–30 s legacy reads, so the compact path is the way forward.

---

### prepared/019_api_publishers_longform.sql

**Purpose:** Serve `GET /api/v1/stats/publishers` and
`GET /api/v1/stats/longform` from two small tables instead of scanning
`events_local`. Without projections both endpoints hit the API's 20 s query
cap (ClickHouse error 159): the publisher ranking read ~300M rows in 20 s
and the longform totals read 270M rows / 60 GiB of content in 20 s.

It creates two tables, each fed by an insert-triggered view on
`events_local`:
- `publisher_activity`: `AggregatingMergeTree ORDER BY (grain, kind,
  period, pubkey)` with event count, kinds and first/last `created_at` per
  UTC day and per UTC month, per kind and for all kinds (`kind = -1`).
  Kinds 445 and 1059 (throwaway keys) are excluded. `TTL period + 400 days`.
  Counts are **sums**: a row counted twice stays counted twice.
- `longform_latest`: the latest version of every kind 30023 article per
  (pubkey, d tag), with its id, `created_at` and content length (migration
  017's `argMin` rule: newest `created_at`, lowest id on ties).

The migration refuses to create the publisher view on a non-empty
`publisher_activity` (the backfill boundary needs the view to start on an
empty table). It does **not** backfill.

**Backfill:** `api-rollups plan` fixes the view boundary (the first second
the view captured) and sizes a sample shard; `api-rollups backfill` then
reads `events_local` in id-prefix shards. Publisher shards read only rows
indexed before the boundary and each runs exactly once (one atomic part per
shard, pending markers resolved from `system.query_log`). Longform shards are
idempotent. `api-rollups validate` compares sampled keys against a direct
oracle on `events_local`.

**Behaviour changes** (see [`docs/api_rollups.md`](../api_rollups.md)):
- longform `articles_count` counts distinct articles (latest version per
  pubkey and d tag), not every version; content statistics come from that
  latest version, and `days` filters on its `created_at`;
- publishers use whole UTC days, accept `days` 1–60 (366 until 2026-10-12; capped because longer windows exceed the 20 s query cap), and reject kinds 445
  and 1059; events dated more than a day after ingestion count on the day
  after ingestion.

**Apply, backfill, validate and roll out:** follow
[`ops/RUNBOOK.md` → API rollups](../../ops/RUNBOOK.md#api-rollups-migration-019).

**Rollback:** both endpoints then answer `503 metric_unavailable`:

```bash
just ch-migrate docs/migrations/rollback/019_drop_api_publishers_longform.sql
```

### prepared/020_incremental_derived_views.sql

**Purpose:** Remove the three most expensive derived-view costs on
`events_local` (~2.45B rows, no projections, ~40M new events a day). Measured
in production on 2026-10-12 over 24 h (`system.query_views_log`,
`system.view_refreshes`):

1. `daily_user_stats_mv` (005) took 7.7 s of the ~8.4 s every ingest insert
   takes (2,833 inserts a day, ~6 h a day): for every event it evaluated
   `pubkey IN (SELECT pubkey FROM pubkeys_with_profile_data)` and the same for
   follows, reading ~30.4M lookup rows per insert.
2. `relay_distribution_mv` (014) rescanned all of `events_local` every 6 h
   (1.32 TiB read per run) for each pubkey's latest kind 10002 list.
3. `kinds_stats_mv` (015) rescanned all of `events_local` every hour
   (180 GiB per run) for per-kind `count()`, `uniqExact(pubkey)`, first/last.

The two rescans saturate 24 cores at :00–:07 and push ingest insert p95 from
~17 s to ~31 s (69 s at 12:00 UTC when both run).

**Phase 1** (this file) creates, without changing any existing object's
behaviour:
- `pubkey_flags` (`AggregatingMergeTree ORDER BY pubkey`): first
  `indexed_at` of a kind 0 / kind 3 per pubkey (`min`; sentinel 2106 =
  never), fed by `pubkey_flags_mv`; seeded by the backfill from
  `pubkeys_with_profile_data` / `pubkeys_with_follows_data` with 1970.
- `daily_user_stats.last_indexed_at SimpleAggregateFunction(max, DateTime)`
  (metadata only; SummingMergeTree merges it with `max`).
- `relay_list_latest` (`ORDER BY pubkey`): `argMin` state of the latest kind
  10002 per pubkey (newest `created_at`, lowest id on ties), payload id,
  `created_at` and the 'r' tags exactly as 014 selected them.
- `kind_totals` (`ORDER BY (kind, origin)`): `event_count` (sum, exactly
  once via the 019 boundary model), `pubkeys` (`uniqCombined64` sketch,
  approximate), first/last `created_at`; `origin` keeps backfill and view rows
  apart so validation is exact.

The migration refuses to create `kind_totals_mv` on a non-empty
`kind_totals`. It does **not** backfill.

**Phase 2** (`020_incremental_derived_views_cutover.sql`, run by
`api-rollups cutover`, which refuses unless the 020 backfills are complete and
nothing still reads `kinds_stats_mv`):
- active-users refreshes (013, same schedules/windows/columns) compute
  `has_profile`/`has_follows` as stored flag `> 0`, or
  `profile_since < last_indexed_at` via one grace-hash join on
  `sipHash128(pubkey)` with `pubkey_flags`;
- `ALTER TABLE daily_user_stats_mv MODIFY QUERY` drops the lookups (atomic for
  inserts: no gap, no double count);
- `relay_distribution_mv` recomputes 014's output from `relay_list_latest`,
  hourly at :20;
- `kinds_stats_mv` is dropped; the API reads `kind_totals`.

**Behaviour changes** (details in [`docs/api_rollups.md`](../api_rollups.md#migration-020-incremental-derived-views)):
- `has_profile`, `has_follows`, `has_both` of the active-users tables keep
  their insert-time meaning but stop undercounting: 013's `= 1` dropped every
  merged (date, pubkey) row whose summed UInt8 flag exceeded 1, so published
  values rise. `active_users` and `total_events` are unchanged.
- `/api/v1/kinds` `unique_pubkeys` is approximate (~0.3% standard error);
  counts and first/last are current instead of up to an hour old.
- relay distribution: `created_at` ties resolve to the lowest id (014: arbitrary).

**Apply, backfill, validate, cut over and roll back:** follow
[`ops/RUNBOOK.md` → Incremental derived views](../../ops/RUNBOOK.md#incremental-derived-views-migration-020).

**Rollback:** undo the cutover first, then drop phase 1:

```bash
just ch-migrate docs/migrations/rollback/020_undo_derived_views_cutover.sql
just ch-migrate docs/migrations/rollback/020_drop_incremental_derived_views.sql
```

### prepared/021_kind_activity.sql

**Purpose:** Serve `GET /api/v1/kinds/{kind}` and
`GET /api/v1/kinds/{kind}/activity` without scanning `events_local`. Both
filtered `events_local` by kind; without projections that reads the `kind`
column of all ~2.46B rows and `pubkey`/`content` of every granule holding the
kind (every granule for a large kind): kind 1 answered `500 database_error`
after the API's 20 s cap on 2026-10-12.

It creates `kind_activity` (`AggregatingMergeTree ORDER BY (kind, grain,
period, origin)`, unpartitioned), fed by `kind_activity_mv`: per kind and UTC
hour (only hours newer than now − 35 days; `TTL` deletes older hour rows), day
and month of the event's own `created_at` (unclamped), `event_count` and
`content_bytes` (`sum(length(content))`) — **sums**, written exactly once via
the 019 boundary model, `origin` keeping backfill and view rows apart — and a
`uniqCombined64` sketch of the authors on day and month rows. The migration
refuses to create the view on a non-empty table. It does **not** backfill.

**Backfill:** `api-rollups plan --migration 021` fixes the view boundary;
`backfill --migration 021` reads `events_local` in 256 id-prefix shards
(id, pubkey, kind, created_at, indexed_at and content: ~860 GiB uncompressed,
~1–2 h at 2 threads), each exactly once; hour rows only for hours newer than
the boundary − 35 days. `validate --migration 021` checks every (kind, period)
count and sampled kinds' content bytes and sketches.

**Behaviour changes** (details in [`docs/api_rollups.md`](../api_rollups.md#migration-021-kind-activity)):
- `/kinds/{kind}`: `event_count`, `first_seen`, `last_seen` from
  `kind_totals` (same values); `unique_pubkeys` approximate (~0.3%, the
  `/kinds` figure); `events_24h/7d/30d` start at the UTC hour containing
  `now() - N days` (up to an hour more than before), still without an upper
  bound, so future-dated events stay counted as before.
- `/kinds/{kind}/activity`: UTC periods; `unique_pubkeys` approximate; events
  dated later in the current period count.
- Both answer `503 metric_unavailable` when the table is missing.

The same release changes `api-rollups validate --target kinds`: approximate
`unique_pubkeys` are compared with exact counts for sampled kinds (one bounded
`PREWHERE kind IN (...)` scan) instead of the hourly `kinds_stats_mv`
snapshot, which the 020 cutover dropped.

**Apply, backfill, validate and roll out:** follow
[`ops/RUNBOOK.md` → Kind activity](../../ops/RUNBOOK.md#kind-activity-migration-021).

**Rollback:** both endpoints then answer `503 metric_unavailable`:

```bash
just ch-migrate docs/migrations/rollback/021_drop_kind_activity.sql
```

---

## Full Deployment Checklist (historical)

This is the checklist used when the stats endpoints were first added; every
migration in it is already applied in production. Deploy binaries as described
in [`ops/RUNBOOK.md` → Routine deploy](../../ops/RUNBOOK.md#routine-deploy).

When deploying new API endpoints that depend on these migrations:

### 1. Run migrations

```bash
just ch-migrate docs/migrations/002_pubkey_first_seen.sql
just ch-migrate docs/migrations/003_zap_amounts.sql
```

### 2. Backfill historical data

```bash
# Backfill pubkey first-seen (required for /stats/users/retention and /stats/users/new)
just ch-query "INSERT INTO pubkey_first_seen_data SELECT pubkey, minState(created_at) FROM events_local WHERE kind NOT IN (445, 1059) GROUP BY pubkey"

# Backfill zap amounts (required for /stats/zaps)
# See the full command in the 003_zap_amounts.sql section above
```

### 3. Verify backfill completed

```bash
# Check pubkey_first_seen row count
just ch-query "SELECT count() FROM pubkey_first_seen"

# Check zap_amounts row count
just ch-query "SELECT count() FROM zap_amounts_data"

# Verify sample data
just ch-query "SELECT * FROM pubkey_first_seen LIMIT 5"
just ch-query "SELECT * FROM zap_amounts_data ORDER BY amount_msats DESC LIMIT 5"
```

### 4. Rebuild and deploy the API server

Build a release of the exact commit and point `pensieve-api` at it, following
[`ops/RUNBOOK.md` → Routine deploy](../../ops/RUNBOOK.md#routine-deploy).
`just build-release` in the host's `~/pensieve` checkout does not change what
the service runs.

---

## Backfill Time Estimates

For large production databases, backfills can take significant time. Here are rough estimates:

| Backfill | Events Scanned | Estimated Time (800M events) |
|----------|----------------|------------------------------|
| pubkey_first_seen | All events | 5-30 minutes |
| zap_amounts | Only kind 9735 | 30 seconds - 5 minutes |

**Factors affecting performance:**
- Disk type (SSD vs HDD)
- Available CPU cores
- RAM for aggregation buffers
- Concurrent query load

### Monitoring Backfill Progress

Run in a separate terminal to watch progress:

```bash
watch -n 5 'docker exec pensieve-clickhouse clickhouse-client --database nostr -q "
SELECT
    query_id,
    round(elapsed, 1) as elapsed_sec,
    formatReadableQuantity(read_rows) as rows_read,
    formatReadableQuantity(total_rows_approx) as total_rows,
    round(100 * read_rows / total_rows_approx, 1) as pct_complete
FROM system.processes
WHERE query LIKE '\''%INSERT INTO%'\''
FORMAT Pretty
"'
```

### Check Zap Count Before Backfill

To estimate zap backfill time, first check how many zap receipts exist:

```bash
just ch-query "SELECT count() FROM events_local WHERE kind = 9735"
```

### Batched Backfill (Optional)

For very large tables, you can batch the pubkey_first_seen backfill by date range to reduce memory pressure:

```sql
-- Example: backfill 2024 data only
INSERT INTO pubkey_first_seen_data
SELECT pubkey, minState(created_at)
FROM events_local
WHERE kind NOT IN (445, 1059)
  AND created_at >= '2024-01-01'
  AND created_at < '2025-01-01'
GROUP BY pubkey
```

---

## New API Endpoints

These migrations enable the following new endpoints:

| Endpoint | Description | Required Migration |
|----------|-------------|-------------------|
| `GET /api/v1/stats/throughput` | Events per hour time series | None |
| `GET /api/v1/stats/users/retention` | Cohort retention analysis | 002 |
| `GET /api/v1/stats/users/new` | New users per period | 002 |
| `GET /api/v1/stats/activity/hourly` | Activity by hour of day | None |
| `GET /api/v1/stats/zaps` | Zap statistics | 003 |
| `GET /api/v1/stats/zaps/histogram` | Zap amount distribution histogram | 003 |
| `GET /api/v1/stats/engagement` | Reply/reaction ratios (currently always 503 `metric_unavailable`) | None |
| `GET /api/v1/stats/longform` | Long-form content stats | None |
| `GET /api/v1/stats/publishers` | Top publishers | None |

All endpoints include HTTP caching headers (`Cache-Control: public, max-age=60, stale-while-revalidate=300`).
