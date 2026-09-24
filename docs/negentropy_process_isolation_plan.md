# Negentropy process isolation: implementation proposal

Date: 2026-09-23. Status: implementation plan approved; production rollout remains gated.
Supersedes the SDK-fork prerequisite in `negentropy_hardening_plan.md` for this
approach. Keep the pinned SDK unchanged. The earlier integrity, lifecycle and
local-inventory commits remain useful foundations, not deployed prerequisites
already satisfied in production.

## Decision and boundaries

Use two processes, one host, one repository, and one archive owner:

- `pensieve-ingest`: existing live ingestion plus a small bounded job scheduler,
  local IPC admission endpoint and durable reconciliation ledger.
- `pensieve-negentropy-worker`: a new executable and separate systemd service;
  one relay/window attempt per process lifetime. After an attempt it exits;
  systemd restarts it to request another job. When idle it waits for work on IPC.

No SDK fork, broker, network-facing ingestion endpoint, additional archive,
analytics change, or privileged systemd launcher inside the ingester. The worker
does not open RocksDB, SQLite, archive files, or ClickHouse. It receives a bounded
inventory from the ingester and returns candidate events over a local Unix socket.
The scheduler never runs relay SDK code. A worker cgroup is a sibling of ingestion,
not inside its cgroup. Global host failures remain possible; isolation is not a
claim that a shared disk or kernel cannot fail.

Only one worker initially. Backpressure must stop reconciliation before it slows
live admission materially. Do not enable the old in-process reconciler alongside
the new worker. Rollback is worker-off/live-ingestion-on, not automatically
returning to the known-risk in-process path.

## Research findings

Source inspected at local branch head `dda3b2c`:

- `main.rs`: reconciliation is a spawned task sharing the dedupe and segment writer.
- `sync/negentropy.rs`: bounded event queue/local inventory now exist, but the SDK's
  remote ID sets remain unbounded. Keep those allocations entirely in the worker.
- `pipeline/segment.rs`: seal flushes/fsyncs bytes, renames and fsyncs the directory,
  then calls `DedupeIndex::mark_archived`. Compression/consumer notification comes
  later. Notifications are not a durable receipt journal.
- `pipeline/dedupe.rs`: archived markers use synchronous RocksDB writes. Pending
  claims are not durable. This is the existing admission authority, not SDK counts.
- `SealedSegment` exposes IDs but not paired timestamps; timestamps currently kept
  in the open segment are conditional and consumed by watermark calculation.
- Maximum-age sealing currently depends on Parquet being enabled. Reconciliation
  completion must not depend on that flag or force a seal per event.
- Cold-start seeding now streams but has no durable completed-seed marker. A partial
  seed is inventory, not evidence that an interval is complete.

Read-only host check at approximately 13:20 UTC on 2026-09-23:
systemd 257.13; cgroup v2 with memory/cpu/pids controllers; ingestion active with
NRestarts=0. Segment 24439 sealed 21,856 events, published 21,856 Parquet rows with
zero rejects, and indexed 21,856 ClickHouse events. `/archive` had about 1.9 TiB
free, `/data` 335 GiB; available RAM 114 GiB, swap already about 13 GiB used.
These are point-in-time checks, not load-test results. No services were changed.

## IPC and ownership contract

Suggested socket: `/run/pensieve/negentropy.sock`. Authenticate Unix peer UID,
restrict directory/socket permissions to a dedicated IPC group, and use a dedicated
worker UID that cannot read `/data`, `/archive`, or production secrets. No TCP port.
Socket absence causes bounded reconnect/backoff, never an independent fallback.

Use a versioned length-prefixed protocol; reject lengths before allocation. Message
types: Hello, Job, InventoryChunk, InventoryEnd, Event, Accepted, ProtocolDone,
AttemptFailed, Cancel. All job traffic carries job ID, unpredictable lease token,
attempt number, protocol version and monotonically increasing sequence numbers.
Events use the existing signed event representation, independently verified by the
ingester. Treat malformed frames, stale tokens, wrong timestamps, unexpected message
ordering and frame/count/byte limits as attempt failures.

