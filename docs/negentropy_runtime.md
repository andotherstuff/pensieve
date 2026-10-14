# Isolated reconciliation runtime (inactive by default)

The ingester now has an explicit `--isolated-negentropy` mode, mutually exclusive
with the legacy `--negentropy` in-process loop. The default remains unchanged.
There is no automatic service start, deployment, relay discovery, historical scan,
or production enablement in this change. The static systemd worker is managed by
the operator and cannot be launched by the ingester.

Activation requires all of `--isolated-negentropy-relays-file`,
`--isolated-negentropy-worker-uid`, and
`--isolated-negentropy-replay-floor`. The relay file is strict and nonempty
(1-32 URLs). No catalog targets or default relays are added. The worker UID must
be dedicated and non-root. The default listener is
`/run/pensieve/negentropy.sock`; the runtime directory and its IPC group must
be provisioned separately. The default ledger is
`/data/negentropy/jobs.sqlite`; its parent directory must already exist. The
archive seal cadence must remain nonzero even without Parquet.

The operator must install `ops/systemd/pensieve-negentropy-tmpfiles.conf`
before creating the socket: `/run/pensieve` needs owner `pensieve`, group
`pensieve-ipc` and setgid mode `2770` so the ingester-created `0660` socket
inherits that IPC group. The worker unit has `pensieve-ipc` only as a
supplementary group; it must not acquire the ingester's archive/data group.
The ingester's `ProtectSystem=strict` sandbox must explicitly include
`ReadWritePaths=/run/pensieve` in addition to its existing writable paths.
Provision `/data/negentropy` with a traversable parent (for example root:pensieve
0750) and a pensieve-owned ledger directory. Verify access as the service user
and inside the service mount namespace, not only as root on the host.

### Worker progress budgets

Each assigned attempt retains its nine-minute overall limit (or the remaining
parent lease/canary budget, whichever is shorter). The parent also caps the
session by the remaining worker service lifetime as described below.
Inventory transfer has a
two-minute limit; connection is bounded to 20 seconds. Comparison allows five
minutes for the initial response and between accepted NIP-77 rounds. Only
matching replies successfully processed by the reconciliation engine count;
repeated reply digests, malformed replies, foreign subscriptions and pings do
not buy time. Repeated digests and malformed replies terminate the attempt as
incomplete; unrelated messages are ignored. Comparison is additionally capped at 4,096 replies. These are
experimental bounded-canary settings, not a latency guarantee.

Fetching/draining has a two-minute progress allowance, refreshed by parent
admission acknowledgments or completed, fully satisfied fetch batches. An
individual IPC upload/acknowledgment also has its own two-minute bound, which
relay progress cannot extend. Empty successful comparisons require no new
events. None of these signals implies archive durability. Logs report local
inventory count, comparison rounds/missing IDs, completed fetch batches and
admitted counts, plus the current phase and wall-versus-phase deadline on
timeout. No event payloads or capability tokens are logged. The systemd
ten-minute worker lifetime remains the non-cooperative-process backstop.

At startup, the ledger, existing inventory and writer/dedupe authority are
checked before binding. The single owner thread plans at most 32 lazy 15-minute
windows per turn within the approved 14-day rolling horizon, replays sealed
segments from the explicit floor, performs bounded receipt maintenance, and
leases one job to one authenticated service process. It reserves 2,048 of the
default 100,000 retained job slots for split recovery before planning more roots.
It preserves all unresolved gaps; neither the runtime nor planner prunes them.
Below 20 GiB available on the ledger filesystem, it continues bounded receipt
maintenance but pauses replay, new planning and leases, exposing
`negentropy_isolated_ledger_space_pause`.

