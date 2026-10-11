# Pensieve Ops Runbook

How the production box is actually operated. The box runs at `/home/pensieve/pensieve`
(the repo checkout); default branch `master`.

## Layout

- `ops/production/compose.yml` — Docker stack: ClickHouse, Postgres analytics,
  Prometheus, Grafana, Caddy.
- `ops/production/{caddy,clickhouse,prometheus}/` — config mounted into those containers.
  ClickHouse query settings (profiles) belong in `clickhouse/users.xml` (mounted under
  `users.d`); `<profiles>` in `clickhouse/config.xml` (`config.d`) are silently ignored.
  The `default` user (ingest, API, migrations) uses stock settings, including
  synchronous inserts. Check effective values by name, e.g.
  `SELECT name, value, changed FROM system.settings WHERE name IN ('async_insert',
  'wait_for_async_insert', 'max_memory_usage', 'max_execution_time')`
  (`WHERE changed` lists only overrides, so it is empty for stock settings).
- `ops/systemd/*.service`, `*.timer` — **source copies** of the installed units. Editing a
  file here does NOT change the running unit; you must install + `daemon-reload` (below).
- `ops/scripts/sync-archive.sh` — hourly archive → Storage Box sync (run by `archive-sync.service`).
- Secrets: **`/etc/pensieve/pensieve.env`** (outside the repo). Read by Docker Compose
  (`--env-file`) and by the native binaries (systemd `EnvironmentFile`). Template:
  repo-root [`env.production.example`](../env.production.example).

## What runs how

| Component | Runs as |
|-----------|---------|
| ClickHouse, Postgres analytics, Prometheus, Grafana, Caddy | Docker Compose via `pensieve.service` |
| `pensieve-ingest`, `pensieve-serve`, `pensieve-preview` | native binaries via systemd |
| archive sync (hourly) | `archive-sync.timer` → `archive-sync.service` |
| shadow analytics refresh (daily) | `pensieve-analytics-refresh.timer` → `pensieve-analytics-refresh.service` |

Grafana datasources/dashboards are configured **on the running instance** (no repo
provisioning). The ingester exposes Prometheus metrics on `:9091`.

## Routine deploy

### Archive admission monitoring and recovery

Follow [the recovery policy](../docs/archive_failure_recovery.md). Recovery is
intentionally manual and incident-specific; a universal repair procedure is not
required. For deployment:

- Verify that recovery-required state stops intake and remains observable
  without a systemd restart loop.
- Configure Uptime Kuma to monitor `/health/archive-admission` on the private
  ingester metrics port, accepting only HTTP 200, either directly over existing
  private connectivity or through a credential-protected outbound push probe.
  Pensieve currently uses the latter; do not expose the port publicly. Test
  notification firing and resolution. The checked-in Prometheus rules alone
  do not notify anyone.
- Run a read-only preflight for existing recovery markers/orphan segments and
  account for them before restarting. Preserve files and markers during an
  incident; investigate and repair before explicitly resuming admission.

The routine restart instructions below do not override an active recovery fault.

For a controlled ingester stop, do not treat the exit code of `systemctl stop`
alone as proof of graceful shutdown. Check `Result`, `ExecMainCode`,
`ExecMainStatus`, and the journal for `shutdown complete`; a stop timeout or
SIGKILL is an error requiring investigation before starting the replacement.
With startup Parquet replay enabled, the shadow finishes its current immutable
work unit and defers remaining sealed segments to the unchanged replay policy on
restart. It does not cancel an in-flight conversion/upload. Without startup
replay, the shadow must drain its notification queue. Check catch-up and current
work-unit duration before choosing a stop deadline, particularly when replacing
an older binary that still drains the entire historical replay.

1. **Pull:** `cd ~/pensieve && git pull origin master`
2. **Build (if Rust changed):** `just build-release`
3. **Install/reload units (only if `ops/systemd/` changed):**
   ```bash
   sudo install -m 644 ops/systemd/*.service ops/systemd/*.timer /etc/systemd/system/
   sudo systemctl daemon-reload
   ```
4. **Restart native services as needed** (ingester last to minimize ingest gaps):
   ```bash
   sudo systemctl restart pensieve-api pensieve-preview
   sudo systemctl restart pensieve-ingest
   ```
