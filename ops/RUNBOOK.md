# Pensieve Ops Runbook

How the production box is actually operated (`ssh pensieve`, user `pensieve`).
The repo checkout is `/home/pensieve/pensieve`; default branch `master`. The
checkout provides the Compose file and the ops scripts, but **not** the service
binaries: those are release builds selected by systemd drop-ins (see
[Routine deploy](#routine-deploy)). Facts marked with a date were observed on the
host on that date.

## Layout

- `ops/production/compose.yml` — Docker stack: ClickHouse, Prometheus, Grafana, Caddy.
  (The never-started `postgres-analytics` service was removed on 2026-10-12; see
  [Postgres analytics removal](#postgres-analytics-removal-2026-10-12).)
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
  Their `ExecStart` lines point at `~/pensieve/target/release/`, but production overrides
  them with drop-ins; [`ops/systemd/dropins/`](systemd/dropins/README.md) records those.
- `ops/scripts/sync-archive.sh` — hourly archive → Storage Box sync (run by `archive-sync.service`
  straight from the checkout, so the checkout's copy is the one that runs).
- Secrets: **`/etc/pensieve/pensieve.env`** (outside the repo). Read by Docker Compose
  (`--env-file`), `pensieve-api`, `pensieve-preview` and `archive-sync` (systemd
  `EnvironmentFile`). **Not** read by
  `pensieve-ingest`, which is configured by its `ExecStart` flags, drop-ins and the optional
  root-only `/etc/pensieve/parquet-shadow.env`. Other root-only files: `analytics.env`,
  `negentropy-worker.env`, the Kuma push URLs. Template:
  repo-root [`env.production.example`](../env.production.example) and `ops/*.env.example`.

## What runs how

Observed 2026-10-11:

| Component | Runs as | Binary / command actually started |
|-----------|---------|-----------------------------------|
| ClickHouse 24.8, Prometheus, Grafana, Caddy | Docker Compose via `pensieve.service` | `~/pensieve/ops/production/compose.yml` |
| `pensieve-serve` | `pensieve-api.service` + `90-postgres-cutover.conf` | `~/pensieve/releases/api-fe54f04/pensieve-serve` (master: 019 rollups, 60-day publishers cap, 020 `kind_totals`, 021 `kind_activity`; since 2026-10-13 09:36 CEST) |
| `pensieve-preview` | `pensieve-preview.service` + `10-release.conf` | `~/pensieve/releases/preview-3ab1045/pensieve-preview` |
| `pensieve-ingest` | `pensieve-ingest.service` + 5 drop-ins | `~/pensieve/releases/ingest-b466b37/pensieve-ingest` (master incl. #72 and #73, since 2026-10-12 16:36 CEST) with `--isolated-negentropy` (from `zzzz-bounded-canary.conf`, which also sets `Restart=no`); `stop-timeout.conf` sets `TimeoutStopSec=15min` |
| isolated NIP-77 worker | `pensieve-negentropy-worker.service` (static; started with the ingester via `negentropy-worker.conf`) | `/usr/local/libexec/pensieve-negentropy-worker`, continuous against Premium Primal since 2026-10-12 |
| archive sync (hourly) | `archive-sync.timer` → `archive-sync.service` | `~/pensieve/ops/scripts/sync-archive.sh` |
| Kuma push probes (30 s) | `pensieve-archive-admission-kuma.timer`, `pensieve-negentropy-kuma.timer` (both active) | `/usr/local/libexec/pensieve-*-kuma.py` (sources: `ops/scripts/*-kuma.py`) |
| Parquet historical campaign | `pensieve-parquet-campaign.service` | in the repo, **not installed** on the host |

While `zzzz-bounded-canary.conf` is installed, the ingester does **not** restart by itself
after it exits; check it after any stop or crash.

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
  Pensieve currently uses the latter (`pensieve-archive-admission-kuma.timer`,
  [`ops/scripts/archive-admission-kuma.py`](scripts/archive-admission-kuma.py));
  do not expose the port publicly. Test
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

**Do not stop the ingester during :00–:07 UTC.** Refreshable views rescan all of
`events_local` at those times: `kinds_stats_mv` (180 GiB) every hour and
`relay_distribution_mv` (1.3 TiB read) at 00/06/12/18 UTC; `active_users_daily_mv` also
refreshes hourly at :00 (from `daily_user_stats`). The migration 020 cutover removes both
rescans ([Incremental derived views](#incremental-derived-views-migration-020)). They saturate
the CPU, ingest inserts slow from ~8 s to ~50 s per batch, and shutdown waits for
the in-flight segment to finish indexing. On 2026-10-12 a stop at 06:04 UTC
exceeded the then 90 s `TimeoutStopSec` and was killed; `stop-timeout.conf` now
allows 15 minutes. Check `system.view_refreshes` (`status`, `next_refresh_time`)
before stopping.

**Killed shutdowns heal themselves (since `ingest-b466b37`, #73).** Every sealed
segment is journaled in `/archive/segments/.clickhouse_reindex_queue` before it is
indexed (`v1 <committed> <batch_end> <path>`), progress is recorded before each insert
batch, and the entry is removed once the segment is fully in `events_local`. On start
the indexer resumes from the journal, checks only the possibly in-flight batch against
`events_local`, and inserts only missing IDs, so the summing views are not
double-counted. Watch `clickhouse_index_queue_segments` (normally 0–2) and
`clickhouse_index_queue_errors_total`; completion logs `indexed segment segment=…
inserted=… already_present=…`. Shutdown gives the indexer 20 s to drain, then stops at
the next batch boundary.

Manual repair is only needed if the journal file itself is lost or a build older than
#73 was killed. Insert only the missing IDs: re-indexing a whole segment double-counts
the summing views (reactions, replies, reposts, daily user stats). The 2026-10-12 repair
converted each segment with `notepack-to-parquet`, selected the IDs absent from
`events_local` through a `clickhouse-client --external` table into a Native file,
checked its row count, inserted it, and re-counted to zero missing.

Production deploys one component at a time from a release build of an exact
commit. Do **not** deploy with `git pull` + `just build-release` in `~/pensieve`:
the services do not run `target/release/` (drop-ins override `ExecStart`). The
checkout is what the Compose stack and `archive-sync` use; keep it a clean
`master` (synced 2026-10-12) and update it with
`git -C ~/pensieve merge --ff-only origin/master` when `ops/` changes, never with
hand edits. Mounted configs and `compose.yml` take effect only when the affected
container is recreated.

Compose changes apply per container: after the 2026-10-12 sync, Prometheus was recreated alone
(`docker compose --project-name pensieve-deploy --env-file /etc/pensieve/pensieve.env up -d --no-deps prometheus`
from `ops/production/`, after `promtool check config`) to load `archive-alerts.yml`.

Pattern (used for `releases/api-*` and `releases/preview-3ab1045`; preview shown). Copying a
warm target dir (`cp -a --reflink=auto /data/pensieve-build/<previous> <new>`) and building in a
resource-limited unit (`systemd-run --uid=pensieve --property=CPUQuota=400% --property=MemoryMax=8G
--property=IOSchedulingClass=idle --setenv=CARGO_TARGET_DIR=…`) keeps ingestion unaffected; the
2026-10-12 API build took 57 s that way.

1. **Build the exact commit in a detached worktree** with its own target dir:
   ```bash
   git -C ~/pensieve fetch origin
   SHA=<7-char commit>
   git -C ~/pensieve worktree add --detach ~/pensieve-preview-$SHA $SHA
   cd ~/pensieve-preview-$SHA
   CARGO_TARGET_DIR=~/pensieve-preview-target cargo build --release --locked -p pensieve-preview
   ```
2. **Install into a new release directory** (root-owned, never overwritten):
   ```bash
   sudo install -d -m 755 /home/pensieve/pensieve/releases/preview-$SHA
   sudo install -m 755 ~/pensieve-preview-target/release/pensieve-preview \
     ~/pensieve-preview-target/release/preview-lookups \
     /home/pensieve/pensieve/releases/preview-$SHA/
   ```
3. **Point the drop-in at it** and restart only that service:
   ```bash
   sudoedit /etc/systemd/system/pensieve-preview.service.d/10-release.conf
   #   [Service]
   #   ExecStart=
   #   ExecStart=/home/pensieve/pensieve/releases/preview-<sha>/pensieve-preview
   sudo systemctl daemon-reload
   sudo systemctl restart pensieve-preview
   ```
4. **Verify** the running binary, the journal and health:
   ```bash
   readlink /proc/$(systemctl show -p MainPID --value pensieve-preview)/exe
   journalctl -u pensieve-preview --since '5 minutes ago' --no-pager
   curl -fsS localhost:8081/health   # API: localhost:8080/health
   ```
5. **Record it:** update the copy in [`ops/systemd/dropins/`](systemd/dropins/README.md)
   in the PR, and keep the previous `releases/` directory for rollback.

Drop-ins per service (2026-10-11): `pensieve-api.service.d/90-postgres-cutover.conf`
(also sets `PENSIEVE_BIND_ADDR`, `PENSIEVE_LATEST_EVENT_WATERMARK_PATH` and
`RELAY_DB_PATH` inline), `pensieve-preview.service.d/10-release.conf`,
and for the ingester `giftwrap-recipient-block.conf`, `nip66-monitor.conf`,
`override.conf`, `stop-timeout.conf` (`TimeoutStopSec=15min`), `zzzz-bounded-canary.conf` (the last `ExecStart` wins).

**Ingester exception.** The running ingester was built from a plain source copy in
`~/releases/negentropy-<sha>/` with `CARGO_TARGET_DIR=/data/pensieve-build/negentropy-<sha>`
and is run from that target dir; the matching `pensieve-negentropy-worker` was installed
to `/usr/local/libexec/`. Follow the graceful-stop checks above for every ingester stop.

**Units.** Install only the unit files you changed, then `daemon-reload`. Do not glob
`ops/systemd/*.service`: that also installs gated units (`pensieve-negentropy-worker`,
the Kuma probes, `pensieve-parquet-campaign`) and the base `ExecStart` lines.
```bash
sudo install -m 644 ops/systemd/<changed>.service /etc/systemd/system/
sudo systemctl daemon-reload
```

**Docker infra:** restart only if `ops/production/` changed, and only after the
checkout holds the intended files: `sudo systemctl restart pensieve`.

**Migrations:** apply one file explicitly, `just ch-migrate docs/migrations/NNN_description.sql`.
Never run `just ch-migrate-all` here: it replays every top-level migration,
including drops, `TRUNCATE` and full backfills (see
[`docs/migrations/README.md`](../docs/migrations/README.md)).

Verify: `systemctl status pensieve pensieve-api pensieve-ingest pensieve-preview`,
`curl localhost:8080/health`, `journalctl -u pensieve-ingest -f`.

### Rollback

Point the drop-in back at the previous release directory, `daemon-reload`, restart
that service, and check `/proc/<pid>/exe`. Do not rebuild from the checkout.

- Removing a drop-in falls back to `~/pensieve/target/release/<binary>`. Those are old
  builds (2026-10-11: `pensieve-preview` from 2026-07-27, before compact lookups, so it
  would do 10–30 s scans now that `events_by_kind` is gone; `pensieve-ingest` is the
  2026-10-05 giftwrap hotfix). The comment inside `10-release.conf` that says "remove
  this file" predates migration 018; do not follow it.
- `releases/` holds `api-6b21547`, `api-8a6bc61`, `api-9829378`, `api-7ac5121` (built from the
  unmerged `codex/negentropy-hardening` branch), `api-8bdac94` (master, previous), `api-f60f003`
  (master after the Postgres removal, previous), `api-60c4bfe` (019 rollups and `api-rollups`), `api-ff8a446` (90-day cap), `api-b466b37` (60-day cap, previous), `api-d026d53` (020 `kind_totals` and `api-rollups cutover`, previous), `api-fe54f04` (021 `kind_activity`, current), `ingest-b466b37` (current
  ingester), `ingest-ba0c4a1` (previous ingester from PR #72, with its worker) and only
  `preview-3ab1045`. Build target dirs other than the latest `api-*`/`ingest-*` caches in
  `/data/pensieve-build/` were removed on 2026-10-12.
- Schema changes roll back through `docs/migrations/rollback/` where a file exists.

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

**Status (2026-10-11):** `pensieve-parquet-campaign.service` is not installed on the
host and `/etc/pensieve/` has no `parquet-campaign.env` or `parquet-object-storage.env`.
The steps below build from the `~/pensieve` checkout; if you run them again, build in a
detached worktree instead (see [Routine deploy](#routine-deploy)) and point
`SOURCE_MANIFEST_BIN` and the campaign binary at that build.

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
     /usr/bin/env INVENTORY_ONLY=1 \
     /home/pensieve/pensieve/ops/scripts/run-parquet-archive-campaign.sh
   sudo -u pensieve /home/pensieve/pensieve/target/release/pensieve-source-manifest \
     verify \
     --manifest /var/lib/pensieve-parquet/state/historical-source-manifest.json \
     --expected-max-segment-number 7702
   ```
   Set `INVENTORY_ONLY=1` with `/usr/bin/env` as shown, not `--setenv`:
   systemd lets `EnvironmentFile=` override `Environment=`, and the template
   sets `INVENTORY_ONLY=0`, which would start the full download/publish run.
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

## DuckDB analytics (research)

`pensieve-analytics` builds exact analytics from an active-file Parquet
snapshot into a local DuckDB database and immutable, evidence-checked
artifacts. These are batch research tools: nothing is served from them and no
unit runs them. Use the private `/etc/pensieve/analytics.env`
([template](analytics.env.example)) for S3 credentials and DuckDB limits, and
give manual runs the same resource envelope as other heavy jobs (for example
`systemd-run --property=CPUQuota=200% --property=MemoryHigh=20G
--property=MemoryMax=24G --property=IOWeight=25`). To read from a local copy
instead of S3, stage the snapshot with `ops/scripts/stage-active-raw-snapshot.sh`
and pass `--local-object-root`. See
[`docs/analytics_slice_a.md`](../docs/analytics_slice_a.md) for the Slice A
build itself.

## Postgres analytics removal (2026-10-12)

The Postgres analytics serving store was abandoned on 2026-10-12. Its code, units,
scripts and Compose service were removed from `master` (#75); production never served
from it. On the host the same day:

- The checkout was fast-forwarded to `f60f003`, so `compose.yml` no longer defines
  `postgres-analytics`; then `POSTGRES_ANALYTICS_PASSWORD` was removed from
  `/etc/pensieve/pensieve.env` (in that order: Compose requires the variable while the
  service is defined). The `postgres:16-alpine` image was removed.
- The API moved to `releases/api-f60f003` and its drop-in dropped
  `PENSIEVE_POSTGRES_API_FAMILIES`.
- `pensieve-analytics-refresh.{service,timer}`, `/etc/pensieve/analytics-refresh.env` and
  every analytics env backup were deleted; `DATABASE_URL` was removed from
  `/etc/pensieve/analytics.env`, which keeps only the AWS settings the DuckDB builders read.
- `/archive/analytics` (3.1 TB) and `/var/lib/pensieve-analytics` (4.7 GB) were deleted.
  They held two stale local snapshots of the Parquet lake, staged deltas, intermediate
  build outputs and Slice A DuckDB databases. All 39,004 staged Parquet files were first
  confirmed present in object storage with identical sizes. `/archive` went from 77% to
  23% used. Researchers query the current lake in object storage.

## One-time cutover (ops/ move + secrets → /etc)

**Historical: completed.** Compose, the units and the scripts already use
`ops/` and `/etc/pensieve/`. Kept as a record; do not re-run it.

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

**Status (2026-10-11):** steps 1–6 are done (017 applied, backfilled, validated,
preview on `PREVIEW_COMPACT_LOOKUPS=all`) and the projection was then removed by
migration 018 (see the end of this section). Steps 1–5 below are kept as the
procedure for a fresh install or a rebuild; on production they now need
`--allow-full-scan` where noted, and rollback step 7 is no longer cheap.

Run the tool as the `pensieve` user with `/etc/pensieve/pensieve.env` loaded
(`CLICKHOUSE_URL`; `CLICKHOUSE_DATABASE` defaults to `nostr`). Use the binary
from the preview release, not a build from the `~/pensieve` checkout (which
predates #67 on 2026-10-11):

```bash
# Current preview release on 2026-10-11; use the directory the drop-in points at.
PL="/home/pensieve/pensieve/releases/preview-3ab1045/preview-lookups --checkpoint-dir /home/pensieve/preview-lookups"
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
  Investigate before continuing, and never re-plan with `plan --allow-full-scan`
  on production while the projection exists (`backfill` follows the plan).
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

## API rollups (migration 019)

Moves `pensieve-serve`'s `GET /api/v1/stats/publishers` and
`GET /api/v1/stats/longform` off full scans of `events_local` (both time out
at the API's 20 s cap since the projections were dropped) and onto two small
tables fed by insert-triggered views. Design, correctness model and behaviour
changes: [`docs/api_rollups.md`](../docs/api_rollups.md).

**Status:** Applied, backfilled and deployed on 2026-10-12: migration at 09:14:29 UTC
(boundary `indexed_at < 1791796480`, 11 s after the views); backfill 09:25–12:15 UTC
(publishers 256/256, ~2 h incl. quiet-minute pauses; longform 256/256, ~50 min; ingest p95
insert latency rose from ~8 s to ~19 s during the publisher shards, no insert errors, no
indexing backlog); `validate` 0 failed rows (publishers 22,175 rows / 667 pubkeys incl. 200
around the boundary; longform 108,895 rows; `addressable_latest` cross-check 466,207 matched,
0 different); API `releases/api-60c4bfe` since 12:52 UTC. Query timings with the cache off:
publishers 30 d 4.9 s, longform 0.4 s, publishers 366 d **39 s**.

**Known limit:** the API caps `days` at 60 (`400` above that) since 2026-10-12. Since
mid-2026 20–45 million distinct pubkeys are active per month (mostly single-use keys), so
cost grows with the window: 30 days ~26M rows / 5–7 s, 60 days ~46M / ~9.5 s for the
ranking alone and 14.4 s through the API (which also computes kinds and first/last times),
90 days ~71M / ~20 s (at the cap), one year ~150M / 39 s. Re-measure if key churn grows;
a precomputed ranking is the fix if 60 days approaches the cap.
The rollup keeps 400 days, so the cap can be raised once long windows get a precomputed
path.

Run the tool as the `pensieve` user with `/etc/pensieve/pensieve.env` loaded
(`CLICKHOUSE_URL`; `CLICKHOUSE_DATABASE` defaults to `nostr`). Build
`pensieve-serve` and `api-rollups` from the merged commit into a new
`releases/api-<sha>/` directory, as for any API release (see
[Routine deploy](#routine-deploy)):

```bash
AR="/home/pensieve/pensieve/releases/api-<sha>/api-rollups --checkpoint-dir /home/pensieve/api-rollups"
```

Every tool query is tagged `log_comment = 'api-rollups…'` and carries
`--max-threads` (default 2), `--max-memory-gib` (default 8) and
`--max-execution-secs` limits.

### 1. Preflight (read-only)

```bash
just ch-query "SELECT version()"
just ch-query "SELECT name, formatReadableSize(unreserved_space) unreserved, formatReadableSize(total_space) total FROM system.disks"
just ch-query "SELECT count() FROM system.mutations WHERE database = 'nostr' AND table = 'events_local' AND is_done = 0"
# Ingest inserts: synchronous, no errors (~70/hour):
just ch-query "SELECT toStartOfHour(event_time) h, count() n, sum(written_rows) rows, any(Settings['async_insert']) async_setting, countIf(exception != '') errors FROM system.query_log WHERE event_time > now() - INTERVAL 3 HOUR AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart' GROUP BY h ORDER BY h"
just ch-query "SELECT view_name, status, count() FROM system.query_views_log WHERE event_time > now() - INTERVAL 1 HOUR GROUP BY view_name, status ORDER BY view_name"
# When the refreshable views rescan events_local:
just ch-query "SELECT view, status, last_refresh_time, next_refresh_time FROM system.view_refreshes"
```

Record the outputs in the change log.

### 2. Create the tables and views

Apply outside :58–:07 UTC, right **after** an ingest insert burst (step 1's
query shows when they happen). The plan needs the first insert the views see
to start at least 5 s after they were created; an insert racing the creation
makes `plan` refuse (step 3 has the recovery).

```bash
just ch-migrate docs/migrations/prepared/019_api_publishers_longform.sql
```

For the next 30 minutes, watch the two new views next to the existing ones:

```bash
just ch-query "SELECT view_name, status, count(), any(exception) FROM system.query_views_log WHERE event_time > now() - INTERVAL 30 MINUTE AND view_name LIKE 'nostr.%' GROUP BY view_name, status ORDER BY view_name"
just ch-query "SELECT count() n, countIf(exception != '') errors, any(exception), quantile(0.95)(query_duration_ms) p95_ms FROM system.query_log WHERE event_time > now() - INTERVAL 30 MINUTE AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart'"
journalctl -u pensieve-ingest --since "30 min ago" | grep -i clickhouse
```

**Abort:** if a view reports an exception or `events_local` inserts start
failing, run the rollback SQL at once (step 7). Inserts are synchronous, so a
failing view fails the insert and ClickHouse ingest stalls until it is
dropped.

### 3. Plan

After at least one ingest insert has landed (and 10 minutes after step 2,
which `backfill` enforces anyway):

```bash
$AR plan --migration 019 --min-free-gib 100
```

- **Publishers** fixes the view boundary: the first second the view captured.
  Backfill shards read only rows indexed before it; the view owns the rest.
- Each target prints the full-table read (from `system.columns`), a sample
  shard (shard 0 run as a `SELECT`), and the pre-merge and peak disk estimate.
  Expected order of magnitude for 2.43B rows, uncompressed: publishers
  ~320 GiB (id and pubkey at 64 bytes each, plus kind, created_at and
  indexed_at); longform ~860 GiB (adds ~540 GiB of content, extrapolated from
  the 60 GiB per 270M rows the old query read). The plan prints the
  compressed figure. `longform_latest` ends near 466k rows (the plan shows the
  kind 30023 coordinate count from `addressable_latest`). Stop if the peak
  does not fit comfortably in `/data` unreserved space.
- **`Recreate the view`**: an insert raced the view's creation, so the
  boundary is ambiguous. Nothing has been backfilled yet; recover with

  ```bash
  just ch-query "DROP VIEW nostr.publisher_activity_mv"
  just ch-query "TRUNCATE TABLE nostr.publisher_activity"
  just ch-migrate docs/migrations/prepared/019_api_publishers_longform.sql
  ```

  then wait for an insert and `plan --target publishers --discard-progress`.
  The migration refuses to recreate the view on a non-empty table.

### 4. Backfill (bounded, resumable)

```bash
$AR --max-threads 2 --max-memory-gib 8 backfill --migration 019 --max-runtime-mins 120 --delay-secs 5 --min-free-gib 100
$AR status
```

- Shards do not start in the quiet minutes (`--quiet-minutes 58-7` by
  default): the hourly refreshable views rescan `events_local` at :00–:07.
- **Exit code 2** means the session stopped early (runtime, disk guard or
  merge backlog). Re-run the same command later; done shards are skipped.
- Each publisher shard runs **exactly once**: it is one INSERT written as one
  part, and its query id is recorded before it is sent. If a session dies
  mid-shard, the next run settles the shard from `system.query_log` (or
  `system.part_log`). If it cannot (`cannot tell whether … committed`), look
  the query id up by hand:

  ```bash
  just ch-query "SELECT event_time, type, written_rows, exception FROM system.query_log WHERE query_id = '<id>' AND type != 'QueryStart'"
  just ch-query "SELECT event_time, part_name, rows FROM system.part_log WHERE query_id = '<id>' AND event_type = 'NewPart'"
  $AR resolve --target publishers --shard <n> --committed      # or --not-committed
  ```

  Never delete a publisher checkpoint or `pending/` marker by hand: a shard
  that runs twice is counted twice.
- A shard over `--max-memory-gib` fails cleanly (nothing is written); re-plan
  with `--id-prefix-len 3 --discard-progress` only if **no** publisher shard
  has completed, otherwise raise `--max-memory-gib`.
- `status` must show `view boundary … ok`. `VIOLATED` means the table no
  longer matches the plan: stop and investigate.
- **Calibrate:** compare `status` bytes on disk with the plan after the first
  shards; merges shrink the table as they catch up.

### 5. Validate and benchmark

```bash
$AR validate --migration 019 --out /home/pensieve/api-rollups/validate.json
```

- Must exit 0 with `failed rows: 0` and no `INCONCLUSIVE` target.
  `duplicate_inflation` rows are explained (an event inserted twice whose
  duplicate is still visible in `events_local`); `missing`, `excess` and
  `mismatch` are failures.
- The boundary sample reads the whole `indexed_at` column once (minutes at 2
  threads); `--boundary-sample 0` skips it.
- The longform cross-check compares every kind 30023 coordinate's winner with
  `addressable_latest` and must show no disagreement.

Then time the endpoint queries directly, as the API runs them:

```bash
just ch-query "WITH toDate(now() - toIntervalDay(30), 'UTC') AS start_day, if(toStartOfMonth(start_day) = start_day, start_day, toStartOfMonth(start_day) + toIntervalMonth(1)) AS split_day SELECT a.pubkey AS pubkey, sum(a.event_count) AS event_count FROM nostr.publisher_activity AS a WHERE a.kind = -1 AND ((a.grain = 'day' AND a.period >= start_day AND a.period < split_day) OR (a.grain = 'month' AND a.period >= split_day)) GROUP BY pubkey ORDER BY event_count DESC LIMIT 100 SETTINGS max_threads = 4, use_query_cache = 0 FORMAT Null"
just ch-query "SELECT count(), uniqExact(pubkey), sum(tupleElement(l, 3)) FROM (SELECT pubkey, argMinMerge(latest) AS l FROM nostr.longform_latest GROUP BY pubkey, d_tag) SETTINGS max_threads = 4, use_query_cache = 0"
just ch-query "SELECT query_duration_ms, read_rows, formatReadableSize(read_bytes) FROM system.query_log WHERE type = 'QueryFinish' AND event_time > now() - INTERVAL 5 MINUTE AND (query LIKE '%publisher_activity%' OR query LIKE '%longform_latest%') AND query NOT LIKE '%system.query_log%' ORDER BY event_time DESC LIMIT 5"
```

Repeat the first query with `toIntervalDay(60)` (the API's maximum). Both must finish well
under the API's 20 s cap; record the numbers in the change log.

### 6. Deploy the API

Point the `pensieve-api` drop-in's `ExecStart` (the file is still named
`90-postgres-cutover.conf`) at the new
`releases/api-<sha>/pensieve-serve`, `daemon-reload`, restart
`pensieve-api`, check `/proc/<pid>/exe`, and update the drop-in record in
`ops/systemd/dropins/` in the same PR. Then:

```bash
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats/publishers?days=30&limit=5"
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats/publishers?days=365&kind=1&limit=5"
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats/longform"
journalctl -u pensieve-api --since "10 min ago" | grep -i "rollup\|error"
```

`articles_count` drops from all versions to distinct articles (about 466k
from 22k authors on 2026-10-12): this is the intended change.

### 7. Rollback

- **API:** point the drop-in back at the previous `releases/api-*` and
  restart. Those builds scan `events_local` again (and time out, as before).
- **Tables and views:** with the API rolled back, or accepting `503
  metric_unavailable` on both endpoints:

  ```bash
  just ch-migrate docs/migrations/rollback/019_drop_api_publishers_longform.sql
  ```

  Re-applying 019 later needs `plan --discard-progress` and a full backfill.

## Incremental derived views (migration 020)

Removes the three most expensive derived-view costs on `events_local`. Measured on
2026-10-12 over 24 h: `daily_user_stats_mv`'s per-insert lookups took 7.7 s of every ~8.4 s
ingest insert (2,833 inserts, ~6 h a day); `relay_distribution_mv` rescans 1.32 TiB every
6 h and `kinds_stats_mv` 180 GiB every hour, saturating 24 cores at :00–:07 and pushing
insert p95 from ~17 s to ~31 s (69 s at 12:00 UTC). Design, semantics and behaviour
changes: [`docs/api_rollups.md` → Migration 020](../docs/api_rollups.md#migration-020-incremental-derived-views).

**Status:** Applied 2026-10-12: phase 1 at 17:12 UTC (the kinds view recreated at 17:23:20 because the first insert raced it by 4 s; `plan` refused as designed); backfill 17:34–18:50 UTC (user-flags 16 shards, relay-lists and kinds 256 each; ingest 0 errors, p95 ≤ 14.6 s); `validate` 0 failed rows (kinds re-checked after `kinds_stats_mv`'s 19:00 refresh: its first run flagged 6 ephemeral kinds against the hour-old reference, while `kind_totals` matched the exact current counts); API `releases/api-d026d53` at 19:02 UTC; cutover at 19:18:03 UTC (10 statements); post-cutover `validate` 0 failed rows. Effect: `daily_user_stats_mv` 7,698 ms → 2 ms per insert (30.4M → 9k rows read), ingest insert ~8.4 s → 0.23 s average (p95 0.29 s); `relay_distribution` refresh 408 s / 1.32 TiB every 6 h → 5.6 s hourly at :20; `kinds_stats_mv` dropped; active users' `has_*` corrected (2026-10-11: `has_profile` 14,318 → 26,788, `has_follows` 5,103 → 10,410).

Known follow-ups (both addressed by [migration 021](#kind-activity-migration-021)): `/api/v1/kinds/{kind}` still scans `events_local` and times out for large kinds (e.g. kind 1); `validate` compared `kind_totals` with `kinds_stats_mv`, which is up to an hour old, so fast-moving ephemeral kinds failed falsely before the cutover, and after it `unique_pubkeys` was not compared at all — the 021 release compares it with exact counts.

Two phases. Phase 1 (`prepared/020_incremental_derived_views.sql`) only adds tables, three
insert-time views and the `daily_user_stats.last_indexed_at` column; nothing that exists
changes behaviour. Phase 2 (the cutover) switches `daily_user_stats_mv`, the active-users
refreshes and `relay_distribution_mv`, and drops `kinds_stats_mv`. **Order:** phase 1 →
backfill → validate → deploy the API that reads `kind_totals` → cutover. The API never
reads an incomplete table: before its deploy it reads `kinds_stats_mv` as today, after it
reads the backfilled and validated `kind_totals`; `relay_distribution` and the
`active_users_*` tables keep their names and are replaced atomically by each refresh.

Use the `api-rollups` binary from the release that contains this migration, as for 019
(`$AR` below is the 019 alias with the same `--checkpoint-dir`; 020 targets get their own
subdirectories). Every tool query carries `--max-threads` (default 2),
`--max-memory-gib` (default 8) and `--max-execution-secs` and is tagged
`log_comment = 'api-rollups…'`.

### 1. Preflight (read-only)

```bash
just ch-query "SELECT version()"
just ch-query "SELECT name, formatReadableSize(unreserved_space) unreserved, formatReadableSize(total_space) total FROM system.disks"
just ch-query "SELECT count() FROM system.mutations WHERE database = 'nostr' AND table IN ('events_local', 'daily_user_stats') AND is_done = 0"
# Baseline cost per view (expect daily_user_stats_mv ~7.7 s per insert):
just ch-query "SELECT view_name, count() n, round(avg(view_duration_ms)) avg_ms, max(view_duration_ms) max_ms, round(avg(read_rows)) avg_read FROM system.query_views_log WHERE event_time > now() - INTERVAL 24 HOUR GROUP BY view_name ORDER BY avg_ms DESC"
# Baseline insert latency and the refresh rescans:
just ch-query "SELECT toStartOfHour(event_time) h, count() n, quantile(0.95)(query_duration_ms) p95_ms, countIf(exception != '') errors FROM system.query_log WHERE event_time > now() - INTERVAL 24 HOUR AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart' GROUP BY h ORDER BY h"
just ch-query "SELECT view, status, last_success_time, duration_ms, formatReadableSize(read_bytes) read, next_refresh_time FROM system.view_refreshes WHERE database = 'nostr' ORDER BY view"
# Who reads kinds_stats_mv (the API; anything else must move to kind_totals before the cutover):
just ch-query "SELECT user, client_name, http_user_agent, count() FROM system.query_log WHERE event_date >= today() - 1 AND type = 'QueryFinish' AND query_kind = 'Select' AND has(tables, 'nostr.kinds_stats_mv') GROUP BY ALL"
just ch-query "SELECT count() FROM nostr.pubkeys_with_profile_data"
just ch-query "SELECT count() FROM nostr.pubkeys_with_follows_data"
```

Record the outputs in the change log. `relay_distribution_mv`'s `duration_ms` calibrates
the relay-lists backfill (it reads the same columns, on all cores).

### 2. Apply phase 1

Outside :58–:07 UTC, right **after** an ingest insert burst: the kinds plan needs the first
insert `kind_totals_mv` sees to start at least 5 s after the view was created.

```bash
just ch-migrate docs/migrations/prepared/020_incremental_derived_views.sql
```

For 30 minutes, watch the three new views (expect a few ms each) and insert latency:

```bash
just ch-query "SELECT view_name, status, count(), round(avg(view_duration_ms)) avg_ms, any(exception) FROM system.query_views_log WHERE event_time > now() - INTERVAL 30 MINUTE AND view_name IN ('nostr.pubkey_flags_mv', 'nostr.relay_list_latest_mv', 'nostr.kind_totals_mv', 'nostr.daily_user_stats_mv') GROUP BY view_name, status ORDER BY view_name"
just ch-query "SELECT count() n, countIf(exception != '') errors, any(exception), quantile(0.95)(query_duration_ms) p95_ms FROM system.query_log WHERE event_time > now() - INTERVAL 30 MINUTE AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart'"
journalctl -u pensieve-ingest --since "30 min ago" | grep -i clickhouse
```

**Abort:** a view exception or failing inserts → run
`docs/migrations/rollback/020_drop_incremental_derived_views.sql` at once (inserts are
synchronous; a failing view stalls ClickHouse ingest).

### 3. Plan

After at least one ingest insert and 10 minutes after step 2:

```bash
$AR plan --target user-flags --id-prefix-len 1 --min-free-gib 100
$AR plan --target relay-lists --target kinds --min-free-gib 100
```

- **kinds** fixes the view boundary (first second `kind_totals_mv` captured). On
  `Recreate the view`, nothing is backfilled yet:
  `just ch-query "DROP VIEW nostr.kind_totals_mv"`,
  `just ch-query "TRUNCATE TABLE nostr.kind_totals"`, re-apply phase 1, wait for an
  insert, `plan --target kinds --discard-progress`.
- Each target prints the source read, a sample shard (with its duration), the wall-clock
  estimate and the disk estimate. Expected: user-flags reads ~30M lookup rows (minutes);
  relay-lists reads tags of nearly every granule, about one 014 rescan (~1.3–1.5 TiB
  uncompressed, ~2–3 h at 2 threads); kinds ~320 GiB uncompressed (~1.5–2 h, like 019's
  publishers). Stop if the peak does not fit comfortably in `/data`.

### 4. Backfill (bounded, resumable)

```bash
$AR --max-threads 2 --max-memory-gib 8 backfill --migration 020 --max-runtime-mins 120 --delay-secs 5 --min-free-gib 100
$AR status
```

Same rules as 019: no shard starts at :58–:07; exit code 2 means stopped early (re-run);
**kinds** shards run exactly once (a lost outcome is settled from `system.query_log` /
`system.part_log`, else `$AR resolve --target kinds --shard <n> --committed|--not-committed`
after checking by hand); never delete a kinds checkpoint or `pending/` marker.
user-flags and relay-lists are idempotent. `status` must show `view boundary … ok` for
kinds. Watch insert p95 and the disk during the relay-lists shards (they read `tags`).

### 5. Validate

```bash
$AR validate --migration 020 --out /home/pensieve/api-rollups/validate-020.json
```

- Must exit 0 with `failed rows: 0` and no `INCONCLUSIVE` target.
- **user-flags**: lookups and `pubkey_flags` agree, stored flags of the last 7 days are
  backed, sampled `daily_user_stats` counts match `events_local`, and the cutover join
  reproduces the stored flags exactly. The notes print `active_users_daily` for the last
  days under 013's rule (`= 1`, published today), stored flags (`> 0`) and the join:
  **record the difference** — after the cutover `has_profile`/`has_follows`/`has_both`
  move from the first to the last (see docs; active users and events do not change).
- **relay-lists**: sampled winners match; the note compares the distribution the cutover
  will publish with the current table (differences = lists since the last 014 refresh,
  and `created_at` ties).
- **kinds**: one scan of `kind`/`indexed_at`/`created_at` (a few minutes at 2 threads);
  backfill share exact. Until the 021 release `unique_pubkeys` was compared within 2% of
  `kinds_stats_mv`; it is now compared with exact counts for sampled kinds (see
  [Kind activity → Validate](#kind-activity-migration-021)).

### 6. Deploy the API

Build and point the `pensieve-api` drop-in at the release as in
[API rollups → Deploy the API](#6-deploy-the-api) (update the drop-in record in the same
PR). Then:

```bash
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/kinds?limit=5"
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats/kinds/total"
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats"
journalctl -u pensieve-api --since "10 min ago" | grep -i "rollup\|error"
```

`/kinds` `unique_pubkeys` is now approximate (~0.3% standard error); counts are current.

### 7. Cut over

At least 15 minutes after the deploy (the API caches `/kinds` for 5 minutes), outside
:58–:07, right after an ingest burst:

```bash
$AR cutover --dry-run
$AR cutover
```

It refuses unless the three backfills are complete and nothing outside the tool read
`kinds_stats_mv` in the last 15 minutes (step 1's query shows who). It recreates the three
active-users refreshes and `relay_distribution_mv` (each runs once immediately), switches
`daily_user_stats_mv` with `MODIFY QUERY` (no gap, no double count), drops
`kinds_stats_mv`, and records the time in `cutover-020.json`. A failure names the statement;
the statements are idempotent, so fix and re-run, or roll back (step 9). Then:

```bash
just ch-query "SELECT view, status, last_success_time, duration_ms, exception FROM system.view_refreshes WHERE database = 'nostr' ORDER BY view"
just ch-query "SELECT as_select FROM system.tables WHERE database = 'nostr' AND name = 'daily_user_stats_mv'"
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats/users/active"
curl -s -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/stats/relays/distribution?limit=5"
```

### 8. Validate again and monitor

After an hour (`validate` now also samples pubkeys active around the cutover and checks
`last_indexed_at` of rows inserted after it):

```bash
$AR validate --migration 020 --out /home/pensieve/api-rollups/validate-020-after.json
```

Over the next day, compare with step 1's baseline:

- `system.query_views_log`: `daily_user_stats_mv` from ~7.7 s to milliseconds per insert;
  the three new views a few ms each.
- Insert p95 per hour (step 1 query): no :00 spike to ~31 s / 69 s; overall well below
  the ~17 s baseline.
- `system.view_refreshes`: no `kinds_stats_mv`; `relay_distribution_mv` hourly at :20,
  `active_users_*_mv` succeed (`duration_ms` ~seconds, `exception` empty).
- CPU at :00–:07 (Grafana/Prometheus node metrics): no longer saturated.

### 9. Rollback

- **Cutover** (restores 005's insert-time lookups, 013's refreshes, 014's 6-hourly rescan
  with `EMPTY` so `relay_distribution` keeps its rows until the next 00/06/12/18 run, and
  015's `kinds_stats_mv`, which refreshes at once):

  ```bash
  just ch-migrate docs/migrations/rollback/020_undo_derived_views_cutover.sql
  ```

  Wait for `kinds_stats_mv`'s first successful refresh (`system.view_refreshes`) before
  pointing the API back at a pre-020 release. `daily_user_stats` rows inserted between the
  cutover and the rollback store flags as 0, so `has_*` undercount those days until they
  leave the refresh windows (active users and events are unaffected).
- **Phase 1** (after the cutover is undone, and with an API that does not read
  `kind_totals`, which otherwise answers 503 on `/kinds`, `/stats/kinds/total`,
  `/stats/events/earliest` and `/stats`; the file refuses while a switched view still
  reads a 020 object):

  ```bash
  just ch-migrate docs/migrations/rollback/020_drop_incremental_derived_views.sql
  ```

  Re-applying 020 later needs `plan --migration 020 --discard-progress` (after
  `TRUNCATE TABLE nostr.kind_totals`, which the migration's guard requires) and a full
  backfill.

## Kind activity (migration 021)

Moves `GET /api/v1/kinds/{kind}` and `GET /api/v1/kinds/{kind}/activity` off scans of
`events_local` by kind (kind 1 answered `500 database_error` after the 20 s cap on
2026-10-12) onto `kind_activity`, a small insert-fed table of per kind and UTC hour (last
35 days), day and month counts, content bytes and author sketches; `/kinds/{kind}` takes
its totals from 020's `kind_totals`. Design, time basis and behaviour changes:
[`docs/api_rollups.md` → Migration 021](../docs/api_rollups.md#migration-021-kind-activity).
The same release replaces `validate`'s kinds `unique_pubkeys` check (it compared with
`kinds_stats_mv`, gone since the 020 cutover) with an exact sampled comparison.

**Status:** Applied 2026-10-13 at 06:14:16 UTC at a quiet moment (view boundary
`indexed_at < 1791872126`; the view costs ~2 ms per insert). `plan` 06:24 UTC (256 shards,
~1.36 TiB compressed read, peak disk ~79 GiB estimated); backfill ~06:25–07:30 UTC
(80,314,740 rows written, 4.8M rows / 1.49 GiB after merges; ingest 0 errors, insert p95
≤ 8.5 s). `validate --migration 021` and `validate --target kinds` both 0 failed rows
(kinds: largest `unique_pubkeys` sketch error 0.271%, 2 heavy kinds skipped). The API
`api-fe54f04` reads it since 07:36 UTC: `/kinds/1` went from `500` after 20.0 s to 200 in
33 ms; `/kinds/1/activity?group_by=day&limit=365` 0.81 s;
`/kinds/7/activity?group_by=month&limit=120` 0.43 s.

**Order:** apply → plan → backfill → validate → deploy the API that reads `kind_activity`
(the previous API did not read it, so nothing served an incomplete table) → monitor.

Use the `api-rollups` binary from the release that contains this migration (`$AR` as for
019/020, same `--checkpoint-dir`; the target gets its own `kind-activity/` subdirectory).
Every tool query carries `--max-threads` (default 2), `--max-memory-gib` (default 8) and
`--max-execution-secs` and is tagged `log_comment = 'api-rollups…'`.

### 1. Preflight (read-only)

```bash
just ch-query "SELECT version(), timezone()"
just ch-query "SELECT name, formatReadableSize(unreserved_space) unreserved, formatReadableSize(total_space) total FROM system.disks"
just ch-query "SELECT count() FROM system.mutations WHERE database = 'nostr' AND table = 'events_local' AND is_done = 0"
# Baseline per-view cost (kind_totals_mv is the closest analogue: expect a few ms):
just ch-query "SELECT view_name, count() n, round(avg(view_duration_ms)) avg_ms, max(view_duration_ms) max_ms FROM system.query_views_log WHERE event_time > now() - INTERVAL 24 HOUR GROUP BY view_name ORDER BY avg_ms DESC"
# Baseline insert latency (~0.23 s average since the 020 cutover); note when bursts land:
just ch-query "SELECT toStartOfHour(event_time) h, count() n, quantile(0.95)(query_duration_ms) p95_ms, countIf(exception != '') errors FROM system.query_log WHERE event_time > now() - INTERVAL 3 HOUR AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart' GROUP BY h ORDER BY h"
# The endpoint today (expect a timeout for kind 1):
curl -s -o /dev/null -w '%{http_code} %{time_total}s\n' -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/kinds/1"
```

`timezone()` should be `UTC`: the old activity periods used the server time zone, the new
ones UTC. Record the outputs in the change log.

### 2. Apply

Outside :58–:07 UTC, right **after** an ingest insert burst: `plan` needs the first insert
`kind_activity_mv` sees to start at least 5 s after the view was created.

```bash
just ch-migrate docs/migrations/prepared/021_kind_activity.sql
```

For 30 minutes, watch the new view (expect a few ms per insert, like `kind_totals_mv`) and
insert latency:

```bash
just ch-query "SELECT view_name, status, count(), round(avg(view_duration_ms)) avg_ms, max(view_duration_ms) max_ms, any(exception) FROM system.query_views_log WHERE event_time > now() - INTERVAL 30 MINUTE AND view_name IN ('nostr.kind_activity_mv', 'nostr.kind_totals_mv') GROUP BY view_name, status ORDER BY view_name"
just ch-query "SELECT count() n, countIf(exception != '') errors, any(exception), quantile(0.95)(query_duration_ms) p95_ms FROM system.query_log WHERE event_time > now() - INTERVAL 30 MINUTE AND query_kind = 'Insert' AND has(tables, 'nostr.events_local') AND type != 'QueryStart'"
journalctl -u pensieve-ingest --since "30 min ago" | grep -i clickhouse
```

**Abort:** a view exception or failing inserts → run
`docs/migrations/rollback/021_drop_kind_activity.sql` at once (inserts are synchronous; a
failing view stalls ClickHouse ingest).

### 3. Plan

After at least one ingest insert and 10 minutes after step 2:

```bash
$AR plan --migration 021 --min-free-gib 100
```

- Fixes the view boundary (first second `kind_activity_mv` captured) and prints the hour
  window (hours newer than the boundary − 35 days). On `Recreate the view`, nothing is
  backfilled yet: `just ch-query "DROP VIEW nostr.kind_activity_mv"`,
  `just ch-query "TRUNCATE TABLE nostr.kind_activity"`, re-apply step 2, wait for an
  insert, `plan --migration 021 --discard-progress`.
- Expected: id, pubkey, kind, created_at, indexed_at **and content** of the whole table,
  ~860 GiB uncompressed (the longform read of 019, ~50 min there) plus a
  `GROUP BY kind, grain, period` with sketches; wall-clock ~1–2 h at 2 threads (sample shard
  × 256 plus pauses and quiet minutes, printed). The disk estimate assumes 512 B per
  pre-merge row (conservative; ≤ ~0.5M rows per shard). Stop if the peak does not fit
  comfortably in `/data`.

### 4. Backfill (bounded, resumable)

```bash
$AR --max-threads 2 --max-memory-gib 8 backfill --migration 021 --max-runtime-mins 120 --delay-secs 5 --min-free-gib 100
$AR status
```

Same rules as 019/020: no shard starts at :58–:07; exit code 2 means stopped early
(re-run); every shard runs **exactly once** (a lost outcome is settled from
`system.query_log` / `system.part_log`, else
`$AR resolve --target kind-activity --shard <n> --committed|--not-committed` after checking
by hand); never delete a `kind-activity` checkpoint or `pending/` marker. `status` must show
`view boundary … ok`. Watch insert p95 during the shards (they read `content`).

### 5. Validate

```bash
$AR validate --migration 021 --out /home/pensieve/api-rollups/validate-021.json
$AR validate --target kinds --out /home/pensieve/api-rollups/validate-kinds-021.json
```

- Both must exit 0 with `failed rows: 0` and no `INCONCLUSIVE` target.
- **kind-activity**: every (kind, day), (kind, month) and recent (kind, hour) count — backfill
  share exact, view share bracketed by the cutoff — from one scan of
  `kind`/`created_at`/`indexed_at` (~25 GiB, minutes; ~1–3M groups returned); content bytes
  and day sketches of seeded random kinds within `--content-budget-events` (5,000 events,
  ≤ 5,000 granules of `content`). Record the notes (group counts, largest sketch error).
- **kinds** (the new exact `unique_pubkeys` check, plus the counts): one scan of
  `kind`/`indexed_at`/`created_at`, then one `PREWHERE kind IN (…)` scan over 12 sampled
  kinds that reads `pubkey` of every granule once kind 1 is sampled (~150 GiB uncompressed,
  ~10–30 min at 2 threads, ~40 B memory per (kind, pubkey), spilling beyond 4 GiB).
  Kinds above `--pubkeys-max-exact` (20M approximate pubkeys, e.g. 1059/445) are left out
  and listed; `--pubkeys-kind K` adds one, `--pubkeys-kinds 0` skips the scan. The note
  prints the largest relative error (expect ≤ ~0.5%; fast-moving kinds are bracketed
  between the cutoff and the scan, so they no longer fail falsely).

### 6. Deploy the API

Build and point the `pensieve-api` drop-in at the release as in
[API rollups → Deploy the API](#6-deploy-the-api) (update the drop-in record in the same
PR). Then:

```bash
curl -s -w '\n%{http_code} %{time_total}s\n' -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/kinds/1"
curl -s -w '\n%{http_code} %{time_total}s\n' -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/kinds/1/activity?group_by=day&limit=365"
curl -s -w '\n%{http_code} %{time_total}s\n' -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/kinds/7/activity?group_by=month&limit=120"
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $TOKEN" "http://localhost:8080/api/v1/kinds/65535"   # 404 unless kind 65535 exists
journalctl -u pensieve-api --since "10 min ago" | grep -i "rollup\|error"
just ch-query "SELECT query_duration_ms, read_rows, formatReadableSize(read_bytes) FROM system.query_log WHERE type = 'QueryFinish' AND event_time > now() - INTERVAL 10 MINUTE AND has(tables, 'nostr.kind_activity') AND query NOT LIKE '%system.query_log%' ORDER BY event_time DESC LIMIT 10"
```

Expect `/kinds/1` in tens of milliseconds (thousands of rows read) and the activity series
well under a second. `unique_pubkeys` is now approximate (the `/kinds` figure); the 24 h /
7 d / 30 d windows start at the UTC hour containing `now() - N days`.

### 7. Monitor

Over the next day: `kind_activity_mv` a few ms per insert in `system.query_views_log`, no
view exceptions, insert p95 unchanged against step 1; `$AR status` lag small and the
boundary `ok`; no `database_error` for `/kinds/*` in the API log. The hour rows' TTL runs at
most once a day (`merge_with_ttl_timeout`); `kind_activity` stays at a few GiB:

```bash
just ch-query "SELECT grain, count(), min(period), max(period) FROM nostr.kind_activity GROUP BY grain"
just ch-query "SELECT formatReadableSize(sum(bytes_on_disk)), sum(rows), count() FROM system.parts WHERE database = 'nostr' AND table = 'kind_activity' AND active"
```

### 8. Rollback

- **API:** point the drop-in back at the previous `releases/api-*` and restart; those builds
  scan `events_local` again (and time out for large kinds, as before).
- **Table and view:** with the API rolled back, or accepting `503 metric_unavailable` on
  `/kinds/{kind}` and `/kinds/{kind}/activity`:

  ```bash
  just ch-migrate docs/migrations/rollback/021_drop_kind_activity.sql
  ```

  Re-applying 021 later needs `plan --migration 021 --discard-progress` and a full backfill
  (the migration's guard refuses a non-empty table without its view).

## Grafana

Provisioning is no longer in the repo. Configure the Prometheus datasource
(`http://prometheus:9090`) and dashboards directly on the running instance at
`https://<DOMAIN>/grafana/`. Plugins (ClickHouse, SQLite datasources) are still
installed at container start. See the ingestion/coverage dashboard guide handed off
separately for the panel set.

## Negentropy reconciliation

The ingester's isolated NIP-77 reconciliation (`--isolated-negentropy`, a separate
`pensieve-negentropy-worker` process over `/run/pensieve/negentropy.sock`, provisioned by
`ops/systemd/pensieve-negentropy-tmpfiles.conf`) is rolled out behind explicit gates; it is
not covered by the routine deploy above. Since 2026-10-12 16:36 CEST the ingester runs
`releases/ingest-b466b37` (master, incl. #72 and #73) from `zzzz-bounded-canary.conf` with the rolling Premium Primal ledger;
the worker runs continuously against Premium Primal only since 2026-10-12 15:22 UTC and is started with the ingester (`negentropy-worker.conf`); see [continuous readiness](../docs/negentropy_continuous_readiness.md#continuous-premium-primal-operation-since-2026-10-12). Expect these drop-ins to change. See [`docs/negentropy_runtime.md`](../docs/negentropy_runtime.md)
and [`docs/negentropy_continuous_readiness.md`](../docs/negentropy_continuous_readiness.md).
Its Kuma probe is `pensieve-negentropy-kuma.timer` (`/health/negentropy`); the
archive-admission probe is `pensieve-archive-admission-kuma.timer` (`/health/archive-admission`).
Both were active on 2026-10-11. Their scripts are `ops/scripts/negentropy-kuma.py`
and `ops/scripts/archive-admission-kuma.py`, installed to `/usr/local/libexec/pensieve-*-kuma.py`.

### Monitor 46 red

Read the fixed reason first: `curl -s localhost:9091/health/negentropy`.
`LEDGER=/data/negentropy/production-premium-20261011T180545Z/jobs.sqlite`.

- `unresolved_failures`: a job is blocked or in retry backoff after three failed
  attempts. First read two gauges on `curl -s localhost:9091/metrics`:
  `negentropy_isolated_failure_pause_exempt_jobs` counts jobs whose relay keeps
  reporting the same advertised-but-undelivered IDs and that are on track to a
  relay-unavailable exclusion; `negentropy_isolated_failure_pause_counted_jobs`
  counts every other unresolved job. With `counted_jobs` 0, wait: an exempt job is
  completed automatically (`negentropy relay-unavailable exclusion` WARN lines; see
  [continuous readiness](../docs/negentropy_continuous_readiness.md#relay-unavailable-exclusions))
  at attempt 6 or 7 if its reports are uninterrupted (about an hour after its
  third failure). It stays exempt through attempt 10 at most (about four hours
  after its third failure); after that it counts like any other failure, although
  a later exclusion can still complete it. With
  `counted_jobs` above 0 it is a different fault. Stop only the worker (`sudo systemctl stop pensieve-negentropy-worker`),
  capture `sudo -u pensieve python3 ops/scripts/negentropy-ledger-status.py "$LEDGER"`
  and the `isolated worker attempt failed` journal lines, and leave the monitor red
  for a decision. If the ingester runs with
  `--isolated-negentropy-pause-on-failure-secs`, the runtime stops leasing once one
  counted job has failed continuously for the grace (`isolated reconciliation
  paused leasing` in the journal); stopping the worker is then optional.
- `runtime_failed`: the isolated runtime stopped; only an ingester restart clears
  it. Stop the worker (it otherwise restarts every 5 s with `worker IPC failed`
  because the runtime removed its socket), find the cause in
  `journalctl -u pensieve-ingest | grep 'isolated reconciliation paused'`, and do not
  restart until the cause is understood or fixed. Never edit, reset or delete the
  ledger.

### Recovery from the 2026-10-13 lease race

`invalid parent session request` at 06:01:22Z was the lease race described in
[continuous readiness](../docs/negentropy_continuous_readiness.md#incident-2026-10-13-runtime-stopped-on-a-lease-race).
The ledger is consistent: job 768 is `leased` (attempt 16, expiry 06:11:22Z) with
no session and no receipts. Any runtime start after that expiry runs `expire` on
its first tick, which moves job 768 to `retry_wait` (`worker_lost`, due 60 s later)
without touching receipts; before expiry the live lease only blocks new leases.
Only the ingester changes; the worker binary and IPC version are unchanged, so the
installed `ba0c4a1` worker stays. Restarting the old `ingest-b466b37` build would
resume safely but can hit the same race again whenever a gap becomes due during a
preflight scan.

1. Stop the worker: `sudo systemctl stop pensieve-negentropy-worker`.
2. Keep the ledger copy taken after the stop as incident evidence, and record
   `negentropy-ledger-status.py "$LEDGER"` once more. Do not modify the live ledger.
3. Build the fixed master commit as a release (see Routine deploy; ingester only):
   `releases/ingest-<sha>/pensieve-ingest`.
4. Edit `zzzz-bounded-canary.conf`: point `ExecStart` at the new release, keep every
   existing argument, and decide on the pause flag. Job 768 is still unresolved, so
   with `--isolated-negentropy-pause-on-failure-secs` leasing stops again after the
   grace; omit it until the job is decided if reconciliation should keep running.
5. `sudo systemctl daemon-reload`, then restart the ingester with the graceful-stop
   checks above (`TimeoutStopSec=15min`). `negentropy-worker.conf` starts the worker
   with it; otherwise `sudo systemctl start pensieve-negentropy-worker`.
6. Verify: the journal shows `isolated worker session budget` lines and no new
   `isolated reconciliation paused`; `negentropy_isolated_ready` is 1 and
   `negentropy_isolated_unhealthy` 0; the status tool shows `states.leased` 0 or 1,
   job 768 back in `retry_wait`, and `last_success` advancing. `/health/negentropy`
   will report `unresolved_failures` (not `runtime_failed`) while job 768 remains
   unresolved. The relay-unavailable exclusion rollout below resolves it.
7. Record the new release in `ops/systemd/dropins/` and the table above.

### Rolling out relay-unavailable exclusions (ledger schema 10)

The maintainer decision for job 768 (2026-10-13): a window completes, with a
recorded proof, once its relay has reported the same fully enumerated set of
advertised-but-undelivered IDs on five consecutive attempts over at least an hour
(see [continuous readiness](../docs/negentropy_continuous_readiness.md#relay-unavailable-exclusions)).
Only the ingester changes; the worker binary and IPC version are unchanged, so the
installed worker stays. Opening the ledger migrates it from schema 9 to 10. A
pre-change ingester refuses a schema-10 ledger (see
[rolling back](#rolling-back-relay-unavailable-exclusions)).

1. Stop the worker, then take a consistent ledger backup with the ingester still
   running: `sudo -u pensieve sqlite3 "$LEDGER" ".backup '$LEDGER.pre-schema10'"`.
   Record `negentropy-ledger-status.py "$LEDGER"` (it shows `ledger_schema` 9 and
   job 768 in `retry_wait`). The backup is only for the downgrade path under
   [rolling back](#rolling-back-relay-unavailable-exclusions); keep it until the
   new build has run for a few days.
2. Build the merged master commit as `releases/ingest-<sha>/pensieve-ingest` (see
   Routine deploy; ingester only).
3. Edit `zzzz-bounded-canary.conf`: point `ExecStart` at the new release and keep
   every existing argument. The defaults need no new flags. Do not add
   `--isolated-negentropy-pause-on-failure-secs` with this build: its global pause
   clock can pause before an exclusion lands (fixed by the follow-up release
   below). Spell out the defaults
   (`--isolated-negentropy-unavailable-exclusion-attempts 5`,
   `--isolated-negentropy-unavailable-exclusion-span-secs 3600`,
   `--isolated-negentropy-unavailable-exclusion-max-ids 8`) only if the drop-in
   should record them.
4. `sudo systemctl daemon-reload`, restart the ingester with the graceful-stop
   checks above; the worker starts with it.
5. Verify within a minute of startup: one journal line
   `negentropy relay-unavailable exclusion` with `job=768`,
   `relay=wss://premium.primal.net`, `count=1`,
   `event_id=089d1fdc2d1cf0f03be5844014378a3f2bcca0d1092619a70ed6f86b7c286b65`,
   `first_attempt=1` and `last_attempt` the latest attempt with a report (15 if
   none ran since the lease race; attempts without a report are skipped);
   `negentropy_relay_unavailable_exclusions_total` 1 and
   `negentropy_relay_unavailable_exclusions_unresolved` 1 on `:9091/metrics`;
   `curl -s localhost:9091/health/negentropy` no longer `unresolved_failures`, and
   Kuma monitor 46 green after its next push; the status tool shows `ledger_schema`
   10, `relay_unavailable_exclusions.events` 1 with job 768 in `recent`, and
   `complete_states.relay_unavailable_exclusion` 1. `isolated worker session
   budget` lines continue and `last_durable_completion_unix` advances. If no
   exclusion line appears, job 768's latest reports differ (for example a `relay`
   failure since the incident): it is excluded once five consecutive identical
   reports span an hour, and health stays `unresolved_failures` until then.
6. Record the release in `ops/systemd/dropins/` and the table above.

Every later exclusion logs the same WARN line; check the listed event ID against
other relays. If one ever looks wrong, roll back as below and keep the ledger for
review. Never delete exclusion rows or edit jobs by hand.

### Exclusion follow-ups: worker re-request and per-job pause clock

The release after #90 changes both binaries; the IPC version and ledger schema
(10) stay, so either binary can be upgraded alone, but deploy both:

- **Worker.** Before reporting `Unavailable` the worker re-requests exactly the
  still-missing IDs (at least once, and again while each request delivers
  something), so a relay that caps one REQ's results below the 128-ID batch is no
  longer reported as withholding the tail. The installed worker (`ba0c4a1`) lacks
  this, and the parent cannot tell the difference: until it is replaced, a capped
  relay could still get a window wrongly excluded.
- **Ingester.** The failure pause uses a per-job clock and exempts jobs on track to
  an exclusion; IDs already archived (for example before the replay floor) count
  as delivered, and a window whose withheld IDs are all archived completes as
  `already_archived`. `--isolated-negentropy-pause-on-failure-secs 7200` is safe to
  enable from this build on.

1. Stop the worker: `sudo systemctl stop pensieve-negentropy-worker`. Record
   `negentropy-ledger-status.py "$LEDGER"`.
2. Build the merged master commit as in Routine deploy, both binaries:
   `cargo build --release --locked -p pensieve-ingest --bin pensieve-ingest --bin pensieve-negentropy-worker`.
   Install the ingester as `releases/ingest-<sha>/pensieve-ingest`. Keep the old
   worker (`sudo cp -p /usr/local/libexec/pensieve-negentropy-worker
   /usr/local/libexec/pensieve-negentropy-worker.ba0c4a1`), then
   `sudo install -m 755 <target>/release/pensieve-negentropy-worker /usr/local/libexec/`.
3. Edit `zzzz-bounded-canary.conf`: point `ExecStart` at the new release, keep every
   existing argument, and add `--isolated-negentropy-pause-on-failure-secs 7200` if
   unattended pausing on persistent failures is wanted. No other flag is needed.
4. `sudo systemctl daemon-reload`, then restart the ingester with the graceful-stop
   checks above; the worker starts with it.
5. Verify: `readlink /proc/$(systemctl show -p MainPID --value pensieve-ingest)/exe`
   is the new release; `sha256sum /usr/local/libexec/pensieve-negentropy-worker`
   matches the build output and the worker's `ActiveEnterTimestamp`
   (`systemctl show -p ActiveEnterTimestamp pensieve-negentropy-worker`) is after
   the install; `isolated worker session budget` lines continue;
   `negentropy_isolated_failure_pause_exempt_jobs` and
   `negentropy_isolated_failure_pause_counted_jobs` appear on `:9091/metrics`; with
   the flag, `negentropy_isolated_failure_pause` is 0. Worker journal lines
   `isolated fetch progress` now carry `requested` and `delivered`.
6. Record the release in `ops/systemd/dropins/` and the table above.

Roll back by reinstalling the saved worker and pointing the drop-in at the
previous `releases/ingest-*`; both read schema 10, so no ledger change is needed.
Remove the pause flag when going back to a #90 build.

### Rolling back relay-unavailable exclusions

A pre-#90 ingester (for example `ingest-b466b37`) refuses a schema-10 ledger with
`unsupported ledger identity/version`: it leaves the isolated runtime off
(`/health/negentropy` `runtime_failed`, Kuma monitor 46 red) while live ingestion
continues. So there are two paths:

1. **Keep the new build, stop new exclusions (preferred).** Add
   `--isolated-negentropy-disable-unavailable-exclusion` to `zzzz-bounded-canary.conf`
   (it conflicts with the three `-unavailable-exclusion-*` tuning flags; remove
   those), `daemon-reload` and restart the ingester with the graceful-stop checks.
   Recorded exclusions and `already_archived` completions stay complete; new
   matching jobs keep retrying. Without exclusions a withheld window holds
   `unresolved_failures` again, so drop `--isolated-negentropy-pause-on-failure-secs`
   too unless a pause on it is wanted.
2. **Downgrade the binary and restore the backup.** Only if the new build itself
   must go. Stop the worker, then stop the ingester with the graceful-stop checks
   (both must stay stopped until step 4). Check
   `negentropy-ledger-status.py "$LEDGER"` shows `unresolved_receipts` 0; if not, do
   not restore (receipts not yet archive-confirmed would be lost): use path 1.
   Then, as `pensieve`: `mv "$LEDGER" "$LEDGER.schema10"` (also `-wal`/`-shm` if
   present) and `cp -p "$LEDGER.pre-schema10" "$LEDGER"`. Point the drop-in at the
   pre-#90 release, remove every flag that build does not know (all
   `-unavailable-exclusion` flags), `daemon-reload` and start the ingester. This
   discards every exclusion and all reconciliation progress since the upgrade:
   job 768 is back in `retry_wait` and holds `unresolved_failures` again, windows
   planned since then are planned again, and job IDs issued since the upgrade are
   issued again for other windows. Keep `$LEDGER.schema10` for reference.

## Cleanup debt
- `ops/scripts/sync-archive.sh` still uploads + prunes `*.notepack` segments; reconcile it
  with Parquet retention now that the live shadow and historical campaign exist (see
  [`docs/migration-plan.md`](../docs/migration-plan.md)).
- `/api/v1/stats/publishers` is capped at `days=60`; a longer window needs a precomputed
  ranking (see [API rollups](#api-rollups-migration-019) → Known limit).
- The API reads `RELAY_DB_PATH` through one long-lived `immutable=1` SQLite connection while the
  ingester keeps writing the WAL database, so relay endpoints can be stale.
- `ops/systemd/*.service` `ExecStart` lines do not match production; the drop-ins in
  [`ops/systemd/dropins/`](systemd/dropins/README.md) are the record.
