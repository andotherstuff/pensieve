# Pensieve Ops Runbook

How the production box is actually operated. The box runs at `/home/pensieve/pensieve`
(the repo checkout); default branch `master`.

## Layout

- `ops/production/compose.yml` — Docker stack: ClickHouse, Prometheus, Grafana, Caddy.
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
| ClickHouse, Prometheus, Grafana, Caddy | Docker Compose via `pensieve.service` |
| `pensieve-ingest`, `pensieve-serve`, `pensieve-preview` | native binaries via systemd |
| archive sync (hourly) | `archive-sync.timer` → `archive-sync.service` |

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
