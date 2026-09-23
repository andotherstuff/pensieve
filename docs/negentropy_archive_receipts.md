# Isolated reconciliation: archive receipt consumer (increment 2a)

Stacked above PR #46. This increment connects the upload library to the **existing
ingester-owned** `DedupeIndex` and `SegmentWriter`. It adds no socket, scheduler,
worker executable, service, production migration, or analytics change. The old
reconciler remains the only runtime caller until the later integration gates pass.

## What now owns completion

1. `UploadSession::receive` commits the received ID before returning a candidate.
2. `admit_and_accept` uses the shared reservation, notepack encoder, and writer.
   Only that admission path can release public upload credit. An already archived
   duplicate need not be written again; an in-flight duplicate still waits for
   archive proof. Legacy on-disk Pending is not an acknowledged archive owner.
   Errors poison the session, preserve the receipt, and never produce an ACK.
   The lease clock is checked before admission and again before acknowledgement.
3. A matching ProtocolDone atomically releases the capability and moves the job
   to `awaiting_durability`. It is **not completion**, including for zero events.
   At most two such jobs can accumulate before new leases stop.
4. `JobLedger::reconcile_archived` checks up to 256 receipt IDs against durable
   Archived markers in the same index used by the writer. Pending, missing IDs,
   SDK success, notifications, EOF, and worker exit are not evidence of archival.
   An archive-recovery latch rejects reconciliation. Startup archive recovery
   must finish before the caller uses these APIs.
5. A job becomes complete only after a valid current protocol summary and no
   remaining received obligations from **any** attempt. A zero-event retry cannot
   erase IDs committed by a prior failed worker. Split parents additionally need
   both children complete and their own received obligations satisfied.

This certifies accounting for an observed reconciliation, not global relay history
or remote object-store upload. Existing Archived markers mean local durable seal.
The caller must supply the actual shared writer/index pair, not independent stores.

## Bounded cleanup, recovery, and retained accounting

Each call scans a bounded, ordered batch using a persisted `(attempt, sequence)`
cursor. At the end it wraps; one missing early ID therefore cannot starve later
IDs. Calls with no rows may only reset the cursor. No live upload is compacted.

After an attempt has ended (protocol success, retry, split, or block), an
archive-confirmed receipt's detail can be removed. In the same FULL transaction,
its attempt's archived-frame counter is incremented and the global retained-row
counter decremented. Count, byte total, wire digest, protocol outcome, job,
interval and lineage remain. Missing receipts are never deleted. Archived counts
are **frames**, including repeated IDs, not durable novelty or unique events.

Compaction and explicit `retry_durability` bypass admission ceilings, as lease
expiry does, but actual database/IO errors still fail. A failed compaction rolls
back deletions, counters, cursor and completion together. The next call can replay
archive checks safely. `retry_durability` fences the parent's recovery decision by
attempt number and retains every ID while retrying the same window.

The retained receipt ceiling is now runtime policy (`max_receipts`, default
100,000), alongside the job and byte ceilings. Raising it requires a capacity
decision, not deleting unfinished work. No full VACUUM or archive-file cleanup is
performed; freed SQLite pages can be reused. Job and attempt summaries are retained
and therefore are **not** unbounded-lifetime storage: the byte/job ceilings still
pause admissions and require future measured retention policy or operator action.

The undeployed prototype schema is version 3. Versions 1 and 2 fail closed without
mutation; there is intentionally no speculative migration for unshipped databases.
Never point this prototype at production state or delete incompatible state.

## Verification and remaining gates

Integration tests use real temporary SQLite, RocksDB and notepack files. They cover
pending/archived duplicates, compaction reuse across more receipts than the cap,
registration without admission, older failed-attempt obligations, cursor fairness,
transaction rollback, explicit zero-event success, awaiting capacity, stale recovery,
split-parent completion, deadline expiry during admission, archive failure, and
recovery above the admission ceiling. A subprocess exits without destructors after
sealing, then a new process reopens both databases and completes the receipt check.
No Parquet consumer or seal notification is involved. This is process-loss testing,
not a power-loss or real disk-exhaustion certification.

Still required before enabling the isolated worker:

- Bounded executor/queues and scheduler integration; none of these synchronous
  calls belong on an async reactor thread. Select retained jobs fairly and keep
  reconciliation ticking while leases are paused.
- Independent maximum-age sealing, including when Parquet is disabled. These tests
  explicitly call the real seal method; they do not establish a runtime seal timer.
- Bounded sealed-inventory replay with a durable cursor, gap detection and a
  deliberate rollout floor; no all-history rebuild is introduced here.
- Complete authenticated Unix transport and worker process, followed by Linux
  resource-limit, alert delivery, throughput and production canary gates.
- Measure per-event FULL-commit cost and receipt polling under real storage load
  before choosing batching or claiming sustainable reconciliation throughput.

No deployment, production ingestion restart, SDK fork, or merge is part of 2a.
