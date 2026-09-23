# Isolated reconciliation: bounded archive inventory (increment 2c)

Library only; no startup scan, scheduler, pruning change or production activation.
Use after the archive recovery gate on the parent-owned bounded database executor.

`SyncStateDb::archived_window` exports an inclusive fixed interval in timestamp/ID
order. It checks durable Archived markers for legacy entries; Pending and missing
IDs are omitted, allowing redundant downloads rather than suppressing recovery.
Both examined and returned rows are capped at 250,000. TooDense returns **no usable
partial set**; the scheduler must split the interval or block/alert at one second.
This set describes known archive inventory, not complete relay history.

`InventoryReplay::begin` binds a canonical archive directory, prefix and explicit
rollout floor to a versioned cursor in the existing sync RocksDB. There is no
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

Runtime integration must distinguish a seal still in progress from corruption:
the final filename is visible before the writer finishes marking IDs Archived.
A missing marker leaves the cursor unchanged; it is not permission to skip the
segment. Before enabling replay, bind its directory/prefix to the actual writer
and add a retryable seal-in-progress outcome. Persistent missing markers after
sealing completes must remain an actionable fault, not an infinite silent retry.
Cursor identity changes also require an explicit operator repair path before
activation; do not delete the inventory database to fix a mistyped floor/path.

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
frames and missing archive markers. These are local-storage tests, not Linux worker
isolation or power-loss certification.