5. **Restart Docker infra ONLY if `ops/production/` changed:**
   ```bash
   sudo systemctl restart pensieve
   ```
6. **Run migrations explicitly (if schema changed):**
   ```bash
   just ch-migrate docs/migrations/NNN_description.sql
   ```

Verify: `systemctl status pensieve pensieve-api pensieve-ingest pensieve-preview`,
`curl localhost:8080/health`, `journalctl -u pensieve-ingest -f`.

## First live Parquet shadow activation

Installing the binary and systemd unit does **not** enable the shadow. The
optional `/etc/pensieve/parquet-shadow.env` file is the activation switch; seed
it from [`ops/parquet-shadow.env.example`](parquet-shadow.env.example) without
putting credentials in Git.

The first activation must establish a segment-number boundary while ingestion
is stopped. A timestamp is not a safe boundary because newly received Nostr
events may have old or future `created_at` values.

1. Stop the ingester gracefully and verify no compression is still in flight:
   ```bash
   sudo systemctl stop pensieve-ingest
   find /archive/segments -maxdepth 1 -name '*.open' -print
   ```
2. Compute the inclusive replay floor as one greater than the highest existing
   sealed or open segment:
   ```bash
   highest_segment="$(
     find /archive/segments -maxdepth 1 -type f -printf '%f\n' |
       sed -nE 's/^segment-([0-9]+)\.notepack(\.gz|\.open)?$/\1/p' |
       sort -n |
       tail -n 1
   )"
   test -n "$highest_segment"
   replay_from_segment="$((10#$highest_segment + 1))"
   printf 'highest=%s replay_from=%s\n' "$highest_segment" "$replay_from_segment"
   ```
3. Install the restricted configuration, set
   `PENSIEVE_PARQUET_SHADOW_REPLAY_FROM_SEGMENT` to that exact replay floor
   (uncommenting the example entry),
   and verify the object prefix and writer-size settings match the historical
   campaign:
   ```bash
   sudo install -m 600 -o pensieve -g pensieve \
     ops/parquet-shadow.env.example /etc/pensieve/parquet-shadow.env
   sudo -u pensieve nano /etc/pensieve/parquet-shadow.env
   ```
4. Start the ingester and verify that the durable replay policy is the expected
   `from-segment:N`, historical segment numbers are not queued, and the normal
   API/ingest metrics remain healthy:
   ```bash
   sudo systemctl start pensieve-ingest
   journalctl -u pensieve-ingest --since '2 minutes ago' --no-pager
   sqlite3 /archive/segments/.parquet-shadow/inventory.sqlite \
     "SELECT key, value FROM inventory_settings ORDER BY key;"
   ```
5. Let the five-minute maximum-age timer seal the first live segment. Record
   that segment as high-water mark `H`, verify its work unit is `published`,
   and verify the corresponding immutable object and checksum in object
   storage. The historical campaign's catch-up pass must include every sealed
   segment through `H`; the intentional overlap is removed logically by event
   ID later.

On every restart, the live inventory rejects a changed replay policy, object
store, object prefix, writer-size limit, frame-size limit, or staging directory.
The ingester continues with its authoritative notepack path while logging that
the optional shadow is disabled. Treat that as a failed shadow deployment:
correct the configuration deliberately; do not delete a populated inventory
merely to bypass the guard.

## Bounded historical Parquet catch-up

The historical campaign is finite. Its immutable input universe ends at the
inclusive historical/live boundary `H = 7702`; later live segments belong to
the live shadow. Do not restart a healthy campaign merely to deploy the source
manifest support. Install it for the next pass after the current sequential
pass has stopped normally.

1. Pull the merged revision and build both campaign binaries:
   ```bash
   cd /home/pensieve/pensieve
   git pull --ff-only origin master
   cargo build --release -p pensieve-lake \
     --bin pensieve-parquet-campaign \
     --bin pensieve-source-manifest
   ```
