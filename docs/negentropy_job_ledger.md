# Isolated reconciliation: durable job ledger (increment 1a)

This increment implements the persistent obligation ledger, **not a running
replacement for the current negentropy scheduler**. It is exported as
`pensieve_ingest::sync::jobs` but has no runtime callers, CLI, migrations against
production databases, or deployment changes. PR #42's old scheduler is not a
prerequisite.

## Contract

- The ingester owns a dedicated SQLite database using WAL and synchronous FULL.
  The future integration must put it in an owner-only directory, inaccessible
  to the worker, and call it through a bounded blocking executor.
- A job names a caller-defined frozen sweep, canonical relay URL, and inclusive
  Unix-second interval. Re-enqueueing the same root is idempotent. Overlapping
  roots within the same sweep/relay are rejected; later sweeps may revisit them.
- Unfinished windows never expire with the lookback. The scheduler will seed a
  14-day lookback, but this storage layer neither plans nor truncates windows.
- Immediate transactions and a database constraint allow one leased job across
  connections. Each attempt gets a fresh random 256-bit capability and a
  deadline of at most ten minutes. Tokens are excluded from Debug output.
- Expiry rejects worker mutations at the exact deadline. Recovery retains the
  same window in persisted backoff; it does not infer an OOM or split it.
  Delays are constrained to 60–3,600 seconds; policy and jitter come later.
- Explicit resource splits atomically retain the parent and create `[a,m]` and
  `[m+1,b]`. A one-second interval becomes blocked, not complete. Only a live
  lease can perform these transitions.
- The stacked [archive receipt consumer](negentropy_archive_receipts.md) adds
  awaiting-durability and complete states. Network success alone never completes
  a job; every retained receipt must be archive-confirmed first.

## Bounds and failure behavior

Default admission limits are 100,000 rows (including split parents) and 1 GiB.
The byte check accounts for database/WAL/shared-memory files, allocated blocks
on Unix, logical database pages, and a 64 KiB write reserve. WAL auto-checkpoint
is set to 16 pages. Limits are runtime policy; after checking available capacity,
an operator can reopen with larger limits without deleting existing obligations.
An unsupported schema fails closed.

The byte limit is **not a hard filesystem quota**. SQLite can allocate pages
during commit or rollback. Deployment still needs free-space preflight,
monitoring, and process/filesystem limits. Exceeding the admission ceiling
rejects new admissions and leases, while retry/expiry can still release an old
lease. Those writes may still fail on an actually full filesystem. Existing jobs
remain available for inspection and recovery. The archive consumer compacts only
confirmed receipt detail; no vacuum or unresolved-job deletion is provided.

Database errors roll back the transaction where SQLite permits rollback. A
commit I/O error must be treated as uncertain by future callers: reopen and
inspect persisted state before retrying an operation. Never infer job completion
from an exception, a worker exit, or the absence of a lease response.

## Verification

Tests use real temporary SQLite databases and cover canonical/idempotent roots,
old-gap retention, reopen of all states, expiry boundaries, stale/forged tokens,
concurrent connections, attempt overflow, gap-free splitting, single-timestamp
blocking, row/byte admission rejection, and rollback when the second child
insert fails. A subprocess exits without destructors with both committed work
and an uncommitted transaction; reopening preserves the lease and discards the
uncommitted change. This is a process-loss test, not a power-loss guarantee or a
full-disk integration test.

## Remaining slices and rollout gates

Increment 1b adds the [bounded upload and received-receipt foundation](negentropy_upload_protocol.md),
without a listener or archive integration. Remaining work:

1. Authenticated local IPC, handshake/assignment/inventory exchange, and
   archive-confirmed completion. No worker ACK alone completes a job. Freeze the
   complete protocol and transport limits before adding runtime callers.
2. Bounded sealed-archive inventory and receipt integration, including replay
   cursors and crash tests around admission, sealing, and acknowledgement.
3. Isolated one-relay/window worker and fair bounded scheduler. Add small relay
   allowlist, 14-day initial planning, retained gaps, backoff, and resource-aware
   subdivision; do not automatically explore all history yet.
4. Systemd/cgroup limits, local peer identity checks, disk reserve checks, metrics,
   and Uptime Kuma health alerts that remain unhealthy until recovery succeeds.
   Exercise cancellation/OOM/timeout/full-disk failures on Linux.
5. Separately approved production canary and soak before replacing the old
   in-process loop. Preserve archive ownership and uninterrupted live ingestion.

Historical exploration remains separate future work (issue #39). This increment
does not discover relay coverage or claim that a completed reconciliation proves
all events in a time range exist locally.