The ingester offers credit for bounded in-flight bytes/events. Accepted means only
that admission/receipt tracking owns the event; it does NOT mean archived. The
worker must not acknowledge success to itself merely because it wrote to the socket.
It must drain SDK-to-IPC callbacks before ProtocolDone; queue rejection or SDK
remote IDs not fetched make the attempt incomplete. ProtocolDone includes a count
and incremental digest of event frames; the ingester checks its own count/digest.
Worker exit alone, EOF, EOSE and progress counters never complete a job.

IPC parsing, validation and database work use bounded tasks/queues. Do not accumulate
full job payloads or use `wait_with_output`. An archive fault stops new job leases
and uses the approved fail-closed archive recovery policy.

## Durable ledger and completion

One new ingester-owned SQLite ledger, suggested `/data/negentropy/jobs.sqlite`,
WAL with FULL synchronous durability. Reuse the project's rusqlite dependency.
Do not store event payloads. Tables:

- jobs: relay, inclusive since/until, sweep ID, parent split ID, state, attempt,
  lease identity/expiry, next eligible time, terminal reason, counters/digests.
- receipts: bounded `(job, attempt, event ID, created_at, admission state)` rows.
- relay retry state: failures, cooldown, last protocol and durable successes.
- inventory replay cursor and sweep-planning cursors.

State machine: queued -> leased -> awaiting_durability -> complete.
Failures enter retry_wait, split or blocked; they never masquerade as complete.
Only one active lease and a bounded number of awaiting-durability attempts.

1. Validate a frame; durably register its ID/timestamp against the lease before
   archive admission. Batch registration is allowed, but credit cannot be released
   for records the parent cannot recover. Invalid events fail the attempt.
2. Admit through the same dedupe/segment-writer path as live events. Already archived
   duplicates are satisfied; pending duplicates wait. Never treat pending as archived.
3. Poll receipt IDs in bounded batches against durable archived markers. Do not
   rely solely on an in-memory seal notification. A dedicated, bounded database
   executor prevents blocking the async runtime; its queue also needs a ceiling.
4. After valid ProtocolDone and satisfaction of every receipt, commit complete in
   SQLite. Record zero-event successful attempts explicitly. Completion means the
   relay's observed reconciliation was accounted for, not proof of global Nostr history.

Crash ordering is deliberately at-least-once: archived-before-ledger-complete means
recheck/retry; ledger-received-before-archive means retry; expired lease means retry.
No cross-database distributed transaction is required. Restart must first honor any
archive recovery gate. A broken IPC connection or worker kill preserves unfinished
work, including events already durably received. Lost final acknowledgement can
cause redundant downloads but cannot justify skipping a gap.

Generalize the existing maximum-age seal timer so enabled reconciliation has a
durability bound even with Parquet disabled. Start with the existing five-minute
operational cadence, configurable; never force a tiny segment per job/event.

## Inventory, windows and scheduling

The ingester alone serves complete capped time intervals from sync-state. Do not
truncate ID lists. Read/export incrementally with a fixed row and byte budget;
concurrent appends can safely cause redundant delivery, not falsely claimed absence.
Reject/verify legacy inventory entries that cannot be confirmed archived before
advertising them. An incomplete inventory is safe but less efficient; it is not
coverage. No worker opens a second RocksDB handle.

For reliable ongoing inventory, replay sealed segments with a persisted monotonic
cursor and bounded streaming decode, using seal notifications only as wake-ups.
Insert `(ID, timestamp)` after the archive seal; flush inventory before advancing
the replay cursor. Crash between them replays idempotently. Detect segment-number
gaps and pause cursor advancement. Start from a deliberate rollout floor and retain
the earlier verified inventory; do not silently launch an all-history rebuild.
Do not prune inventory or delete source segments while retained jobs/replay require it.

