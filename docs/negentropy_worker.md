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
The worker runs the pinned SDK's **single-relay** `sync_with_items`, not pool-wide
`join_all`. It has no API for reading RocksDB inventory itself.

The SDK callback validates and serializes into a capped event frame, then waits on
bounded byte credit/queue space. There are at most 15 queued wire frames plus one
in-flight frame, capped to 8 MiB of credited wire bytes; one callback being encoded
can hold an additional capped frame and the SDK event representation. This is not
a bound on the SDK's remote sets or allocator memory. Total output remains 50,000
frames / 64 MiB per attempt. Callbacks serialize through an async mutex; cancellation
or failure leaves the attempt poisoned. There is no unbounded collector vector.

The worker releases frame credit only after a matching Accepted echo, meaning
parent admission ownership, not archival. After sync, disconnect is requested and
capture is explicitly closed under its callback lock. Completion requires every
remote missing ID to appear in both SDK received IDs and captured IDs, no callback
failure, and every captured frame to receive its ACK. Only then send ProtocolDone
with exact count/digest. EOSE, empty socket output and SDK success alone do not
qualify. Unsolicited frames, cancellation, EOF and wrong ACKs fail closed. Failures
exit nonzero without ProtocolDone; parent retains receipts and applies retry policy.

## Deadlines and limitations

A nine-minute deadline wraps Unix connect, assignment, relay connect/sync,
disconnect and output drain. Lease expiry may shorten it. Inventory has a two-minute
exchange limit; during relay work two minutes without an admitted-event ACK ends
the worker. Arbitrary SDK chatter does not reset progress. Socket absence fails
this process; the future service supplies paced restart, not a busy reconnect loop.

The binary exits without waiting for SDK background-task destruction on either
result. It owns no durable writes; OS process exit closes remaining sockets. Async
timeouts cannot interrupt non-yielding SDK code or prevent remote-set OOM. The
separate systemd runtime/memory/CPU limits remain mandatory and untested here.
Logs allow only the worker's fixed phase/error messages, not SDK payload diagnostics.

Tests use a localhost NIP-77 relay and the real pinned SDK, plus the existing
parent receipt/admission library. They cover successful durable publication only
after seal, partial fetch despite EOSE, hanging relay, parent disconnect, stalled
inventory, wrong parent UID, lost/wrong ACK with retained receipts, bounded queue
backpressure/cancellation, event/byte limits, ordering and digest/truncation errors.
They do not substitute for Linux OOM/kill/no-orphan or throughput tests.

Next: parent-owned bounded executor and authenticated endpoint, fair lazy planning
and persisted backoff, periodic receipt recovery during admission pauses, inventory
seal readiness/source binding/operator cursor repair, then isolation/alert canary.