2. Set these non-secret values in the campaign environment:
   ```bash
   HISTORICAL_MAX_SEGMENT=7702
   SOURCE_MANIFEST=/var/lib/pensieve-parquet/state/historical-source-manifest.json
   SOURCE_MANIFEST_BIN=/home/pensieve/pensieve/target/release/pensieve-source-manifest
   ```
3. Freeze and inspect the manifest without downloading or publishing:
   ```bash
   sudo systemctl stop pensieve-parquet-campaign
   sudo systemd-run --wait --pipe --collect \
     --unit=pensieve-parquet-manifest-freeze \
     --uid=pensieve --gid=pensieve \
     --working-directory=/home/pensieve/pensieve \
     --property=EnvironmentFile=/etc/pensieve/parquet-object-storage.env \
     --property=EnvironmentFile=/etc/pensieve/parquet-campaign.env \
     --setenv=INVENTORY_ONLY=1 \
     /home/pensieve/pensieve/ops/scripts/run-parquet-archive-campaign.sh
   sudo -u pensieve /home/pensieve/pensieve/target/release/pensieve-source-manifest \
     verify \
     --manifest /var/lib/pensieve-parquet/state/historical-source-manifest.json \
     --expected-max-segment-number 7702
   ```
   The build is no-clobber. If the file already exists, the wrapper verifies
   and reuses it. Never delete or replace a populated manifest to make a
   boundary mismatch disappear; investigate the configuration instead.
4. Start the bounded catch-up and let normal inventory idempotency skip sources
   already published:
   ```bash
   sudo systemctl start pensieve-parquet-campaign
   journalctl -u pensieve-parquet-campaign -f
   ```
5. After the pass is idle, write the content-addressed completion report:
   ```bash
   sudo -u pensieve /home/pensieve/pensieve/target/release/pensieve-source-manifest \
     audit \
     --manifest /var/lib/pensieve-parquet/state/historical-source-manifest.json \
     --inventory /var/lib/pensieve-parquet/state/campaign.sqlite \
     --output /var/lib/pensieve-parquet/state/historical-completion-audit.json
   ```
   Exit status `0` means the manifest, complete inventory, and active-raw view
   reconcile. Exit status `2` means the report is valid but incomplete; repair
   every named retryable failure or damaged-source exception rather than
   editing the report or an existing Parquet object.

Damaged-source salvage and recurring exact-ID relay recovery follow
[`docs/archive_recovery.md`](../docs/archive_recovery.md). Keep repairs in
separate inventories, publish them under the same canonical object prefix, and
merge their active fragments into the unified snapshot. Never delete the
failed historical inventory row to make the completion report green.

## Shadow analytics Postgres

`postgres-analytics` is a localhost-only serving store for the DuckDB-built
shadow analytics products. Set `POSTGRES_ANALYTICS_PASSWORD` in the private
`/etc/pensieve/pensieve.env`, then start only this dependency with:

```bash
sudo docker compose \
  --project-directory /home/pensieve/pensieve/ops/production \
  --env-file /etc/pensieve/pensieve.env \
  -f /home/pensieve/pensieve/ops/production/compose.yml \
  up -d postgres-analytics
```

Install `/etc/pensieve/analytics.env` with mode `0600`; its `DATABASE_URL`
must use `127.0.0.1`, the `pensieve` database, and the
`pensieve_analytics` user. Run Slice A as a batch job following
[`docs/analytics_slice_a.md`](../docs/analytics_slice_a.md). Do not point the
API or Grafana at the shadow views until the fixed-`as_of` comparison gate is
accepted.

### Recurring Slice A refresh

The daily refresh advances only the `production-live-shadow` fragment in the
currently selected catalog generation. It publishes the immutable catalog,
plans against the current Postgres object ledger, stages only added objects,
takes a copy-on-write DuckDB backup, and atomically applies and publishes an
append-only delta. `no_change` is a successful no-op. Removals, immutable-key
changes, full rebuilds, affected-period plans, staging-limit violations, or any
failed validation leave the generation pointer unchanged and fail the unit for
operator inspection.

Bootstrap the generation pointer once from the exact snapshot and live
fragment used by the current published run. The two files must be a matching
pair; `advance` checks that relationship on every refresh:

