# Isolated reconciliation: bounded archive inventory (increment 2c)

> Point-in-time increment note. The runtime is now wired behind `pensieve-ingest --isolated-negentropy` (off by default); see [negentropy_runtime.md](negentropy_runtime.md).

Library only; no startup scan, scheduler, pruning change or production activation.
Use after the archive recovery gate on the parent-owned bounded database executor.

`SyncStateDb::archived_window` exports an inclusive fixed interval in timestamp/ID
order. It checks durable Archived markers for legacy entries; Pending and missing
IDs are omitted, allowing redundant downloads rather than suppressing recovery.
Both examined and returned rows are capped at 250,000. TooDense returns **no usable
partial set**; the scheduler must split the interval or block/alert at one second.
This set describes known archive inventory, not complete relay history.

The window is read in two passes. The first collects inventory keys only and
returns TooDense as soon as the 250,001st row is seen, so a dense window costs one
bounded key scan (at most about 10 MB of keys, about 20 MB peak while the
filtered copy is built) and no dedupe lookups. Only a window within
the cap pays one dedupe `get` per row in the second pass. Before 2026-10-14 the
lookups were interleaved with the scan, so each TooDense answer was reached only
after 250,000 wasted lookups, repeated for each split child (see
[continuous readiness](negentropy_continuous_readiness.md#incident-2026-10-14-preflight-stalled-the-owner-thread)).
`reconciliation_window` likewise checks the policy exclusion count (SQLite) before
the inventory's lookups.

`InventoryReplay::begin` checks its canonical archive directory, prefix and exact
dedupe object against the supplied writer before initializing any cursor. A writer
without its own dedupe authority cannot authorize replay. It then binds that
source and an explicit rollout floor to a versioned cursor in the existing sync RocksDB. There is no
default history floor. Later configuration changes fail closed. Only one replay
reader can be active per database handle. It opens exactly the next plain/gzip
sealed segment, never an `.open` file. Constant-memory directory discovery reports
a missing predecessor instead of skipping to a later sealed segment.

Each `step` processes at most 256 frames (or a smaller caller budget), with one
16 MiB-capped payload and a bounded ID/timestamp batch. Canonical decoding validates
IDs/signatures; every ID must also have an Archived marker. Invalid/truncated data,
index/IO errors or recovery state poison the reader; drop and retry from the last
completed segment. Gzip decoding checks all members and the stream trailer. Decoded
object overhead is not claimed to equal the wire-byte limit. There is no hard
wall-clock deadline on a blocking disk call.

Replay takes a short `try_lock` snapshot of the writer's admission gate and
exclusive next-segment cutoff. A busy writer returns `None` without opening source
files, scanning the archive, or initializing/advancing the cursor; the caller retries
later. This includes the interval after rename but before durable Archived markers
and checkpoint removal. A successful snapshot only permits segments below its
cutoff. The gate is released before replay opens/decodes any segment; replay never
holds up live archive admission while streaming. Startup obtains the cutoff from
existing segment names only after recovery preflight rejects checkpoints/open files.
That cutoff is readiness, not integrity proof: missing earlier files, invalid data
and missing Archived markers still fail closed and preserve the cursor. It never
forgives a persistent missing marker as a transient seal race. As with the writer,
this assumes no second archive writer or external mutation bypasses its gate.

The inactive `ReplayCursorSnapshot`/`rewind_inventory_cursor` library permits an
explicit metadata-only rewind within the existing namespace, preserving inventory.
It excludes active readers and checks exact observed bytes. Lowering the rollout
floor also requires explicitly adjusting the caller's configured floor; mismatches
still fail closed. Initialization beyond the ready archive boundary is rejected.
Namespace/path repair and an operator CLI remain unimplemented; do not delete the
inventory database to change identity. Replay retains all archive-authority checks.

Valid partial batches may enter inventory before segment completion. They are
safe, incomplete inventory, not coverage. On EOF the inventory WAL is synchronized
**before** a synchronous cursor update. Crash before that update repeats the same
segment idempotently; no partial byte offset is trusted. Dropping a reader never
advances its cursor. The reserved 41-byte metadata key sorts beyond all event
timestamp/ID keys and cannot be erased by the legacy timestamp pruner.

The later scheduler must disable the legacy pruner/seed loop, retain inventory and
source segments needed by unfinished work, provide fair bounded executor turns and
pause on archive/disk faults. A vanished relay event remains an unresolved receipt;
do not erase it to make a job complete. Backoff and persistent unhealthy alerts are
required, with operator recovery when the relay no longer supplies it. No SDK fork,
second archive, historical rebuild or production database migration occurs here.

Tests cover bounded export/overflow, unverified legacy IDs, inclusive boundaries,
partial replay/reopen, synchronized completion/reopen, concurrent reader rejection,
changed-floor refusal, missing predecessor, gzip input, malformed/oversized/truncated
frames, source/dedupe mismatch, the deterministic rename-before-marker race, and
missing archive markers after a completed seal. These are local-storage tests, not Linux worker
isolation or power-loss certification.