Approved policy: initial scope remains 14 days. Preserve
unfinished intervals even after they age out. Older history is separately authorized.
Freeze each sweep's upper boundary; do not chase moving now during an attempt.
Initially plan 15-minute inclusive windows, with no timestamp boundary overlap or gap.
On local inventory overflow split before leasing; on resource-limit/volume failure
split the attempted interval. Split `[a,b]` into `[a,m]` and `[m+1,b]` atomically.
Parent is resolved only when all children complete. At a one-second interval, stop
splitting and alert/back off; never spin or silently drop a too-dense timestamp.

Do not split ordinary connectivity errors: retry the same window with persisted
exponential backoff and jitter (proposed 1 minute to 1 hour). Unknown worker loss
gets one same-window retry before subdivision. Confirm OOM vs timeout from service
evidence where possible; do not assume every disconnect was an OOM.

Round-robin relays and reserve scheduling capacity for oldest retained gaps as well
as fresh intervals. Repeated rolling sweeps allow late-published events to be found;
completed intervals do not mean that a relay can never acquire more events there.
Plan lazily from cursors rather than pre-enqueueing unlimited history. Explicit
small allowlist for canary; missing/empty configuration fails closed for negentropy,
not live ingestion. Do not automatically expand targets from the relay catalog.

## Proposed initial limits (canary-tuned, not validated throughput claims)

| Resource | Initial proposal |
| --- | --- |
| Active workers | 1 |
| Worker CPU / memory high / memory maximum | 1 CPU / 1 GiB / 2 GiB |
| Worker swap / tasks / core dumps | 0 / 64 / disabled |
| Worker service wall time | 10 minutes plus 5-second kill grace |
| Application job deadline / idle deadline | 9 minutes / 2 minutes of no useful progress |
| Local inventory | 250,000 IDs, fixed-width chunks; fail/split above cap |
| IPC event frame / outstanding credit | 1 MiB / 8 MiB and 16 events |
| Attempt output | 50,000 frames or 64 MiB, whichever first |
| Awaiting durability | 2 attempts maximum; no new leases beyond this |
| Ledger budget | 1 GiB including WAL; pause leases and alert before exhaustion |

Count real admitted/reconciled progress for idle timers, not arbitrary heartbeat
traffic. Kernel wall timeout is the backstop if SDK code never yields. Limits may
cause incomplete jobs; they must not silently exclude legitimate large events.
Use existing disk-reserve policy where stricter; initially require at least 20 GiB
free on `/data` for leasing, and respect the archive's own admission gate/reserve.
Bound ledger rows/errors and check actual allocated SQLite/WAL bytes. Compact only
terminal receipt detail after a durable summary; never purge unresolved jobs to
meet a quota. Avoid routine full VACUUM and any temporary duplicate archive.

## Linux service contract

Static worker service, `Type=simple`, dedicated user, `Restart=always` with paced
restart/start-rate protection, `RuntimeMaxSec=10min`, `TimeoutStopSec=5s`,
`KillMode=control-group`, `OOMPolicy=kill`, `MemoryHigh=1G`, `MemoryMax=2G`,
`MemorySwapMax=0`, `CPUQuota=100%`, `TasksMax=64`, `LimitCORE=0`.
Worker gets no privilege to move cgroups, start units or write archive data.
Verify sandbox paths, socket access, DNS/Tor and required crypto/runtime files on
the target system; don't assume a hardening directive is harmless.

Systemd owns worker cleanup, even when the ingester crashes. Parent closes the
lease connection on cancellation; worker exits, with service timeout as backstop.
The ingester rejects late traffic after lease expiry. Restart pacing must permit
normal short jobs without disabling the service; idle workers wait instead of
rapidly exiting. A stopped service is not restarted by the ingester.
When manually disabled, no job state is lost. The service must not share the
ingester's memory-limited cgroup, and effective limits must be checked before leasing.
This is memory-failure containment, not an SDK allocation bound or complete hostile
code sandbox. Host-wide OOM remains outside this guarantee.

