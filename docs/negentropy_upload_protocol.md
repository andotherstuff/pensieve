# Isolated reconciliation: bounded upload and received receipts (increment 1b)

Builds on the merged job-ledger foundation from PR #45. This is library code and temporary
database tests only: **no listener, worker executable, runtime wiring, service
changes, or production migration**. It does not replace the existing reconciler.
The stacked [archive consumer increment](negentropy_archive_receipts.md) adds
`admit_and_accept`, durable completion, and bounded receipt compaction.

## Implemented boundary

`sync/ipc.rs` models the worker-to-parent upload after an authenticated worker has
received its job and inventory. Authentication and that preceding exchange are
not implemented here. Possession of a header is not proof of Unix peer identity.

- Four-byte big-endian payload length, then UTF-8 JSON. Reject zero or lengths
  over 1 MiB before allocating/reading payload. Encoding also uses a capped
  writer. Unknown message types/fields, versions, malformed JSON and truncated
  reads fail closed. Frame/receipt values containing tokens deliberately lack
  Debug implementations; do not log serialized messages.
- Event, ProtocolDone and AttemptFailed messages carry version, job, attempt,
  unpredictable lease capability and sequence. Upload sequence starts at one
  and advances by exactly one. Error poisons the session. EOF is not success.
- Events are independently signature/ID verified and must be within the leased
  inclusive timestamp interval. The persisted lease is checked before receipt
  mutation and acknowledgement, not just copied once at session startup.
- At most 16 unacknowledged event frames / 8 MiB of framed bytes. At most 50,000
  event frames / 64 MiB per attempt. Duplicate event IDs still consume frame and
  byte budgets. These are wire/row limits, not a bound on Rust allocator overhead.
- A received record commits with FULL SQLite durability **before** the caller
  receives the candidate for archive admission. `admit_and_accept` establishes
  admission ownership before an ordered, single-use Accepted acknowledgement.
  Returned credit does not reset total attempt budgets. The Accepted value is a
  typed result, not yet a complete parent-to-worker transport implementation.

All APIs are synchronous. Future runtime integration must enforce lifecycle and
idle deadlines, one authenticated connection per lease, a bounded blocking executor
and bounded queues. `read_receive` samples its supplied clock after decoding, not
before a potentially blocked read, and poisons the session on framing failure.
It cannot interrupt a stuck reader itself. Reconnection retries a fresh attempt;
it does not resume a partially received stream or reset its budget.

## Received is not archived

The initial schema includes `attempts`, `receipts`, and a transactional retained-row
counter. There is no speculative migration for the undeployed #45 prototype;
incompatible prototype databases fail closed without modification. No event payloads are stored. Each receipt
records job/attempt/sequence, event ID, timestamp, framed byte count and frame hash.
Attempt summaries store count, bytes, digest and whether ProtocolDone matched.

The global retained-receipt limit defaults to 100,000 rows, in addition to existing
job and SQLite/WAL byte admission ceilings. The archive consumer compacts only
confirmed receipts into durable attempt summaries; missing IDs are retained.
Exhaustion pauses admission, not recovery; no unresolved receipts are discarded.
All receipt replay is rejected: errors poison the session and require a fresh
attempt, including an uncertain commit outcome. A single transaction performs the
persisted sequence check, receipt insertion, summary and counter update; there is
no per-event scan of retained receipt rows. Network duplicates cannot release
credit twice. Per-event FULL commits remain an unbenchmarked cost; batching and
throughput acceptance belong with the archive consumer before deployment.

Let D0 be SHA256 of ASCII `pensieve-negentropy-upload-v1` followed by one NUL
byte. For each Event frame, `F = SHA256(big-endian length || exact JSON bytes)`;
then `Dnext = SHA256(Dprevious || F)`. The worker hashes the exact encoded bytes,
not a reserialization. ProtocolDone must match the ordered count/digest and cannot
arrive with outstanding acknowledgements. Empty success explicitly records count
zero and D0. Failed attempts and EOF never record protocol success.

**ProtocolDone does not complete a job**, even with zero events. It moves the job
to awaiting durability and fences the upload capability. Only the archive consumer
can complete it. A crash after receipt commit but before archive admission leaves
an unresolved obligation, not a claim that data was recovered.

## Tests and remaining integration gates

Tests exercise prefix-only oversize rejection, malformed/truncated input, invalid
signatures/windows/identities, repeated and out-of-order messages, credit return
and exhaustion, total attempt limits, failed/empty/mismatched summaries, stale
acknowledgements, retained receipts across reopen/retry, internal replay rejection,
incompatible schema rejection, transaction rollback, and global receipt/byte admission rejection.
The abrupt-process-exit regression now also retains a committed receipt while
discarding uncommitted changes. Seeded counters/rows exercise large boundaries
without claiming a 50,000-event throughput or full-disk production test.

The archive consumer adds satisfaction from durable markers and completion.
Independent seal cadence and bounded sealed inventory replay remain. The complete authenticated socket
protocol must also add Hello, assignment, inventory chunks/end, cancellation and
parent response framing before a real worker can run. No live worker, success
metric, listener or deployment should be enabled on this intermediate library.
