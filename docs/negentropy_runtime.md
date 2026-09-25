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

This is a development milestone, not a rollout approval. The Linux cgroup
OOM/hang/kill proof, alert delivery, exact canary allowlist, throughput/freshness
targets, disk preflight and backups, three durable canary jobs with injected
recovery, and a 24-hour soak remain operator gates. Repeated sweeps eventually
reach the lifetime job ceiling; a measured terminal-summary retention design
must precede indefinite operation. Existing older prototype ledgers are rejected
without migration or deletion and require an explicit recovery decision.
