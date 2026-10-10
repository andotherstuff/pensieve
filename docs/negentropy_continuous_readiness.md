# Continuous reconciliation readiness

This is the follow-up to PR64, merged as `4273ba6`. Continuous operation is
**not enabled** by this document or the status tool. Preserve all prior ledgers.

## Verified bounded trial (2026-10-10)

The Premium Primal-only trial completed three durable windows and recovered from
an injected worker loss. The 60 archived frames are not 60 proven novel events.
Evidence, exact binaries, journals and the subsequent segment-29854 Parquet
verification are retained on pensieve under
`/var/lib/pensieve-canary/20261010T162833Z/`.

The shorter trial replaces the previously proposed 24-hour initial canary, not
proof of indefinite unattended operation or of multi-relay capacity.

## First increment: measure without deleting

Run `sudo -u pensieve python3 ops/scripts/negentropy-ledger-status.py /path/to/jobs.sqlite`
against schema 8. The caller must own the database and existing WAL sidecars;
running as root against a pensieve-owned ledger is refused before SQLite opens.
It opens SQL read-only, limits SQL work to five seconds, reports aggregate
counts and separately sampled DB/WAL/SHM sizes, and refuses unknown identities.
It never emits lease tokens, event IDs, policy recipient lists or failure bodies.
SQL read-only does not mean filesystem-write-free: SQLite can touch shared-memory
reader coordination and recreate sidecars during concurrent writer shutdown.
The ownership check prevents creation under another UID. A WAL database missing
either sidecar is refused with `wal_sidecars_missing`; do not fabricate sidecars,
change ownership or use `immutable=1` to bypass this on a live database. Closed
WAL backups are deliberately unsupported by this tool.

`lease_grants` is cumulative across all jobs and attempts, not live worker count;
`states.leased` counts current leases. `attempt_summaries` may be smaller when a
worker died before recording a receipt or protocol summary. The oldest incomplete
event time is not queue latency. Pinning counts include jobs in all states.
File sizes are raw non-atomic observations, **not admission headroom**: Rust also
accounts for allocated blocks, logical size and a write reserve. Default budget
constants are intentionally not copied into the report. A successful report
means measurements are available, not that retention or rollout is safe.

Domain refusals emit stable error codes (`wrong_ledger_owner`,
`wal_sidecars_missing`, `unsupported_identity`, `receipt_counter_mismatch`,
`exclusion_counter_mismatch`); unexpected errors never echo database text.

Test with `python3 -m unittest discover -s ops/tests -p 'test_negentropy_ledger_status.py'`.

## Retention implementation contract (next increment)

Do not simply delete `state='complete'` rows. Schema 8 has three hazards:

1. `policy_exclusions.first_job/first_attempt` references attempt rows, including
   completed jobs. That evidence may satisfy receipts on other unresolved jobs.
2. Parent/child jobs jointly prove split-window completion; deleting one child
   destroys lineage and can prevent safe parent reconciliation.
3. Jobs lack completion timestamps; `since` is event time, not queue age or last
   successful reconciliation. The integer primary key is not AUTOINCREMENT, so
   naive deletion can also permit job-ID reuse.

Implement an explicit versioned migration, with backup and rollback tests, before
enabling retention. Give jobs a monotonic non-reused identity and a completion
timestamp. Maintain the original replay floor (29808 on this deployment), policy
fingerprint, planner cursor, fair selector state and every unresolved obligation.

Compact only fully completed, closed rolling sweeps whose whole split trees are
complete, have no remaining receipts, and have no exclusion-proof references.
Skip pinned trees; never evict their proof merely to free job slots. Preserve
compact per-relay/day coverage and received/archived/excluded/attempt/failure
accounting separately; call these frame counts, not unique new-event counts.
Record the exact intervals and schema/policy identity so a summary cannot imply
broader coverage. Define a bounded lifetime for detailed successful histories and
bounded summary storage; merely moving all rows to an ever-growing table is not
retention. Initial detailed retention duration and summary limits must be reviewed
alongside measured growth, not invented from this three-job trial.

Each compaction transaction must have explicit row/byte bounds, commit the summary
and deletion together, and preserve counters and foreign keys. No automatic
VACUUM of a live ledger; freed pages can be reused, and WAL checkpoint behavior
must be measured. Keep receipt recovery available under admission backpressure.

Required tests: incomplete/blocked/retry/leased trees unchanged; pinned exclusion
proof retained; split lineage; no job-ID reuse; idempotent compaction; aggregate
reconciliation; crash/transaction rollback; schema migration/reopen; full budget
behavior; fair leasing after cleanup; bounded query plans and WAL growth.

## Monitoring and activation gates

- Add durable completion time and last-success/failure observations. Alert on
  stalled work, persistent missing-ID failures and ledger/admission backpressure.
  Archive-admission Kuma is already tested, but its HTTP 200 does **not** prove
  reconciliation is advancing. Test the reconciliation alert separately.
- Measure a single-relay rolling run against the approved 14-day horizon before
  expanding the relay set. There are 1,344 root windows per full sweep per relay;
  the planner can revisit the horizon after a changed boundary. Three small
  windows do not establish the sustained service rate or time to the job cap.
- Fresh non-canary production ledger; never reuse or delete failed canary ledgers.
  Keep the existing inventory and its replay identity. Back up config and verify
  Parquet catch-up before the necessary controlled ingester restart.
- Premium Primal only initially, no catalog expansion. Preserve worker resource
  limits, recipient block and archive reserves. A finite monitored pilot is not
  permission for indefinite unattended operation before retention is implemented.
- Enable the static worker only after reviewed code, green exact-head CI, verified
  binaries, alert delivery and the above gates. Stopping the worker must leave
  live ingestion and all unresolved gaps intact.
