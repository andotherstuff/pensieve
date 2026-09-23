# Isolated negentropy: approved execution plan

Approved 2026-09-23. This is the tracked implementation checklist for the process
isolation proposal. It does not authorize a production deployment or change the
analytics architecture. The existing SDK remains pinned; no SDK fork is required.

## Ownership and product decisions

- One separate worker process handles one relay/time-window attempt. The ingester
  alone owns archives, dedupe, inventory and the durable SQLite job ledger. The
  worker receives bounded inventory and uploads candidates over authenticated Unix
  IPC; it never opens production databases, archives or secrets.
- Start with the existing 14-day horizon. Retain unfinished gaps until recovered,
  even after they age out. Initially use an explicit small relay allowlist, not
  automatic catalog expansion. Historical exploration remains a separate future
  feature (issue #39), not a prerequisite.
- Record relay/window attempts, outcomes and observed counts. Distinguish received
  frames, pending duplicates, archived duplicates and durable novelty. Neither
  protocol success nor zero returned events proves complete global history.
- A received obligation is durable before acknowledgement. Completion requires
  valid ProtocolDone and archive proof for every received ID across all attempts.
  Failure, timeout, worker exit, pending dedupe and notifications never imply success.
- Uptime Kuma is the chosen alert destination. Health stays failed until the actual
  recovery condition resolves. Verify delivery during rollout; an elaborate universal
  recovery procedure is not required.

## Reviewable slices and current status

| Slice | Scope | Gate / status |
| --- | --- | --- |
| 1a, #45 | Durable jobs, leases, retry and split | Merged; CI and review passed |
| 1b, #46 | Bounded framed uploads, credits, received receipts | Review/CI gate; library only |
| 2a, #48 | Shared archive admission, durable receipt reconciliation and compaction | Review/CI gate; no runtime consumer yet |
| 2b | Independent periodic archive sealing | Current implementation; Parquet-disabled durability and joined shutdown tests |
| 2c | Bounded sealed-segment inventory replay/export | Next; explicit rollout floor, durable cursor, gap detection and archived-only advertisement |
| 3 | Worker executable, authenticated IPC and parent scheduler | Not implemented; fake relay flood/hang/partial-fetch/cancellation tests |
| 4 | Linux isolation, metrics and operations | Not implemented; synthetic worker OOM/hang/kill and real alert-delivery gate |
| 5 | Controlled production canary and soak | Not started; separate readiness decision |

An earlier in-process hardening draft (#42) is not a prerequisite or a second
runtime to enable alongside this worker. Library merges are not deployment claims.
Run focused tests and mandatory `just precommit` immediately before every commit;
require exact-head CI and independent reviews before merging behavior changes.

## Remaining implementation contracts

Inventory replay must stream sealed segments, persist a monotonic cursor only
after inventory is durable, replay safely after crashes, and stop on missing
segments. Notifications only wake it. Preserve verified old inventory; choose an
explicit rollout floor rather than silently rebuilding all history. Verify legacy
entries against durable archive markers before advertising them. Export complete
capped intervals; never truncate and call them complete. Do not prune inventory or
source segments required by unresolved jobs/replay.

Keep database work off async reactors, using bounded queues/executors. Begin with
one worker, at most two awaiting-durability jobs, 15-minute inclusive windows and
fair scheduling between fresh work and old gaps. Split `[a,b]` into `[a,m]` and
`[m+1,b]` for volume limits, not ordinary connectivity errors. A one-second window
that exceeds limits blocks and alerts. Persist backoff (initial proposal 1 minute
to 1 hour); unknown worker loss gets a same-window retry before volume classification.

Initial engineering limits, to measure rather than assume adequate: 250,000 local
IDs, 1 MiB frames, 16 events / 8 MiB credit, 50,000 frames / 64 MiB per attempt,
1 GiB ledger including WAL. Pause admissions on budget/archive faults while allowing
bounded receipt recovery. Never delete unfinished work to make quotas pass. Attempt
summaries also need a measured retention policy before indefinite operation.

Systemd, not the ingester, owns the separate worker's process lifetime and sibling
cgroup: proposed 1 CPU, MemoryHigh 1 GiB, MemoryMax 2 GiB, no swap/core dumps,
64 tasks, 10-minute runtime and 5-second kill grace. Application deadlines are
9 minutes overall and 2 minutes without useful progress. Use paced restarts, a
dedicated unprivileged UID, peer-UID authentication and restricted socket access;
no public listener. Verify effective limits and sandbox access on Linux.

## Production gates and stop conditions

1. Verify current ingestion health, disk reserves, exact binaries and configuration
   backups. Require at least 20 GiB `/data` reserve and the stricter archive policy.
2. Run synthetic worker-only OOM/hang/kill tests. Ingestion PID/restarts must remain
   unchanged; no orphan worker; unfinished obligations survive. Test archive and
   ledger failures separately and verify Kuma receives alerts.
3. Confirm the exact canary relay allowlist and measured throughput/freshness targets
   with the operator. These are not yet selected by this document.
4. One deliberate graceful ingester restart enables IPC and disables the old loop.
   At least three durable canary jobs plus injected worker recovery must pass.
5. Soak for 24 hours, measuring backlog age, throughput, memory and live admission.
   Expand only if service capacity exceeds incoming work. Rollback disables worker
   and new leases but preserves live ingestion, ledger and unresolved gaps.

Stop for archive uncertainty, failed tests/reviews, missing rollout decisions or
inadequate capacity. Do not restart production ingestion during development, deploy
automatically on merge, purge evidence, or count local macOS tests as Linux isolation
proof. No all-history analytics scan is part of this work.
