# Production systemd drop-ins

Since 2026-11-23 production has **no** drop-ins for Pensieve units. Each unit is
installed verbatim from `ops/systemd/`, and the ingester binary comes from the release
that `~/pensieve/releases/current` points at (see
[`ops/RUNBOOK.md` → Releases](../../RUNBOOK.md#releases)). Change a unit in the repo and
install it with the next release instead of adding a drop-in.

Until then, drop-ins under `/etc/systemd/system/<unit>.d/` reset `ExecStart=` and pointed
each service at a release directory. Their last state (the ingester's seven, the API's
and the preview's two each) is in this directory's git history, and a copy taken on the
host before removal is in `~pensieve/maintenance-backups/units-20261123/`.

Check that nothing overrides a unit:

```bash
systemctl show pensieve-ingest -p DropInPaths -p ExecStart
readlink /proc/$(systemctl show -p MainPID --value pensieve-ingest)/exe
```