## Reviewable implementation slices and acceptance gates

1. **Ledger and protocol types** (`sync/jobs.rs`, `sync/ipc.rs`): migrations,
   framed parsing, bounded credits, lease transitions, retry/split accounting.
   Gate: crash/reopen at each transition, stale token, duplicate frames, oversized
   length, truncated messages, one-second split and ledger-full tests. No runtime wiring.
2. **Durable admission and inventory** (`main.rs`, `pipeline/segment.rs`,
   `sync/state.rs`): unify live/worker admission, receipt reconciliation, independent
   seal cadence, bounded sealed replay. Gate: pending-vs-archived races, concurrent
   live duplicates, crash before/after seal/dedupe/ledger commit, missing-segment
   cursor tests and Parquet-disabled durability test. No success from notifications alone.
3. **Worker executable and scheduler** (`src/bin/negentropy-worker.rs` plus sync
   modules): one job/process, unchanged SDK, exact interval, streamed IPC, parent
   scheduler, explicit allowlist, persisted backoff. Gate: fake relay floods/hangs,
   no progress, IPC loss, truncated output, failed callback and partial-fetch tests;
   live handler continues with bounded queues while worker fails.
4. **Systemd isolation and observability** (`ops/systemd`, Prometheus, runbook):
   peer credentials, sandbox, memory/CPU/time limits. Gate on Linux: intentionally
   OOM/hang/kill ONLY synthetic worker; verify ingestion PID/restarts unchanged,
   worker cgroup empty, retry retained, and no orphan process. Local macOS tests
   cannot substitute for this gate. Test archive/ledger failure separately.
5. **Controlled rollout**: baseline health and disk checks, exact binary hashes,
   config backups, one deliberate graceful ingester restart to enable IPC and
   disable old reconciliation; initial worker canary on one approved relay.
   Gate: at least three successful durable jobs plus injected worker recovery,
   then 24-hour soak with backlog age, throughput, memory and ingestion rates.
   Expand only when service capacity exceeds incoming job demand. No automatic
   adoption of all catalog relays. Rollback stops worker and leases, preserves ledger.

Run focused regressions and mandatory `just precommit` immediately before each
commit. Keep commits separate, no deployment during development. Verify alert
delivery, not just rule syntax: worker unavailable with queued work, old unresolved
gap, job age >20 minutes, no durable success for an hour when work exists, repeated
OOM/limit failures, ledger budget, and archive recovery required. Clear warnings
only after the relevant recovery condition actually resolves. Bound metric labels;
keep job IDs in logs, not Prometheus labels.

## Sources and remaining decisions

Primary systemd documentation confirms that cgroup memory limits contain unit OOM,
runtime limits apply to simple services (not oneshot), and control-group kill mode
cleans the service's processes. Verify installed-version behavior during the Linux gate:

- https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.resource-control.xml
- https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.service.xml
- https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.kill.xml
- https://docs.rs/tokio/latest/tokio/process/struct.Child.html (researched child ownership;
  static systemd service avoids relying on best-effort child drop/reaping in ingestion).

The user approved the 14-day initial horizon, retention of unfinished gaps until
recovered, and lightweight per-relay/per-interval accounting. Record each attempt's
checked-at time, outcome/reason, received events, durably novel events, archived
duplicates, pending duplicates, transferred bytes and runtime. Pending and merely
accepted events must not inflate durable novelty. Bound detailed attempt retention
and retain compact summaries without deleting unresolved gap state. Successful
zero-new-event checks are not evidence that a relay has no events in the interval.
Automatic historical exploration is deferred to `negentropy_historical_exploration.md`;
it is not a prerequisite for the reliability rollout.

No additional product decision is needed to begin the ledger/protocol slice.
All numeric limits above are proposed
engineering defaults for a measured canary, not a claim that they fit current relay
volume. Exact canary allowlist, throughput/freshness acceptance thresholds, and alert
receiver must be verified before rollout. No SDK changes are needed for this design.
