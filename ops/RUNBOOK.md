# Pensieve Ops Runbook

How the production box is actually operated (`ssh pensieve`, user `pensieve`).
The repo checkout is `/home/pensieve/pensieve`; default branch `master`. The
checkout provides the Compose file and the ops scripts, but **not** the service
binaries: those are release builds selected by systemd drop-ins (see
[Routine deploy](#routine-deploy)). Facts marked with a date were observed on the
host on that date.

## Layout

- `ops/production/compose.yml` — Docker stack: ClickHouse, Prometheus, Grafana, Caddy.
  (`master` dropped the never-started `postgres-analytics` service on 2026-10-12; the
  host checkout still defines it until fast-forwarded, see
  [Postgres analytics leftovers](#postgres-analytics-leftovers).)
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
| `pensieve-serve` | `pensieve-api.service` + `90-postgres-cutover.conf` | `~/pensieve/releases/api-8bdac94/pensieve-serve` (master, since 2026-10-12) |
| `pensieve-preview` | `pensieve-preview.service` + `10-release.conf` | `~/pensieve/releases/preview-3ab1045/pensieve-preview` |
| `pensieve-ingest` | `pensieve-ingest.service` + 5 drop-ins | `/data/pensieve-build/negentropy-ba0c4a1/release/pensieve-ingest` (PR #72, since 2026-10-12 08:29 CEST) with `--isolated-negentropy` (from `zzzz-bounded-canary.conf`, which also sets `Restart=no`); `stop-timeout.conf` sets `TimeoutStopSec=15min` |
| isolated NIP-77 worker | `pensieve-negentropy-worker.service` (static) | `/usr/local/libexec/pensieve-negentropy-worker` |
| archive sync (hourly) | `archive-sync.timer` → `archive-sync.service` | `~/pensieve/ops/scripts/sync-archive.sh` |
| shadow analytics refresh | `pensieve-analytics-refresh.timer` (**disabled**, obsolete) | removed from `master` 2026-10-12; see [Postgres analytics leftovers](#postgres-analytics-leftovers) |
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
`events_local` at those times: `kinds_stats_mv` and `active_users_daily_mv` every
hour, and `relay_distribution_mv` (1.3 TiB read) at 00/06/12/18 UTC. They saturate
the CPU, ingest inserts slow from ~8 s to ~50 s per batch, and shutdown waits for
the in-flight segment to finish indexing. On 2026-10-12 a stop at 06:04 UTC
exceeded the then 90 s `TimeoutStopSec` and was killed; `stop-timeout.conf` now
allows 15 minutes. Check `system.view_refreshes` (`status`, `next_refresh_time`)
before stopping.

**After a killed shutdown, repair ClickHouse.** The indexer only requeues segments
whose inserts *failed* (`/archive/segments/.clickhouse_reindex_queue`); a segment
in flight at SIGKILL, and any sealed-but-not-yet-indexed segment, stays partially
or entirely missing from `events_local` after restart. Find them from the journal
(`Indexing segment N` without a matching `Indexed segment N`, plus the final
sealed segment). Insert only the missing IDs: re-indexing a whole segment
double-counts the summing views (reactions, replies, reposts, daily user stats).
The 2026-10-12 repair converted each segment with `notepack-to-parquet`, selected
the IDs absent from `events_local` through a `clickhouse-client --external` table
into a Native file, checked its row count, inserted it, and re-counted to zero
missing.

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
(also sets `PENSIEVE_BIND_ADDR`, `PENSIEVE_POSTGRES_API_FAMILIES=none` and
`PENSIEVE_LATEST_EVENT_WATERMARK_PATH` inline; builds from 2026-10-12 on ignore
`PENSIEVE_POSTGRES_API_FAMILIES`), `pensieve-preview.service.d/10-release.conf`,
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
  unmerged `codex/negentropy-hardening` branch; previous), `api-8bdac94` (master, current) and
  only `preview-3ab1045`.
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

## Postgres analytics leftovers

The Postgres analytics serving store was abandoned on 2026-10-12 and its code,
units, scripts and Compose service were removed from `master`. Production
never served from it (`PENSIEVE_POSTGRES_API_FAMILIES=none`), the container was
not running, and `pensieve-analytics-refresh.timer` was disabled. What remains
on the host, to clean up with the maintainers (do not run these from an agent
session):

- **Compose and `POSTGRES_ANALYTICS_PASSWORD`.** Until the `~/pensieve` checkout is
  fast-forwarded past this change, its `compose.yml` still defines `postgres-analytics` with
  `POSTGRES_ANALYTICS_PASSWORD:?`. Remove that variable from
  `/etc/pensieve/pensieve.env` only **after** the checkout's `compose.yml` no
  longer requires it; otherwise every `docker compose` command, including
  `pensieve.service`, fails.
- **Refresh units.** `/etc/systemd/system/pensieve-analytics-refresh.{service,timer}`
  (disabled) and `/etc/pensieve/analytics-refresh.env`; remove the units and
  `daemon-reload`.
- **API drop-in.** `pensieve-api.service.d/90-postgres-cutover.conf` sets
  `PENSIEVE_POSTGRES_API_FAMILIES=none`, which builds from 2026-10-12 on ignore.
  Drop it at the next API deploy; keep `PENSIEVE_LATEST_EVENT_WATERMARK_PATH`
  (it serves `latest_event` without a ClickHouse scan). See
  [`systemd/dropins/README.md`](systemd/dropins/README.md).
- **Data.** `/archive/analytics` (3.1 TB on the archive disk on 2026-10-12): two local
  snapshots of the Parquet lake (`full-input`, 13 Aug, hard-linked into `lake`, 27 Aug;
  ~1.17 TB together), staged `deltas`, the intermediate build outputs for each Postgres
  product, and the Slice A DuckDB databases and backups. All of it is derived: the lake
  itself lives in object storage. The maintainers decided on 2026-10-12 to delete all of it
  (researchers will query the current lake, not a stale snapshot) after checking every
  staged Parquet file against object storage. Also `/var/lib/pensieve-analytics`
  (refresh state). `/data/postgres-analytics` never existed on the host.
- **`DATABASE_URL` / `POSTGRES_ANALYTICS_PASSWORD` in `/etc/pensieve/analytics.env`**
  are no longer read by any binary.

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
not covered by the routine deploy above. Since 2026-10-12 08:29 CEST the ingester runs
build `negentropy-ba0c4a1` (PR #72) from `zzzz-bounded-canary.conf` with the rolling Premium Primal ledger;
the worker is started only for monitored pilots. Expect that drop-in to change. See [`docs/negentropy_runtime.md`](../docs/negentropy_runtime.md)
and [`docs/negentropy_continuous_readiness.md`](../docs/negentropy_continuous_readiness.md).
Its Kuma probe is `pensieve-negentropy-kuma.timer` (`/health/negentropy`); the
archive-admission probe is `pensieve-archive-admission-kuma.timer` (`/health/archive-admission`).
Both were active on 2026-10-11. Their scripts are `ops/scripts/negentropy-kuma.py`
and `ops/scripts/archive-admission-kuma.py`, installed to `/usr/local/libexec/pensieve-*-kuma.py`.

## Cleanup debt
- `ops/scripts/sync-archive.sh` still uploads + prunes `*.notepack` segments; reconcile it
  with Parquet retention now that the live shadow and historical campaign exist (see
  [`docs/migration-plan.md`](../docs/migration-plan.md)).
- `/api/v1/stats/longform` and `/api/v1/stats/publishers` time out (20 s cap) on ClickHouse:
  longform scans `events_local` for kind 30023 (fast only with the dropped `events_by_kind`
  projection) and publishers scans 30 days by `created_at` (fast only with the dropped
  `events_by_time` projection). The fix is small insert-fed ClickHouse tables (prepared
  migration 019, in progress), not Postgres.
- The API reads `RELAY_DB_PATH` through one long-lived `immutable=1` SQLite connection while the
  ingester keeps writing the WAL database, so relay endpoints can be stale.
- `ops/systemd/*.service` `ExecStart` lines do not match production; the drop-ins in
  [`ops/systemd/dropins/`](systemd/dropins/README.md) are the record.