The listener authenticates the accepted Unix peer against the dedicated systemd
unit, retains a pidfd, and rechecks identity and remaining service lifetime
immediately before assignment. Archive replay and idle waiting consume the
worker's ten-minute service lifetime, not a new full nine-minute reservation.
The final recheck caps the parent session at the smaller of nine minutes and
the remaining service lifetime minus a ten-second safety margin (also capped
by the fixed-canary intake budget when applicable). At least two minutes must
remain after that margin to admit a session; otherwise no work is assigned to
that process. The ten-minute systemd backstop and all identity checks remain
unchanged. Session-budget logs expose the actual admitted duration.
The listener never infers a verified OOM from EOF. Local inventory density is
checked before leasing and split atomically without an
attempt; a one-second dense interval is blocked and logged. One fair-lease
command preflights at most 16 candidates and starts no new one after 60 s
(`negentropy_preflight_budget_exhausted_total`); it then returns no lease and
the next tick, which refreshes health first, continues from the durable splits. A concurrent append
between preflight and session can still cause a later TooDense outcome, which
remains an unresolved gap. Only an authenticated `Volume` report from the worker
triggers a post-lease split. Other failure reports and unknown loss retain the
same window under paced retry.

`ProtocolDone` is only a transport result. The ledger completes a job only after
all received event IDs have durable archive markers; the periodic archive seal
timer and bounded receipt maintenance are independent of optional Parquet. ACK,
socket close, worker exit and in-memory seal notifications never complete work.
Archive recovery or local ledger faults pause the isolated runtime and leave
receipts/jobs intact. `negentropy_isolated_ready`,
`negentropy_isolated_recovery_required`, planner backpressure and binding
rejection metrics expose its state separately from live-ingestion health.
An isolated startup fault (including invalid allowlist, unavailable ledger or
occupied socket) marks `negentropy_isolated_unhealthy=1` and leaves live ingestion
running, without activating the legacy loop. An existing socket is never removed
automatically: the operator must establish that no active owner exists before
recovering a stale path. Isolated shutdown errors are reported only after the
archive seal timer, final seal, dedupe flush and checkpoint cleanup have run.
Replay checks cancellation between bounded decoder batches and services receipt
maintenance during a long sealed segment.

### Fault scope

Only faults that genuinely affect every job stop the runtime and latch
`runtime_failed`: archive recovery, ledger storage/budget errors, invalid or
corrupt ledger input, and an owner-thread failure. Per-attempt outcomes do not:
a worker failure report, EOF, a protocol or deadline error, or a release whose
lease already lapsed (`negentropy_isolated_stale_release_total`; `expire`
recovers the lease on a later tick) each retain the same window as durable
unresolved work and the runtime continues. Each authenticated failure logs
`isolated worker attempt failed; gap retained for retry` with the job, attempt,
relay, failure kind, outstanding-ID count and retry delay (never event IDs or relay
text), and increments `negentropy_worker_attempt_failed_total`. The preflighted
fair candidate is the job that is leased (see [fair leases](negentropy_fair_leases.md));
a candidate that changed meanwhile is reselected, not treated as a fault.

### Relay-unavailable exclusions