```bash
snapshot_hex=90658e6f86fb7430642082369ecf59e3282f1b58c9f6767fec916c26f81ac6fa
sudo install -d -m 0750 -o pensieve -g pensieve \
  /var/lib/pensieve-analytics/refresh \
  /var/lib/pensieve-analytics/refresh/generations \
  /var/lib/pensieve-analytics/refresh/generations/$snapshot_hex \
  /var/lib/pensieve-analytics/refresh/runs \
  /archive/analytics/deltas /archive/analytics/backups
sudo install -m 0640 -o pensieve -g pensieve \
  /var/lib/pensieve-parquet/catalog/incremental-20260804T055921Z/active-raw.json \
  /var/lib/pensieve-analytics/refresh/generations/$snapshot_hex/active-raw.json
sudo install -m 0640 -o pensieve -g pensieve \
  /var/lib/pensieve-parquet/catalog/incremental-20260804T055921Z/production-live.json \
  /var/lib/pensieve-analytics/refresh/generations/$snapshot_hex/production-live.json
printf 'sha256:%s\n' "$snapshot_hex" | sudo tee \
  /var/lib/pensieve-analytics/refresh/generations/$snapshot_hex/APPLIED >/dev/null
sudo chown pensieve:pensieve \
  /var/lib/pensieve-analytics/refresh/generations/$snapshot_hex/APPLIED
sudo chmod 0640 \
  /var/lib/pensieve-analytics/refresh/generations/$snapshot_hex/APPLIED
sudo ln -sfn generations/$snapshot_hex \
  /var/lib/pensieve-analytics/refresh/current
sudo chown -h pensieve:pensieve /var/lib/pensieve-analytics/refresh/current
```

Install a non-secret `/etc/pensieve/analytics-refresh.env` based on
[`analytics-refresh.env.example`](analytics-refresh.env.example). Point
`PENSIEVE_ANALYTICS_WORK_DATABASE` at the current persistent DuckDB file. Then
install and activate the units:

```bash
sudo install -m 0755 ops/scripts/run-analytics-refresh.sh \
  /home/pensieve/pensieve/ops/scripts/run-analytics-refresh.sh
sudo install -m 0644 ops/systemd/pensieve-analytics-refresh.service \
  ops/systemd/pensieve-analytics-refresh.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now pensieve-analytics-refresh.timer
sudo systemctl start pensieve-analytics-refresh.service
```

The timer runs daily at 03:20 local time with up to 20 minutes of jitter. The
service is limited to two CPUs, a 20 GiB memory soft limit, a 24 GiB hard limit,
and reduced I/O weight. It retains the newest three copy-on-write DuckDB
backups and two verified local delta caches. Compact catalogs, plans,
verification receipts, status JSON, and checksums remain under
`/var/lib/pensieve-analytics/refresh/runs/` for audit.

Verify a run without exposing secrets:

```bash
systemctl status pensieve-analytics-refresh.service
journalctl -u pensieve-analytics-refresh.service --since today --no-pager
readlink -f /var/lib/pensieve-analytics/refresh/current
jq . /var/lib/pensieve-analytics/refresh/runs/*/status.json
systemctl is-active pensieve-ingest
systemctl show pensieve-ingest -p NRestarts
```

### Slice A comparison evidence

After a successful refresh, run the read-only comparison harness before any
reader cutover. It obtains the fixed `as_of` from the current Postgres run and
uses `events_local FINAL` for ID-deduplicated ClickHouse values. Do not pass
secrets on the command line. Give a root-owned systemd unit or transient
service these environment files:

- `/etc/pensieve/analytics.env` for the Postgres connection and password; and
- `/etc/pensieve/pensieve.env` for the ClickHouse connection, when its
  defaults are not sufficient.

The first production probe should use the same resource envelope as the
refresh job because exact `FINAL` scans are expensive:

```bash
sudo install -d -m 0750 -o pensieve -g pensieve \
  /var/lib/pensieve-analytics/comparisons
sudo systemd-run --wait --pipe --collect \
  --unit=pensieve-analytics-compare-manual \
  --uid=pensieve --gid=pensieve \
  --working-directory=/home/pensieve/pensieve \
  --property=EnvironmentFile=/etc/pensieve/analytics.env \
  --property=EnvironmentFile=/etc/pensieve/pensieve.env \
  --property=CPUQuota=200% \
  --property=MemoryHigh=20G --property=MemoryMax=24G \
  --property=IOWeight=25 \
  /home/pensieve/pensieve/target/release/pensieve-analytics-compare \
    --output /var/lib/pensieve-analytics/comparisons/<snapshot-id>.json \
    --completed-days 30
```

