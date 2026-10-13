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

## Rolling Premium Primal pilot (2026-10-12)

The first rolling pilot (2026-10-11 20:07 CEST, build `20f0b4b`) admitted no
work: the worker waited 82 s through archive replay and was rejected with
`worker service lifetime insufficient`, because admission reserved a full
nine-minute session inside the ten-minute service lifetime. PR #72 caps each
session by the remaining lifetime instead.

The repeat ran 08:47:18–09:07:18 CEST on 2026-10-12 with the PR #72 ingester and
worker (build `ba0c4a1`, same code as `66a3459`), the preserved ledger
`/data/negentropy/production-premium-20261011T180545Z/jobs.sqlite` (32 queued,
0 attempts beforehand), replay floor 29808, and a `systemd-run` timer that
stopped the worker after 20 minutes:

- 70 worker processes and lease grants; 69 jobs durably complete.
- 709 frames received and 709 archived; 0 failures, 0 policy exclusions,
  0 unresolved receipts after the final seal.
- Every session was admitted: 69 with `session_seconds=540`, 1 with 519.
- The job in flight at the timed stop logged `Broken pipe` and was retained as
  `retry_wait`; nothing was lost.
- `/health/negentropy` reported `progress_within_deadline` throughout and archive
  admission stayed `200`. The worker was left stopped afterwards, so progress
  health drifts back to stalled until the next run.

## Continuous Premium Primal operation (since 2026-10-12)

After the pilot, the maintainer turned on continuous reconciliation against
`wss://premium.primal.net` only (the same allowlist, ledger and replay floor), to
observe it for a while before adding relays. The worker was started at 15:22:59 UTC
with no stop timer, and `pensieve-ingest.service.d/negentropy-worker.conf`
(`Wants=pensieve-negentropy-worker.service`) starts it whenever the ingester starts,
including at boot. In the first 20 minutes: 148 jobs complete (from 69), 2,010 frames
received and 2,009 archived, 0 failures, 0 blocked jobs, health
`progress_within_deadline`, ledger ~184 KiB + ~105 KiB WAL.

The fault policy above still applies: on a blocked job, persistent failure, capacity
fault or stale runtime, stop only the worker, keep the ledger, and leave Kuma monitor 46
red until an operator decides. To turn reconciliation off deliberately, remove the
`negentropy-worker.conf` drop-in, `daemon-reload`, stop the worker, and pause monitor 46.
Expanding beyond Premium Primal is a separate decision.

## Incident 2026-10-13: runtime stopped on a lease race

At 2026-10-13T06:01:22Z the isolated runtime stopped with `isolated reconciliation
paused; durable gaps retained error=invalid parent session request`, 15 ms after
the waiting worker logged `worker IPC failed`. `/health/negentropy` latched
`runtime_failed`; live ingestion was unaffected. Each later worker start failed the
same way because the stopped runtime had removed its socket.

**Cause.** Leasing is a two-step owner command: `fair_candidate(now1)` picks the
fair job, the parent scans that job's archived inventory (up to 250,000 dedupe
lookups, which can take minutes under live load), then it leased with
`lease_next_fair(now2)`, which re-ran fair selection. Job 768 (Premium Primal, a
`retry_wait` gap on its 15th relay failure, due at 05:59:06) was not yet due at
`now1`, so the gap-preferring rotation fell back to a fresh root. When the scan
finished at 06:01:22 the gap was due, so re-selection leased job 768 instead,
committed that lease (attempt 16, 600 s TTL, rotation advanced), and the mismatch
check returned `InvalidRequest`. The runtime treats any error from leasing as fatal.
No receipt, archive marker or job ID was lost; the only effects were the stopped
runtime and one consumed attempt number with no session.

