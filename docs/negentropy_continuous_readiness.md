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
against schema 8 or 9. The caller must own the database and existing WAL sidecars;
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
Schema 9 reports retained coverage accounting separately from detailed attempts;
neither is an all-time unique-event count. Its last-success timestamp is a durable
observation, not proof the runtime is currently healthy.
File sizes are raw non-atomic observations, **not admission headroom**: Rust also
accounts for allocated blocks, logical size and a write reserve. Default budget
constants are intentionally not copied into the report. A successful report
means measurements are available, not that retention or rollout is safe.

Domain refusals emit stable error codes (`wrong_ledger_owner`,
`wal_sidecars_missing`, `unsupported_identity`, `receipt_counter_mismatch`,
`exclusion_counter_mismatch`); unexpected errors never echo database text.

Test with `python3 -m unittest discover -s ops/tests -p 'test_negentropy_ledger_status.py'`.

## Retention implementation and release contract

Schema 9 adds completion/creation observations, a durable job-ID high-water mark,
and bounded completed-window summaries. Opening schema 7/8 performs the metadata
migration in one transaction; unsupported versions still fail closed. Before
upgrading an existing ledger, stop its sole owner, preserve a consistent SQLite
backup (including WAL state), and retain the old binary. A pre-upgrade backup is
only a rollback option before new work is admitted; never restore an old ledger
over newly accepted receipts. The activation below uses a fresh production ledger
and does not upgrade, reuse, or delete canary ledgers.

Legacy completed rows have unknown completion times/policy identity and remain
ineligible for automatic cleanup. New successful detail is kept at least 24 hours.
Each maintenance turn considers at most one root and 2,048 tree nodes, 4,096
detail rows and 8 MiB of estimated detail. Whole trees pinned by proof, oversized
trees, current planner sweeps, and sweeps with unresolved jobs are skipped.
Compaction uses exact 15-minute coverage bits per relay/day/policy, together with
frame/attempt/failure accounting. Repeated sweeps do not imply novel events.
Summaries expire 90 days after their last compaction and are capped at 16,384 rows;
a full summary table preserves detail rather than dropping obligations.

Do not simply delete `state='complete'` rows. Schema 8 has three hazards:

1. `policy_exclusions.first_job/first_attempt` references attempt rows, including
   completed jobs. That evidence may satisfy receipts on other unresolved jobs.
2. Parent/child jobs jointly prove split-window completion; deleting one child
   destroys lineage and can prevent safe parent reconciliation.
3. Jobs lack completion timestamps; `since` is event time, not queue age or last
   successful reconciliation. The integer primary key is not AUTOINCREMENT, so
   naive deletion can also permit job-ID reuse.

Validate the versioned migration with backup and rollback tests before enabling
retention. Maintain the original replay floor (29808 on this deployment), policy
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

`/health/negentropy` is separate from `/health/archive-admission`. It returns fixed,
non-sensitive classifications: disabled, starting, idle, recent progress, stale
owner, stalled durable progress, unresolved failures, capacity exhaustion, or
runtime failure. Idle is only healthy with a fresh owner observation. Owner
observations expire after 15 minutes; pending work without durable completion for
one hour fails. Blocked jobs or still-unresolved jobs with three attempts fail
even if another job completes. Persisted creation/completion times survive
restart. An owner failure remains latched for that runtime; new process startup
must still satisfy the durable ledger checks rather than refreshing a grace clock.

Install `ops/scripts/negentropy-kuma.py` as
`/usr/local/libexec/pensieve-negentropy-kuma.py` and the matching service/timer only
after the release gates. Store the new push URL, mode 0600 root-owned, in
`/etc/pensieve/negentropy-kuma.url`; systemd passes it with `LoadCredential`.
Create a **separate** Kuma push monitor with a 180-second missing-heartbeat deadline
and Svent Bot notifications. The timer samples every 30 seconds. Test 503→200
delivery with the explicit `--fixture` endpoint on localhost:19091; do not change
live archive admission to synthesize an alert. Never put the push URL in argv,
logs, Git, or evidence. When intentionally disabling reconciliation, pause its
monitor; the explicit disabled response is not HTTP success. Keep Archive Admission
monitor 45 and API monitor 10 unchanged.

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