Each owner lifecycle turn (twice per tick, before leasing) first checks retry-wait
jobs against the relay-unavailable rule: the latest five consecutive reported
attempts all reported the identical, fully enumerated and explicitly requested set
of at most eight advertised-but-undelivered IDs, spanning at least an hour, and
every received frame is archived. Attempts that ended without any report (lost
worker, lease without a session) are skipped, not counted. IDs of the set that
already carry a durable archive marker count as delivered. The remaining IDs are
excluded: the job completes with reason `relay_unavailable` and a proof row per
ID; the WARN line `negentropy relay-unavailable exclusion` names the job, relay,
count and event ID, and `negentropy_relay_unavailable_exclusions_total` counts
them. When no ID remains (all were archived, for example before the replay floor,
which the inventory never lists), the job completes with reason
`already_archived`, no exclusion row, the INFO line `negentropy window completed:
every ID the relay withheld was already archived` and
`negentropy_relay_unavailable_already_archived_total`. Both completions advance
the ledger's `last_success` (`last_durable_completion_unix`), so they count as
durable progress for the stall watchdog. This replaces the former no-skip rule
for this one case, after production job 768 (2026-10-13) retried one withheld
Premium Primal event fifteen times. Other `Unavailable` outcomes still retain and
retry the gap. The rule, its flags
(`--isolated-negentropy-unavailable-exclusion-attempts`, `-span-secs`,
`-max-ids`, `--isolated-negentropy-disable-unavailable-exclusion`, which conflicts
with the three tuning flags), late arrival and accounting are described in
[continuous readiness](negentropy_continuous_readiness.md#relay-unavailable-exclusions).

**Carried exclusions.** The same turn also completes a retry-wait job, from its
first failure, whose latest reported attempt qualifies and names only
*known-withheld* IDs: each has an unresolved original exclusion row for the
job's relay whose window overlaps the job's window, and no archive marker. This
covers a later planner pass over history revisiting an excluded window, as job
1984 revisited job 768's window on 2026-10-13 and, under per-job proof, held
health red for about an hour re-earning the exclusion. The job completes with
reason `relay_unavailable`; its proof rows record its own run and point at the
original row in `carried_from`; the WARN line `negentropy relay-unavailable
exclusion carried forward` names job, relay, event ID, `carried_from` and
`original_job`; and `negentropy_relay_unavailable_carried_total` counts the
events (not `negentropy_relay_unavailable_exclusions_total`). A set that mixes
known-withheld and new IDs is not carried. Because the turn runs before every
lease, a carried job never reaches the three failures behind
`unresolved_failures` (see
[carried exclusions](negentropy_continuous_readiness.md#carried-exclusions)).
Carried exclusions are ingester-only: the IPC version and the worker binary are
unchanged.

The IPC version is unchanged. The worker changed in one respect: before it
reports `Unavailable` it re-requests exactly the still-missing IDs, at least once
and again while each request delivers something new, so a relay that caps the
results of one REQ below the 128-ID batch is not mistaken for one that withholds
(see [worker](negentropy_worker.md)). An older worker still interoperates, but
its reports do not carry that protection; deploy the worker before relying on
exclusions against relays with result limits.

### Pausing on persistent failure

`--isolated-negentropy-pause-on-failure-secs <SECS>` (unset by default) stops
leasing once one job has been a persistent failure continuously for `SECS`. The
candidates are the jobs behind `/health/negentropy` `unresolved_failures` (a
blocked job, or a retry-wait job after at least three failed attempts), with one
exemption below. The clock is per job and in memory: it starts at the first tick
that observes the job counted and is dropped as soon as a tick does not, so
failures of different jobs never add up to one grace, and a restart allows one
more grace period of paced retries before pausing again. Ticks observe between
worker sessions, so a job that is retried and fails again stays continuously
counted. The pause logs `isolated reconciliation paused leasing` once and sets
`negentropy_isolated_failure_pause=1`. Replay, planning, archive maintenance,
exclusions and health observation continue; no lease is granted, so no attempt
is consumed and every gap and receipt is kept. Health stays red. The connected
worker idles and is recycled by its service lifetime. Leasing resumes only once
no counted job is past the grace; since nothing is retried while paused, in
practice that means an operator restart without the flag (or after resolving the
job). `0` pauses as soon as a counted failure is observed.

**Exemption for windows on track to an exclusion.** A retry-wait job does not
count while its reports show it heading for a relay-unavailable completion: its
latest reported attempt is a qualifying `unavailable` report (fully enumerated,
within one 128-ID fetch, at most `max-ids` IDs), that report's identical run (the
one an exclusion would measure) has not met the rule yet, and the job is at most
at attempt `2 × attempts` (10 by default). Reports before the current run do not
matter: another failure kind or a changed set (a relay that starts delivering one
of two withheld IDs) starts a new run (an attempt without a report is skipped and
does not break the run), which
then needs five identical reports of its own, and past the attempt ceiling the
job counts whatever it reports. An unqualifiable latest report, a run that met
the rule but is held back by unarchived receipts, a disabled rule and blocked
jobs all count. The exempt job still holds `/health/negentropy` at
`unresolved_failures`. Every tick, whether or not the pause is enabled,
`negentropy_isolated_failure_pause_exempt_jobs` gauges the exempt jobs and
`negentropy_isolated_failure_pause_counted_jobs` the ones the pause counts. With this, a withheld window is completed instead of pausing
reconciliation, including when a `relay` or deadline report interrupts its run or
when several withheld windows overlap; a genuine persistent failure, such as the
relay being down for more than the grace, still pauses all leasing. With one
relay that is the intended behaviour; revisit it before adding relays, because
one bad relay would then pause healthy ones too.

## Bounded canary

`--isolated-negentropy-canary-until <UNIX_SECONDS>` replaces rolling planning
with exactly three adjacent 15-minute root windows immediately before the given
exclusive upper boundary. The boundary must be past and divisible by 900. This
mode requires exactly one explicitly approved relay and a **new dedicated ledger**;
it refuses an existing ledger rather than mixing unrelated work or automatically
restarting a finished canary. Canary jobs durably prohibit rolling planning even
if the flag is omitted on a subsequent start. Never delete that ledger to retry: retain unfinished
jobs and evidence for an explicit recovery decision.

The mode admits at most six worker attempts, replays at most three sealed segments
from the explicit floor, and budgets intake for the first 20 minutes. Reaching
the attempt limit stops new leases, not receipt maintenance. Maintenance continues
until the 25-minute shutdown deadline so independent live sealing can make admitted
events durable. Require an archive seal cadence of at most 300 seconds for this
trial. Any job still awaiting durability at shutdown is inconclusive, not a pass.
The final five-minute margin is for worker shutdown and evidence collection; an already-running
filesystem operation cannot be forcibly interrupted safely. Live ingestion and its
seal timer continue. Worker service management remains the operator's responsibility;
stop the worker at the end of the test. An expired deadline or exhausted budget is
**not success**: verify the three root windows and any split children, archive
durability, exclusion behavior and worker-loss recovery from the retained ledger.

This mode uses the existing inventory and only bounded replay; it does not certify
full historical inventory coverage. Choose the replay floor deliberately. On a
repeat canary with the same sync database, retain its original replay floor:
the persisted cursor resumes at its recorded next segment. A fresh job ledger
does not reset inventory identity. Never calculate a new floor from the newest
archive segment or delete the inventory to bypass a mismatch. Do not
run a second archive/dedupe owner to avoid an ingester restart. Production use still
requires reviewed binaries, backups, disk preflight, isolated-worker provisioning,
and archive-admission safety checks. Owner decision (2026-10-09): use a 20–30 minute
canary instead of waiting for the initial 24-hour soak because disk capacity is
constrained. This does not certify the hour-long retry tail, long-term ledger/WAL
growth or retention; those remain follow-up operational observations before
expanding the relay set or declaring indefinite unattended operation ready.

This is a development milestone, not a rollout approval. The Linux cgroup
OOM/hang/kill proof, alert delivery, exact canary allowlist, throughput/freshness
targets, disk preflight and backups, three durable canary jobs with injected
recovery, and the bounded canary remain operator gates. Successful closed rolling
sweeps now have bounded detail retention and compact coverage accounting; unresolved
gaps and exclusion-proof owners remain pinned. Schema 8 migrates transactionally
to 9, without inventing legacy completion timestamps. Schema 9 migrates to 10 by
adding failure-report times, the relay-unavailable proof table and a separate
coverage count; no row is rewritten and a pre-change binary refuses the result.
Carried exclusions add a nullable column and a partial index within schema 10, at
open and idempotently; a schema-10 binary without them (ingest-85a2402) still
opens and serves such a ledger. Older unsupported prototype
ledgers remain rejected without deletion. See
[continuous readiness](negentropy_continuous_readiness.md) for the backup,
retention, separate progress-alert, and bounded activation requirements.
On a marked disposable systemd 257 host with the static worker unit already
active, `ops/tests/negentropy-binding-properties.sh` exercises the real fixed
bus query read-only; macOS fixture tests do not constitute that proof.