**Fix.** The parent now leases exactly the candidate whose inventory it checked
(`lease_fair_candidate`; see [fair leases](negentropy_fair_leases.md)). A candidate
that changed meanwhile is reselected later instead of being an error. Releasing an
attempt whose lease already lapsed now affects only that attempt, and every
authenticated attempt failure is logged with its kind and outstanding-ID count
(see [runtime fault scope](negentropy_runtime.md#fault-scope)). Ledger storage
errors, budget exhaustion, invalid ledger input and archive recovery still stop the
runtime.

**Persistently undeliverable IDs.** Job 768's relay advertises one event ID in its
negentropy set that it never delivers: all 15 attempts recorded identical
`unavailable` reports (`missing_count` 1, ID `089d1fdc…6b65`) after receiving and
archiving the other events (20 on attempt 1, 17 on each later one). No relay
returns that event by ID (Premium Primal, relay.primal.net, Damus, nos.lol,
nostr.wine, Snort, offchain.pub, nostr.mom) and it is not in ClickHouse. The policy
at the time forbade skipping it, so the gap was retried hourly and health stayed
`unresolved_failures` from 2026-10-12T19:15Z. The maintainer chose option 3 of
the three considered (keep retrying; block after N identical reports; record a
relay-unavailable exclusion with proof). It is now implemented; see
[relay-unavailable exclusions](#relay-unavailable-exclusions) below.

### Unattended operation and monitor 46

Monitor 46 went red at 19:15Z and nobody acted for eleven hours, as expected
overnight. Two mechanisms keep continuous mode from depending on that:

- **In-process pause (recommended).** Start the ingester with
  `--isolated-negentropy-pause-on-failure-secs <SECS>`. Once one job behind
  `unresolved_failures` has failed continuously for that long (exempt jobs aside,
  below) the runtime stops granting leases, keeps replay, planning,
  receipt maintenance and health, and logs `isolated reconciliation paused leasing`
  (`negentropy_isolated_failure_pause=1`). This is exactly the documented fault
  policy (stop new attempts, keep every gap, leave the monitor red) without root
  automation or a second process. A grace of 7200 s lets a transient relay outage
  recover through four more paced retries (backoff 4, 8, 16, 32, then 60 minutes)
  before pausing. Trade-off: one stubborn job stops all
  reconciliation, including healthy fresh windows, until an operator restarts the
  ingester without the flag or resolves the job; the pause does not end on its own
  because nothing is retried while paused. Restarting the ingester also restarts
  live ingestion. The grace clock is per job, and a window whose relay
  persistently withholds the same advertised IDs is exempt while its reports are
  on track to a [relay-unavailable exclusion](#relay-unavailable-exclusions), so
  it is completed instead of pausing (see the interaction below).
- **Runbook step.** On monitor 46 red with `unresolved_failures`, stop only the
  worker (`sudo systemctl stop pensieve-negentropy-worker`) and record the
  read-only ledger status. Without the flag, this is what must happen manually; the
  worker is started again by the ingester's `Wants=` at the next ingester start.

Not chosen: a root timer that stops the worker on a red probe adds a privileged
moving part that can misread a transient reason, while the in-process pause uses the
ledger's own durable classification. Leaving the runtime retrying (no flag) is safe
for data after this fix; it only means the red monitor hides later faults.

## Relay-unavailable exclusions

This is the one case in which a window completes without an event that its relay
advertised. It replaces, for this case only, the earlier rule that no automatic
expiry exception or skipped-ID success is authorized. Every other `Unavailable`
outcome still retains the gap and retries it.

**Rule.** On every runtime tick, before leasing, the parent checks each retry-wait
job whose latest `K` consecutive reported attempts (default 5) each ended in an
`unavailable` report naming the *identical* missing-ID set, and excludes those IDs
when all of the following hold:

- the set is fully enumerated: `missing_count` equals the number of sampled IDs,
  and the set has at most `--isolated-negentropy-unavailable-exclusion-max-ids`
  IDs (default 8, at most the 128-ID sample cap);
- every missing ID was explicitly requested: each attempt's received frames plus
  its missing count fit in the worker's single 128-ID fetch batch (a larger diff
  can report later-batch IDs that were never asked for). Exactly 128 qualifies. A
  relay that caps one REQ's results below the batch answers EOSE early; the worker
  therefore re-requests exactly the still-missing IDs (at least once, and again
  while each request delivers something) and reports only IDs that a REQ naming
  exactly them did not return. Workers older than this change lack that
  follow-up;
- the reports span at least `T` seconds from first to last (default 3600);
- every frame the job received is already archive-confirmed (no retained receipt).

IDs of the set that already carry a durable archive marker count as delivered:
the archive holds them, whichever path brought them in. This matters for events
archived before the inventory replay floor. The inventory never lists them, so
every attempt's diff names them again, and normal reconciliation could never
finish such a window (the previous rule left it retrying forever). Only the
remaining IDs are excluded; if none remains the job completes with reason
`already_archived` and no exclusion row (INFO `negentropy window completed: every
ID the relay withheld was already archived`,
`negentropy_relay_unavailable_already_archived_total`).

Any other failure kind (relay errors, deadlines and worker-side protocol failures
all report as `relay`), a different set or a protocol-complete attempt ends the
run. An attempt that ended with no report at all carries no relay evidence and is
skipped without counting: a lost worker, an ingester restart mid-session, or a
lease consumed without a session, like job 768's attempt 16 in the lease race
above. Skipping it is safe because any event such an attempt did receive is either
still a retained receipt (which defers the exclusion) or archived (and then
counts as delivered). An unparseable stored report is corruption and stops the runtime as before. Flags: `--isolated-negentropy-unavailable-exclusion-attempts`
(3–32), `-span-secs` (1800–86400), `-max-ids` (1–128), and
`--isolated-negentropy-disable-unavailable-exclusion` to stop new exclusions.

**Report times.** Schema 10 records `failure_reports.reported_at`. Older reports
have no time. Where a gap touches one, the span uses the runtime's fixed backoff
floor after that attempt, `min(60 * 2^(n-1), 3600)` seconds after failure `n`,
which has not changed since the runtime shipped (#60). The only path that can
shorten such a gap is a release that lost its lease and fell back to expiry's
60-second retry (`negentropy_isolated_stale_release_total`). Job 768's latest five
reports (attempts 11–15) have a floor of four hours, so it is excluded at the first
evaluation after deploy (attempt 16, consumed by the lease race without a report,
is skipped), with no new attempt and no manual ledger edit.

**Proof and accounting.** Each excluded ID gets a row in
`relay_unavailable_exclusions`: event ID, job, relay, window, first and last
attempt of the identical run, attempt count, first and last report times (when
known), the span lower bound, `excluded_at`, `resolved_at` and reason
`relay_unavailable`. The rows are self-contained and survive retention of the
job's detail. The job becomes `complete` with reason `relay_unavailable`;
receipts, attempt summaries, failure reports, archive markers and job IDs are
untouched. Retention adds the count to `completed_coverage.relay_unavailable`,
separate from policy `excluded` frames. This is not a full success: each excluded
ID logs one WARN line (`negentropy relay-unavailable exclusion: window completed
without an event the relay advertised but never delivered`, with job, relay,
count and event ID) and increments `negentropy_relay_unavailable_exclusions_total`;
`negentropy_relay_unavailable_exclusions_unresolved` gauges events not yet
archived; the status tool lists the latest exclusions and splits `complete` into
`reconciled`, `relay_unavailable_exclusion`, `split_with_relay_unavailable_descendant`
(a split parent completed because its children did, one of them by an exclusion)
and `already_archived`. Both completions fire the ledger's completion trigger, so
they advance `lifecycle.last_success` (the status tool's
`last_durable_completion_unix`) and count as durable progress for the
`durable_progress_stalled` watchdog, like any other completion.

**Health.** An excluded job is complete, so it no longer counts toward
`unresolved_failures`, and `/health/negentropy` and Kuma monitor 46 recover. Other
failures (relay down, protocol errors, blocked dense timestamps, mixed reports)
count exactly as before.

**Late arrival.** The excluded IDs are not added to the reconciliation inventory.
If the event later reaches the archive by any path (live ingestion, another relay),
it is admitted and deduplicated normally and nothing breaks; each tick checks up to
64 unresolved exclusions against the durable dedupe index and sets `resolved_at`
(`negentropy_relay_unavailable_resolved_total`). Rolling planning reconciles each
window once per relay, so nothing asks the relay again. A re-fetch scheduler for
unresolved exclusions is a follow-up, not part of this change.

**Interaction with the failure pause.** `unresolved_failures` starts when a
job's third attempt fails, and an exempt job still counts there. The pause does
not count it while its reports are on track to an exclusion: the latest reported
attempt qualifies, its identical run (the one an exclusion would measure) has not
met the rule yet, and the job is at most at attempt `2 × K` (10 by default).
Earlier reports do not matter, so a withheld set that shrinks (`{X,Y}` on
attempts 1–3, then `{X}` once the relay delivers `Y`) is excluded on the `{X}`
run without pausing; the first version of the exemption required every report
since the last protocol-complete attempt to name the same set and paused such a
job about 8,300 s in when the set shrank at attempt 5. The pause clock is per job, so failures
of different jobs never add up. #90 used one global clock that reset only when
no job was failing; two counter-examples paused before an exclusion could land,
and since nothing is leased while paused it then stayed paused until a restart:

- a `relay` or deadline report on attempt 3, then the same withheld ID again: the
  identical run restarts at attempt 4, so the fifth identical report is attempt
  8, after the 7,200 s grace had run out at attempt 7;
- two withheld windows about an hour apart: the second became a persistent
  failure before the first was excluded, so the global clock never reset and
  paused before the second exclusion.

Both now complete without pausing (`counter_example_a_*` and
`counter_example_b_*` in `sync/runtime.rs` drive them through the ledger with a
virtual clock, alongside the #90 clock to show where it paused). The exemption is
bounded: a set larger than `max-ids` or one fetch batch, a run that met the
rule but is held back by unarchived receipts, a disabled rule and a job past
attempt `2 × K` all count, so a relay that keeps changing what it withholds, or a
worker that keeps dying after one report, still pauses a grace after it stops
being exempt (about five hours after the third failure, when attempt 11 fails). A relay that stays down still pauses a grace after
the third failure. With the defaults an uninterrupted run from the first failure
is excluded at attempt 7 (short sessions) or 6 (nine-minute sessions). Raising
`K` delays the exclusion and raises the ceiling with it. `T` is measured over the
latest `K` reports, which the 60-minute backoff spaces about an hour apart, so a
`T` above roughly `K − 1` hours can never be met: such a window is never
excluded, and it counts toward the pause once past the ceiling. With one
relay a genuine persistent failure pausing everything is the intended policy;
revisit it before adding relays, because one bad relay would then pause healthy
ones too.

**Schema.** Opening a schema-9 ledger adds the column, table, index and coverage
column in one transaction and sets `user_version` 10. No row is rewritten or
dropped. A pre-change binary refuses a schema-10 ledger (`unsupported ledger
identity/version`): it leaves the isolated runtime off, so `/health/negentropy`
reports `runtime_failed`, while live ingestion continues. There are two rollback
paths (the RUNBOOK has the steps): keep the new build and restart it with
`--isolated-negentropy-disable-unavailable-exclusion` (recorded exclusions stay,
no new ones are made); or, only if the new build itself must go, stop the
ingester and the worker, restore the schema-9 backup taken before the upgrade and
start the old binary. The restore discards every exclusion and all
reconciliation progress since the upgrade (those windows are planned or retried
again) and re-issues the job IDs used since then, so keep the schema-10 file for
reference. As for any pre-upgrade backup, never restore it over receipts that are
not yet archive-confirmed: the current ledger must show `unresolved_receipts` 0.

## First increment: measure without deleting

Run `sudo -u pensieve python3 ops/scripts/negentropy-ledger-status.py /path/to/jobs.sqlite`
against schema 8, 9 or 10. The caller must own the database and existing WAL sidecars;
running as root against a pensieve-owned ledger is refused before SQLite opens.
It opens SQL read-only, limits SQL work to five seconds, reports aggregate
counts and separately sampled DB/WAL/SHM sizes, and refuses unknown identities.
It never emits lease tokens, policy-exclusion IDs, policy recipient lists or
failure bodies. On schema 10 it lists the five most recent relay-unavailable
exclusions with their event IDs, relays and windows (public data an operator needs
to look for the event elsewhere) and splits `complete` into reconciled and
excluded windows.
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
ineligible for automatic cleanup. Legacy pending creation times also stay unknown;
the persisted migration time starts their one-time progress grace period, never a
fabricated completion. Restart cannot reset that deadline. New successful detail
is kept at least 24 hours.
Each maintenance turn considers at most one root and 2,048 tree nodes, 4,096
detail rows and 8 MiB of estimated detail. Whole trees pinned by proof, oversized
trees are skipped. An unresolved sibling root does not pin completed trees; its
gap and split lineage remain detailed. Neither enabled nor disabled planners pin
completed history: they resume at their persisted next window.
Compaction uses exact 15-minute coverage bits per relay/day/policy, together with
frame/attempt/failure accounting. Repeated sweeps do not imply novel events.
Summaries expire 90 days after their last compaction and are capped at 16,384 rows;
a full summary table preserves detail rather than dropping obligations.
Schema 10 adds a separate `relay_unavailable` count per summary. Relay-unavailable
exclusion rows copy their proof (relay, window, attempts, report times), so unlike
policy exclusions they do not pin their job; they are kept when its tree is
compacted.

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

Compact only completed rolling roots whose whole split trees are
complete, have no remaining receipts, and have no exclusion-proof references.
Skip pinned trees; never evict their proof merely to free job slots. Preserve
compact per-relay/day coverage and received/archived/excluded/attempt/failure
accounting separately; call these frame counts, not unique new-event counts.
The bitmap records exact intervals and policy identity, not broader coverage.
Measure detailed-row growth against the retention defaults above; the three-job
trial did not establish sustained capacity.

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
one hour fails. Blocked jobs or jobs in retry backoff after at least three failed
attempts fail even if another job completes. An active attempt is not itself a
failure; protocol success awaiting archive durability remains subject to the
one-hour completion deadline and does not record durable success. Persisted creation/completion times survive
restart. An owner failure remains latched for that runtime; new process startup
must still satisfy the durable ledger checks rather than refreshing a grace clock.
Observations are refreshed after replay and planning, before a nine-minute worker
session; replay time is not charged against that session's observation freshness.
Retention exports `negentropy_retention_removed_jobs_total`,
`negentropy_retention_skipped_roots_total`, and `negentropy_retained_jobs`. A skipped
root may simply be too recent, not necessarily proof-pinned or unhealthy.

Install `ops/scripts/negentropy-kuma.py` as
`/usr/local/libexec/pensieve-negentropy-kuma.py` and the matching service/timer only
after the release gates. Store the new push URL, mode 0600 root-owned, in
`/etc/pensieve/negentropy-kuma.url`; systemd passes it with `LoadCredential`.
Create a **separate** Kuma push monitor with a 180-second missing-heartbeat deadline
and Svent Bot notifications. The timer samples every 30 seconds. Test 503→200
delivery with the explicit `--fixture` endpoint on localhost:19091; do not change
live archive admission to synthesize an alert. The probe reads at most 65 response
bytes and forwards a reason only if the entire body is an allowlisted fixed token;
arbitrary bodies are never sent to Kuma. Never put the push URL in argv,
logs, Git, or evidence. When intentionally disabling reconciliation, pause its
monitor; the explicit disabled response is not HTTP success. Keep Archive Admission
monitor 45 and API monitor 10 unchanged.
On this deployment, monitor 45 is an outbound **push** monitor (verified from Kuma),
not a direct HTTP probe; only monitor 10 is HTTP. The push path intentionally keeps
the private health endpoint unexposed rather than adding network access for Kuma.

### Faults during the bounded activation

A `blocked` dense one-second window is an unresolved gap, not a successful or
acknowledged window. There is deliberately no automatic acknowledgement or blanket
recovery procedure: operator recovery is incident-specific, and the fault must
stay red until recovery is proven. **Any blocked job, persistent failure, capacity
fault or stale/stalled runtime ends the bounded trial.** Do not keep running an
indefinite worker behind a permanently red monitor.

On such a fault, stop only `pensieve-negentropy-worker.service`, retain its ledger,
receipts, inventory and journals, and leave live ingestion alone unless archive
admission itself has failed. Keep the alert active. Capture the read-only ledger
status and the fixed `/health/negentropy` reason, then report the exact fault for
an operator recovery decision. Do not edit a job to `complete`, delete the gap,
reset its ledger, or pause the monitor to make the trial appear healthy. Resolving
a dense-second gap may require a separately reviewed inventory/partitioning change;
no continuous-operation approval follows from this bounded activation while any
such fault remains. The fixed reason in Kuma distinguishes the class of fault;
it is not a claim that one aggregate alert covers multiple concurrent incidents.

- Verify the new progress alert separately: archive-admission HTTP 200 does
  **not** prove reconciliation is advancing. Test both failure and recovery delivery.
- Measure a single-relay rolling run against the approved 14-day horizon before
  expanding the relay set. There are 1,344 root windows per full sweep per relay;
  the planner can revisit the horizon after a changed boundary. Three small
  windows do not establish the sustained service rate or time to the job cap.
- Fresh non-canary production ledger; never reuse or delete failed canary ledgers.
  Keep the existing inventory and its replay identity. Back up config and verify
  Parquet catch-up before the necessary controlled ingester restart.
- Premium Primal only initially, no catalog expansion. Preserve worker resource
  limits, recipient block and archive reserves. A finite monitored pilot is not
  proof of indefinite unattended capacity, even with bounded retention implemented.
- Enable the static worker only after reviewed code, green exact-head CI, verified
  binaries, alert delivery and the above gates. Stopping the worker must leave
  live ingestion and all unresolved gaps intact.
