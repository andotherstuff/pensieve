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
parent lease/canary budget, whichever is shorter). Inventory transfer has a
two-minute limit; connection is bounded to 20 seconds. Comparison allows five
minutes for the initial response and between accepted NIP-77 rounds. Only
matching replies successfully processed by the reconciliation engine count;
repeated reply digests, malformed replies, foreign subscriptions and pings do
not buy time. Comparison is additionally capped at 4,096 replies. These are
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
default 100,000 lifetime job slots for split recovery before planning more roots.
It preserves all unresolved gaps; neither the runtime nor planner prunes them.
Below 20 GiB available on the ledger filesystem, it continues bounded receipt
maintenance but pauses replay, new planning and leases, exposing
`negentropy_isolated_ledger_space_pause`.

The listener authenticates the accepted Unix peer against the dedicated systemd
unit, retains a pidfd, and rechecks identity and remaining service lifetime
immediately before assignment. It never infers a verified OOM from EOF. Local
inventory density is checked before leasing and split atomically without an
attempt; a one-second dense interval is blocked and logged. A concurrent append
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
full historical inventory coverage. Choose the replay floor deliberately. Do not
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
recovery, and the bounded canary remain operator gates. Repeated sweeps eventually
reach the lifetime job ceiling; a measured terminal-summary retention design
must precede indefinite operation. Existing older prototype ledgers are rejected
without migration or deletion and require an explicit recovery decision.
On a marked disposable systemd 257 host with the static worker unit already
active, `ops/tests/negentropy-binding-properties.sh` exercises the real fixed
bus query read-only; macOS fixture tests do not constitute that proof.
