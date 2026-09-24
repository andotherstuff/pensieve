# Isolated reconciliation worker (increment 3a)

`pensieve-negentropy-worker --socket PATH --parent-uid UID` runs at most one
assignment, then exits. This increment does **not** add the ingester listener,
scheduler, systemd unit or production activation. Do not enable it operationally
until the parent and Linux cgroup/permission gates pass. The existing SDK is
unchanged; no SDK fork, archive scan or analytics work is involved.

## Wire and process contract

The worker checks the connected Unix peer UID before sending Hello. The future
parent must independently check the worker UID and enforce directory/socket
permissions before exporting a lease. Socket path possession or a JSON token alone
is not peer authentication. There is no TCP listener or database/archive open in
the worker; linking the ingest library is not a filesystem permission boundary.
The Linux service must deny data/secret access and use a sibling limited cgroup.

All traffic is length-prefixed JSON, capped at 1 MiB before allocation. The parent
sends Job (upload identity at sequence zero), strictly ordered InventoryChunk
messages (at most 256 ID/timestamp pairs each), then InventoryEnd with exact count
and digest. Parent inventory sequence starts at one; worker upload sequence starts
independently at one. At most 250,000 inventory records; reject duplicates,
out-of-window timestamps, incomplete framing, unknown fields and wrong identities.
The inventory digest is SHA256 of `pensieve-negentropy-inventory-v1` plus NUL,
followed by each big-endian u64 timestamp and raw 32-byte ID. Input must already be
in timestamp/ID order. The JSON frame cap also bounds decoding a malformed chunk
before its row-count check; it is not an allocator-byte claim.

One relay and inclusive window (at most 900 seconds) come from the authenticated
parent. Reject URL credentials/query/fragment to avoid logging secrets. The parent
owns allowlisting and outbound URL policy; localhost is used only by test fixtures.
The worker uses the pinned SDK's single-relay transport, but owns a small
download-only NIP-77 loop using the same pinned negentropy 0.5.0 primitive. It does
not call SDK `sync_with_items`: that convenience loop can report success after
notification lag/closure without completing the diff. Our one notification reader
turns either condition into an error; only explicit diff completion followed by
fully fetched 128-ID batches can construct download proof. Remote missing IDs are
capped at 50,000. There is no pool-wide `join_all`, separate support-check subscriber,
or worker API for reading RocksDB inventory. Repeated inventory IDs are rejected
even at different timestamps.

The worker disables SDK tag-count filters to match archive validation while
retaining a 5 MiB relay JSON message cap. Its own 60 KB outgoing negentropy target
does not constrain relay replies: incoming hex replies may use the message cap.
IPC frames still have the independent 1 MiB cap. These are wire bounds, not total
allocator memory guarantees.

The SDK callback validates and serializes into a capped event frame, then waits on
bounded byte credit/queue space. There are at most 15 queued wire frames plus one
in-flight frame, capped to 8 MiB of credited wire bytes; one callback being encoded
can hold an additional capped frame and the SDK event representation. This is not
a bound on all SDK decoder or allocator memory. Total output remains 50,000
frames / 64 MiB per attempt. Callbacks serialize through an async mutex; cancellation
or failure leaves the attempt poisoned. There is no unbounded collector vector.

The worker releases frame credit only after a matching Accepted echo, meaning
parent admission ownership, not archival. After sync, disconnect is requested and
capture is explicitly closed under its callback lock. Completion requires every
remote missing ID to appear in both the worker's fetched IDs and captured IDs, no callback
failure, and every captured frame to receive its ACK. Only then send ProtocolDone
with exact count/digest. EOSE, empty socket output and SDK success alone do not
qualify. Unsolicited frames, cancellation, EOF and wrong ACKs fail closed. Failures
exit nonzero without ProtocolDone; parent retains receipts and applies retry policy.

Exit 2 specifically means EOSE arrived without all advertised IDs (`Unavailable`).
It does not distinguish relay withholding from SDK-expired NIP-40 events; it is
neither proof of permanent absence nor proof of a volume limit. Never split/skip/
complete solely from exit 2. Verified local count/byte exhaustion exits 3; a verified
individual event exceeding the IPC cap exits 4. Capture preserves its first local
rejection even if the SDK suppresses Event notification and EOSE later reports
Unavailable. Signature/window checks precede classification; a single oversized
event is checked before aggregate budgets because splitting cannot fix it.
Cancellation/invalid candidates remain generic failures, never inferred volume.
All other failures exit 1. Unknown exits/signals are not volume signals either.
These exit codes require parent attempt/process binding before use in split policy;
they do not replace durable receipt reconciliation. Relay-advertised diff-count
overflow is not verified event volume and still fails generically. A matching
CLOSED during the diff now fails immediately instead of waiting for idle expiry.
The worker cannot override the pinned SDK's unconditional expiry filtering. Before
3b runtime activation, the scheduler must retain these gaps, use paced bounded
retries/backoff and surface persistent failures for operator action while continuing
healthy jobs. Explicit size failures must be distinguished from unknown loss before
enabling volume splitting. No automatic expiry exception or skipped-ID success is
authorized here. A window containing an undeliverable event can remain unfinished.

## Deadlines and limitations

A nine-minute deadline wraps Unix connect, assignment, relay connect/sync,
disconnect and output drain. Lease expiry may shorten it. Inventory has a two-minute
exchange limit; during relay work two minutes without an admitted-event ACK ends
the worker. Arbitrary SDK chatter does not reset progress. Socket absence fails
this process; the future service supplies paced restart, not a busy reconnect loop.

The binary exits without waiting for SDK background-task destruction on either
result. It owns no durable writes; OS process exit closes remaining sockets. Async
timeouts cannot interrupt non-yielding SDK code or prevent all decoder OOM. The
separate systemd runtime/memory/CPU limits remain mandatory and untested here.
Logs allow only the worker's fixed phase/error messages, not SDK payload diagnostics.

Tests use a localhost NIP-77 relay and the real pinned SDK, plus the existing
parent receipt/admission library. They cover successful durable publication only
after seal, partial fetch despite EOSE, hanging relay, parent disconnect, stalled
inventory, wrong parent UID, lost/wrong ACK with retained receipts, bounded queue
backpressure/cancellation, event/byte limits, ordering and digest/truncation errors.
Explicit regression tests reject lagged/closed notification receivers and repeated
IDs at different timestamps; a real empty diff still completes successfully.
The relay fixture also exercises overlapping inventory with multiple diff rounds,
multiple 128-ID fetch batches, replies above 120,000 hex characters, a 2,001-tag
event and an advertised expired event that remains unavailable rather than complete.
Additional regressions cover first-cause preservation through missing notifications,
attempt budgets versus individual event limits, invalid events never becoming volume
signals, stable exit codes, and prompt failure on a CLOSED diff.
They do not substitute for Linux OOM/kill/no-orphan or throughput tests.

Next: parent-owned bounded executor and authenticated endpoint, fair lazy planning
and persisted backoff, periodic receipt recovery during admission pauses, inventory
seal readiness/source binding/operator cursor repair, then isolation/alert canary.