The exact command, exit-status meanings, alignment-evidence schema, and
resource warnings are documented in
[`docs/analytics_slice_a.md`](../docs/analytics_slice_a.md#fixed-as-of-comparison-harness).
An unproven report is useful reconciliation evidence but cannot approve the
P3 parity gate. Keep `pensieve-serve` and Grafana on ClickHouse.

## One-time cutover (ops/ move + secrets → /etc)

The move of compose/systemd paths and secrets to `/etc` must happen in lockstep with
pulling this change, or services break. On the box, after `git pull`:

1. **Create the secrets file** (the old gitignored `.env` survives the pull as an untracked
   file under the now-removed `pensieve-deploy/`):
   ```bash
   sudo install -d -m 750 -o pensieve -g pensieve /etc/pensieve
   # If the old .env is still present, reuse it:
   sudo install -m 600 -o pensieve -g pensieve \
     ~/pensieve/pensieve-deploy/.env /etc/pensieve/pensieve.env
   # Otherwise seed from the template and edit:
   #   sudo install -m 600 -o pensieve -g pensieve env.production.example /etc/pensieve/pensieve.env
   #   sudo -u pensieve nano /etc/pensieve/pensieve.env
   ```
2. **Reinstall all units** (paths + `EnvironmentFile` changed):
   ```bash
   sudo install -m 644 ops/systemd/*.service ops/systemd/*.timer /etc/systemd/system/
   sudo systemctl daemon-reload
   ```
3. **Recreate the Docker stack from the new path** (`pensieve.service` now runs
   `docker compose --env-file /etc/pensieve/pensieve.env up` from `ops/production/`):
   ```bash
   sudo systemctl restart pensieve
   ```
4. **Restart native services** so they pick up the new `EnvironmentFile`:
   ```bash
   sudo systemctl restart pensieve-api pensieve-preview pensieve-ingest
   ```
5. **Verify the cutover preserved live state**, then delete the leftover
   `~/pensieve/pensieve-deploy/.env`:
   ```bash
   docker network ls | grep pensieve-deploy_default   # reused, NOT recreated
   docker exec pensieve-clickhouse clickhouse-client \
     --query "SHOW CREATE TABLE system.query_log"     # shows the 7-day TTL
   systemctl status pensieve pensieve-api pensieve-preview pensieve-ingest
   ```

## Preview compact lookups

Moves nstr.to's profile, zap and naddr reads off the `events_by_kind`
projection and onto small lookup tables. Design, correctness model and
behaviour differences: [`docs/preview_compact_lookups.md`](../docs/preview_compact_lookups.md).
Every step below can be undone. **Dropping the projection is not one of these
steps** (see the end of this section).

Run the tool from the checkout as the `pensieve` user, with
`/etc/pensieve/pensieve.env` loaded (`CLICKHOUSE_URL`, `CLICKHOUSE_DATABASE`):

```bash
cargo build --release --bin preview-lookups
PL="target/release/preview-lookups --checkpoint-dir /home/pensieve/preview-lookups"
```

### 1. Preflight (read-only)

```bash
just ch-query "SELECT version()"
just ch-query "SHOW CREATE TABLE nostr.events_local"
just ch-query "SELECT name, count() parts, sum(rows) rows, formatReadableSize(sum(bytes_on_disk)) bytes FROM system.projection_parts WHERE database = 'nostr' AND table = 'events_local' AND active GROUP BY name ORDER BY name"
# Every active part must carry events_by_kind (expect 0):
just ch-query "SELECT count() FROM system.parts p WHERE database = 'nostr' AND table = 'events_local' AND active AND name NOT IN (SELECT parent_name FROM system.projection_parts WHERE database = 'nostr' AND table = 'events_local' AND name = 'events_by_kind' AND active)"
just ch-query "SELECT name, formatReadableSize(unreserved_space) unreserved, formatReadableSize(total_space) total FROM system.disks"
just ch-query "SELECT count() FROM system.mutations WHERE database = 'nostr' AND table = 'events_local' AND is_done = 0"
# Ingest inserts: expect ~70/hour, synchronous (async_insert empty or 0), no errors:
just ch-query "SELECT toStartOfHour(event_time) h, count() n, sum(written_rows) rows, any(Settings['async_insert']) async_setting, countIf(exception != '') errors FROM system.query_log WHERE event_time > now() - INTERVAL 3 HOUR AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart' GROUP BY h ORDER BY h"
just ch-query "SELECT view_name, status, count() FROM system.query_views_log WHERE event_time > now() - INTERVAL 1 HOUR GROUP BY view_name, status ORDER BY view_name"
```

Do not continue if `events_by_kind` is missing from any part or a mutation
is pending. Record the outputs in the change log.

### 2. Create the tables and views

```bash
just ch-migrate docs/migrations/prepared/017_preview_compact_lookups.sql
```

This creates empty tables and three insert-triggered views on `events_local`.
For the next 30 minutes (about 35 ingest inserts), watch the views and the
inserts:

```bash
just ch-query "SELECT view_name, status, count(), any(exception) FROM system.query_views_log WHERE event_time > now() - INTERVAL 30 MINUTE AND view_name LIKE 'nostr.%' GROUP BY view_name, status ORDER BY view_name"
just ch-query "SELECT count() n, countIf(exception != '') errors, any(exception) FROM system.query_log WHERE event_time > now() - INTERVAL 30 MINUTE AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart'"
journalctl -u pensieve-ingest --since "30 min ago" | grep -i clickhouse
```

Each new view should show `QueryFinish` with the same count as the existing
views.

**Abort:** if any view reports an exception, or inserts into `events_local`
start failing, run the rollback SQL immediately (step 7). Production inserts
are synchronous, so a failing view fails the insert. The ingester then
retries and queues the segment for reindexing, and ClickHouse ingest stalls
until the view is dropped.

### 3. Plan and estimate disk

```bash
$PL plan --max-rows-per-shard 2000000 --min-free-gib 100
```

For each target, check source rows, keys, final and peak size, and the
combined peak against `/data` unreserved space. Production measurements from
2026-10-10, for comparison:
- profiles: 53.8M rows, 18.4M keys, ~8 GiB final;
- zaps: 7.2M receipts, ~0.6 GiB;
- addressable: about 12.7M rows, 2.9M coordinates.

Free space was 256 GiB. If a target comes out far larger than this, stop and
re-check before backfilling it.

### 4. Backfill (while `events_by_kind` exists)

Wait 10 minutes after step 2. The tool enforces this and the other gap checks.
Then run bounded sessions; each session resumes where the last stopped:

```bash
$PL --max-threads 2 --max-memory-gib 8 backfill --max-runtime-mins 120 --delay-secs 10 --min-free-gib 100
$PL status
```

- **Exit code 2** means the run stopped early (runtime, disk guard, or merge
  backlog). Re-run the same command later.
- **Error on projection or read amplification:** the tool stops if a shard
  did not use `events_by_kind` or read far more rows than planned.
  Investigate before continuing, and never re-run with `--allow-full-scan` on
  production while the projection exists.
- **Calibrate:** after the first few shards, compare `status` bytes per row
  with the plan's estimate.
- **Optional:** add `--gap-pass` on the final session to re-read rows indexed
  around the views' creation time.

### 5. Validate and benchmark

```bash
$PL validate --sample 1000 --hot 100 --out /home/pensieve/preview-lookups/validate.json
$PL bench --samples 50 --out /home/pensieve/preview-lookups/bench-warm.json
# Quiet window only (drops server-wide caches before every query):
$PL bench --samples 50 --drop-caches --include-legacy --legacy-samples 3 --out /home/pensieve/preview-lookups/bench-cold.json
```

- **`validate`** must exit 0: `unexplained differences: 0` and no
  `INCONCLUSIVE` targets. A target where every key was skipped by the cutoff
  proves nothing, so retry later. Legacy
  `tie_break`, `id_content_mix`, `duplicate_inflation` and
  `amount_row_missing` are known legacy defects (see the design doc).
- **`bench`** results are what decide the sub-second target. Record p95 and
  max per lookup, and storage (compact tables vs `events_by_kind`), in the
  change log.

### 6. Switch preview reads (staged)

Set `PREVIEW_COMPACT_LOOKUPS` in `/etc/pensieve/pensieve.env` one lookup at a
time, then `sudo systemctl restart pensieve-preview`:

1. `profiles`
2. `profiles,zaps`
3. `all`

Between steps, compare live latency per lookup and source:

```bash
just ch-query "SELECT log_comment, count() n, quantile(0.5)(query_duration_ms) p50, quantile(0.95)(query_duration_ms) p95, max(query_duration_ms) max FROM system.query_log WHERE event_time > now() - INTERVAL 1 HOUR AND type = 'QueryFinish' AND startsWith(log_comment, 'pensieve-preview:') GROUP BY log_comment ORDER BY log_comment"
journalctl -u pensieve-preview --since "1 hour ago" | grep -c "slow preview lookup"
```

Spot-check a few profiles, zapped notes and articles on nstr.to (`.json`
routes show raw values). The 5-minute HTML cache and CDN delay what you see.

### 7. Rollback

- **Reads:** set `PREVIEW_COMPACT_LOOKUPS=none` and restart
  `pensieve-preview`. While `events_by_kind` exists, the legacy reads use it
  as before. **After migration 018 has removed it, `none` means 10–30 s table
  scans** (and the restore needs 1.6 TiB free), so fix forward instead: switch
  only the affected lookup back via its flag, or rebuild the compact table.
- **Tables and views** (only after reads are rolled back):

  ```bash
  just ch-migrate docs/migrations/rollback/017_drop_preview_compact_lookups.sql
  ```

  Re-applying 017 later needs a fresh `plan --discard-progress` and backfill.

### 8. Projection consumers (before any removal)

The preview is not the only reader that may depend on `events_by_kind`. Known
candidates from the repo:
- `pensieve-serve` `routes/kinds.rs` and `routes/stats.rs` (kind-filtered
  scans);
- negentropy `seed_from_clickhouse` (a `created_at` window);
- Grafana dashboards, which live only on the instance.

With the preview fully switched, audit actual use over at least the 7-day
`query_log` retention. `preview-lookups` tags its own queries and is
excluded:

```bash
just ch-query "SELECT user, splitByChar(':', log_comment)[1] AS client, normalized_query_hash, any(substring(query, 1, 160)) AS example, count() runs, round(avg(query_duration_ms)) avg_ms, max(event_time) last_seen FROM system.query_log WHERE event_date >= today() - 7 AND type = 'QueryFinish' AND has(projections, 'nostr.events_local.events_by_kind') AND NOT startsWith(log_comment, 'preview-lookups') GROUP BY user, client, normalized_query_hash ORDER BY runs DESC LIMIT 100"
```

For each consumer listed, measure it with
`SETTINGS optimize_use_projections = 0` (and `use_query_cache = 0`) before
deciding. The dashboard queries were previously measured at about 9.2 s for
14 queries without the projection.

### 9. Projections-off rehearsal (before any removal)

After the preview has run on `PREVIEW_COMPACT_LOOKUPS=all` without issues:

1. Set `PREVIEW_DISABLE_PROJECTIONS=true` in `/etc/pensieve/pensieve.env` and
   restart `pensieve-preview`. Every preview query then runs with
   `optimize_use_projections = 0`, capped at 10 s and cancelled when its
   request is dropped.
2. Run it for at least a day of normal traffic, watching the step 6 latency
   query, `slow preview lookup` warnings and the preview error rate. This
   query must show the setting applied and no projection used:

   ```bash
   just ch-query "SELECT log_comment, Settings['optimize_use_projections'] AS proj, count() n, quantile(0.95)(query_duration_ms) p95, max(query_duration_ms) max, countIf(notEmpty(projections)) used_projection FROM system.query_log WHERE event_time > now() - INTERVAL 1 DAY AND type = 'QueryFinish' AND startsWith(log_comment, 'pensieve-preview:') GROUP BY log_comment, proj ORDER BY log_comment"
   just ch-query "SELECT log_comment, count() FROM system.query_log WHERE event_time > now() - INTERVAL 1 DAY AND type != 'QueryFinish' AND type != 'QueryStart' AND startsWith(log_comment, 'pensieve-preview:') GROUP BY log_comment"
   ```

   Pass: `proj = 0` and `used_projection = 0` everywhere, p95 well under 1 s
   for the compact lookups, and no rise in failed queries (second query).
3. **Rollback:** set `PREVIEW_DISABLE_PROJECTIONS=false` and restart.

### Projection removal (migration 018)

The removal was accelerated (2026-10-11) because `/data` reached 94% used. The
7-day consumer audit showed the preview's legacy lookups were the only users of
`events_by_kind`, and those had already moved to the compact tables. The
projections-off rehearsal was skipped: `query_log` already showed no preview
query using a projection after the switch to `all`.

1. Re-run the step 8 consumer audit query exactly as written (7 days,
   excluding `preview-lookups`). It must return zero rows other than the
   preview's own legacy lookups from before the switch to `all`.
2. Part-size preflight. ClickHouse 24.8 silently postpones mutating any part
   larger than unreserved space / 1.1. Rows here will wait until free space
   grows (other parts finishing frees space and raises the ceiling); this is
   informational, not a blocker:

   ```bash
   just ch-query "SELECT name, formatReadableSize(bytes_on_disk) AS bytes FROM system.parts WHERE database = 'nostr' AND table = 'events_local' AND active AND bytes_on_disk >= (SELECT toUInt64(unreserved_space / 1.1) FROM system.disks WHERE name = 'default') ORDER BY bytes_on_disk DESC"
   ```

   (2026-10-11: no rows; ceiling ~204 GiB, largest part 150 GiB.)
3. Apply. The file checks that the 017 views exist, that nothing outside
   `preview-lookups` used the projection in the last 6 hours, and that no
   mutation is pending:

   ```bash
   just ch-migrate docs/migrations/prepared/018_drop_events_by_kind_projection.sql
   ```

4. Monitor until `is_done = 1` and no active `events_by_kind` parts remain;
   free space returns as the outdated parts age out. An empty
   `latest_fail_reason` does not by itself mean progress: if `parts_to_do`
   stops falling, re-run the step 2 preflight.

   ```bash
   just ch-query "SELECT mutation_id, create_time, parts_to_do, is_done, latest_fail_reason FROM system.mutations WHERE database = 'nostr' AND table = 'events_local' AND command LIKE '%DROP PROJECTION%events_by_kind%' ORDER BY create_time DESC LIMIT 3"
   just ch-query "SELECT count() parts, formatReadableSize(sum(bytes_on_disk)) bytes FROM system.projection_parts WHERE database = 'nostr' AND table = 'events_local' AND name = 'events_by_kind' AND active"
   df -h /data
   ```

5. Verify the preview and the API: the step 6 latency query, `curl
   http://localhost:8080/health`, and a few nstr.to pages.

After removal:
- `PREVIEW_COMPACT_LOOKUPS=none` brings back 10–30 s legacy reads; the
  rollback SQL needs 1.6 TiB free and is effectively unavailable.
- `preview-lookups plan`, `validate` and `bench` force the projection by
  default; pass `--allow-full-scan`. Rebuilding a compact table (for example
  a zap parser correction) uses `plan --allow-full-scan --shard-by id-prefix`:
  full-table reads in 256 bounded shards, so plan the I/O accordingly.

## Grafana

Provisioning is no longer in the repo. Configure the Prometheus datasource
(`http://prometheus:9090`) and dashboards directly on the running instance at
`https://<DOMAIN>/grafana/`. Plugins (ClickHouse, SQLite datasources) are still
installed at container start. See the ingestion/coverage dashboard guide handed off
separately for the panel set.

## Cleanup debt
- `ops/scripts/sync-archive.sh` still uploads + prunes `*.notepack` segments; this needs
  updating when the Parquet archive lands (see [`docs/migration-plan.md`](../docs/migration-plan.md)).
