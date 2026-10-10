# Retention and progress alerts implementation ledger

Base: merged master `a43de7f9e59e60772f94013ceb313de9431e5f63`.
Production is not changed by this document. The worker remains stopped until
the implementation, review, CI, and operational gates below pass.

## Scope

Only reconciliation bookkeeping is eligible for retention. Canonical segments,
Parquet, ClickHouse, sync inventory, unfinished gaps, and prior canary evidence
are outside cleanup scope. Preserve replay floor 29808 and the live recipient
exclusion policy. Start with Premium Primal only and the existing 14-day horizon.

## Implementation sequence

1. Add versioned ledger lifecycle metadata: non-reused job IDs, durable completion
   times, and restart-stable progress observations. Existing completion dates
   must remain unknown rather than being invented during migration.
2. Compact only closed, wholly complete rolling work, retaining split lineage and
   skipping every tree pinned by receipts or exclusion-proof references. Summary
   insertion and detail deletion must be a single bounded transaction. Bound
   summary storage as well as successful detail; never discard unfinished work
   to make capacity. Do not run automatic VACUUM on the live ledger.
3. Expose separate reconciliation health. Archive admission is not reconciliation
   progress. Pending work without durable completion for one hour, stale runtime
   observations, persistent failures, and capacity exhaustion must be visible.
   Restart alone must not clear an unresolved failure. Disabled/idle operation
   needs explicit handling rather than a misleading success signal.
4. Wire a separate Uptime Kuma monitor to Svent Bot and verify failure/recovery
   delivery without disturbing the existing archive-admission monitor.

## Verification and release gates

- Baseline verification before edits; targeted migration, rollback, foreign-key,
  split-tree, pinned-proof, ID-reuse, bounded-query, and progress-health tests.
- Mandatory final `just precommit`; exact-head Claude and Grok self-review;
  resolve actionable findings, and require green CI before merge.
- Read the merged runtime documentation and operations runbook before deployment.
  Preserve root-only configuration/binary backups and exact artifact hashes.
- Recheck disks, archive admission, sealing, Parquet publication, recipient filter,
  and restart accounting. Never create another archive/dedupe owner.
- Fresh non-canary production ledger; preserve all prior ledgers and inventory.
  Run a directly monitored 20–30-minute Premium Primal activation, including
  durable progress and alert checks. Stop on production errors or required operator
  decisions; routine development errors are fixed before proceeding.
- Measure actual sweep throughput and revisit cadence; the planner can revisit
  the full 1,344-window horizon, not merely the newest 15-minute window.

## Retention and health policy

- Keep successful detail for at least 24 hours. Unknown legacy completion times
  are not eligible for automatic cleanup.
- Summaries use one 96-bit completed-window bitmap per relay/day/policy identity;
  gaps are not represented as covered. Repeated observations add frame accounting,
  not unique-event counts. Expire observations after 90 days and cap summary rows
  at 16,384. Reaching a cap retains detail and reports pressure rather than
  discarding an unfinished obligation.
- One retention turn considers one root, at most 2,048 tree nodes, 4,096 detail
  rows, and 8 MiB of estimated retained detail. Oversized/pinned trees stay intact.
- A separate `/health/negentropy` classification fails on an owner observation
  older than 15 minutes, one hour without durable progress while work is pending,
  persistent unresolved failures, or exhausted capacity. HTTP success is not an
  assertion that the full rolling horizon has been reconciled.
- Pause the separate monitor when intentionally disabling reconciliation; an
  explicit disabled response must not masquerade as successful reconciliation.

These are implementation defaults under test, not measured production capacity
claims. The short activation must report retained-job growth and sweep throughput.

## Initial preflight

Read-only server check: ingester active, NRestarts 0; worker inactive;
archive-admission HTTP 200; approximately 259 GiB free on `/data`, 1.4 TiB on
`/archive`, and 116 GiB available memory. These are preflight observations, not
deployment-time guarantees.
