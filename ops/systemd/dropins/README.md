# Production systemd drop-ins (record)

Production does not run the `ExecStart` lines in `ops/systemd/*.service`.
Drop-ins under `/etc/systemd/system/<unit>.d/` reset `ExecStart=` and point each
service at a release build (see [`ops/RUNBOOK.md` → Routine deploy](../../RUNBOOK.md#routine-deploy)).
This directory records those drop-ins so the repo shows how services actually
start. Paths mirror `/etc/systemd/system/`.

Captured from the production host on 2026-10-11; ingester drop-ins as of 2026-10-12 08:40 CEST. These are **records, not
install sources**: they name specific release directories and canary files
that change on every deploy. When you change a drop-in on the host, update the
copy here in the same PR as the code it deploys.

| Unit | Drop-in | Effect |
|------|---------|--------|
| `pensieve-api` | `90-postgres-cutover.conf` | Runs `releases/api-fe54f04/pensieve-serve` (master: 019 rollups, 60-day publishers cap, 020 `kind_totals`, 021 `kind_activity`; since 2026-10-13 07:36 UTC; previously `api-d026d53`, `api-b466b37`, `api-ff8a446`, `api-60c4bfe`, `api-f60f003`, `api-8bdac94`, `api-7ac5121`), plus `RELAY_DB_PATH=/data/relay-stats.db` and the latest-event watermark path |
| `pensieve-preview` | `10-release.conf` | Runs `releases/preview-3ab1045/pensieve-preview` |
| `pensieve-ingest` | `giftwrap-recipient-block.conf` | Sets `PENSIEVE_BLOCKED_GIFTWRAP_RECIPIENTS` (see [`docs/giftwrap_recipient_hotfix.md`](../../../docs/giftwrap_recipient_hotfix.md)) |
| `pensieve-ingest` | `nip66-monitor.conf` | Older `ExecStart` adding `--nip66-monitor-relays`; superseded by `zzzz-bounded-canary.conf` |
| `pensieve-ingest` | `negentropy-worker.conf` | `Wants=pensieve-negentropy-worker.service`: starts the isolated NIP-77 worker whenever the ingester starts, including at boot (continuous Premium Primal reconciliation since 2026-10-12) |
| `pensieve-ingest` | `override.conf` | `RUST_BACKTRACE=1` |
| `pensieve-ingest` | `stop-timeout.conf` | `TimeoutStopSec=15min` (added 2026-10-12): a ClickHouse insert slowed by the hourly refreshable views must finish instead of being killed mid-segment |
| `pensieve-ingest` | `zzzz-bounded-canary.conf` | Runs `releases/ingest-2e07c31/pensieve-ingest` (master incl. #88, #90, #92, #94 and #95, since 2026-10-13 16:35 UTC, with `--isolated-negentropy-pause-on-failure-secs 7200`; previously `releases/ingest-85a2402/` (#94, same flags; the rollback target), `releases/ingest-26f2170/` (#92, same flags; the rollback target), `releases/ingest-e73bebb/` (#88 and #90, schema 10, no pause flag), before that `releases/ingest-b466b37/` (#72 and #73), which cannot open the schema-10 ledger, and before that the PR #72 build `releases/ingest-ba0c4a1/` with its worker) with `--isolated-negentropy` and the rolling Premium Primal ledger, adds `/run/pensieve` to `ReadWritePaths`, and sets `Restart=no` (the ingester does not restart itself after exiting) |

### Obsolete settings in the API drop-in

The Postgres analytics backend was removed from `pensieve-serve` on 2026-10-12, and the
API drop-in no longer sets `PENSIEVE_POSTGRES_API_FAMILIES` (since `api-f60f003`).

The comment inside `10-release.conf` ("Rollback: remove this file") predates
migration 018. Removing it now would start the 2026-07-27 build in
`~/pensieve/target/release/`, which has no compact lookups and would scan
`events_local` for every profile, zap and naddr preview. Roll back by pointing
`ExecStart` at another `releases/` directory instead.

Drop-ins apply in lexical order and each `ExecStart=` line clears the previous
value, so the last file that sets `ExecStart` wins (`zzzz-…` for the ingester).
Check what is really running with:

```bash
systemctl show pensieve-ingest -p DropInPaths -p ExecStart
readlink /proc/$(systemctl show -p MainPID --value pensieve-ingest)/exe
```
