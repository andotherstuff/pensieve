# Pensieve Ops Runbook

How the production box is actually operated. The box runs at `/home/pensieve/pensieve`
(the repo checkout); default branch `master`.

## Layout

- `ops/production/compose.yml` — Docker stack: ClickHouse, Postgres analytics,
  Prometheus, Grafana, Caddy.
- `ops/production/{caddy,clickhouse,prometheus}/` — config mounted into those containers.
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

## Grafana

Provisioning is no longer in the repo. Configure the Prometheus datasource
(`http://prometheus:9090`) and dashboards directly on the running instance at
`https://<DOMAIN>/grafana/`. Plugins (ClickHouse, SQLite datasources) are still
installed at container start. See the ingestion/coverage dashboard guide handed off
separately for the panel set.

## Cleanup debt
- `ops/scripts/sync-archive.sh` still uploads + prunes `*.notepack` segments; this needs
  updating when the Parquet archive lands (see [`docs/migration-plan.md`](../docs/migration-plan.md)).
