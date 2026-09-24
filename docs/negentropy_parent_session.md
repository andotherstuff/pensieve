# Isolated worker: parent session executor

This is an inactive library boundary, not an ingester endpoint or scheduler. It
opens no listener, launches no process, changes no service and retries no work.

`sync::parent::ParentExecutor` owns one dedicated blocking thread and the sole
`JobLedger` connection. A capacity-one command queue and exclusive mutable methods
bound submissions; there is no per-event `spawn_blocking` task. Inventory reads,
receipt registration, event validation and shared archive admission run on that
thread, never an async reactor. The caller supplies the ingester's existing shared
inventory, writer and exact writer-owned dedupe handles after startup recovery.
The constructor checks pointer identity against the writer's own dedupe authority
and rejects a mismatch or a writer without dedupe before starting its thread.
There is no new archive or second RocksDB open.

The parent checks Unix peer UID before handing the socket to the executor or
exporting any lease capability. It reads a capped greeting, checks the active
lease, obtains complete bounded archived inventory internally, then sends the
assignment and 256-record chunks with the canonical inventory digest. A caller
cannot supply a raw vector and call it archive-confirmed. `TooDense` sends no
partial inventory and changes no job state. Socket ownership belongs to this
single exchange; reconnecting the same active attempt is rejected within the
executor lifetime.

The existing upload session persists each received obligation before shared
archive admission. Only its typed admission result produces an ACK. ACK still
does not mean archive durability. A valid ProtocolDone produces the explicit
`SessionOutcome::ProtocolDone`, leaving the ledger awaiting archive reconciliation.
The single `AttemptFailed` message persists its bounded diagnostic and terminal
attempt fence in ledger schema 4 before returning `SessionOutcome::Failed(FailureKind)`.
Failure reports are returned without automatic retry, split or completion.

The maximum exchange deadline is nine minutes, including queue wait, inventory
and all socket traffic; the lease wall-clock expiry is checked independently.
Blocking socket reads/writes poll at most every 100 milliseconds and check the
same absolute deadline, so a partial frame cannot indefinitely reset the timeout.
Socket timeouts are configured only once, before the greeting: macOS rejects
timeout changes after peer exit even when a terminal frame remains buffered.
The async deadline still closes the socket immediately. Dropping/timing out the
async session shuts down both directions and sets cancellation before another
operation begins. When its own deadline fires, `serve` retains and awaits the owner
reply: an already-committed ProtocolDone or terminal failure is returned instead
of being hidden by a timeout. This can extend the await beyond the socket deadline.
An externally dropped future cannot deliver a reply; its caller must re-read the
durable job/attempt state before deciding recovery, never assume a timeout means
the terminal transaction did not commit. An already-running disk operation cannot be aborted: it may
finish registration/admission after cancellation, but its receipt remains and
cannot cause a false completion. The queue stays bounded while that operation
finishes. `shutdown` asynchronously waits for the owner and returns the ledger;
dropping an executor closes its queue but is not a substitute for joining during
orderly shutdown. No claim of a hard disk-I/O deadline is made.
Without an async cancellation wakeup (for example a stalled reactor), a single
socket syscall may outlast the remaining deadline by up to the 100ms polling
quantum plus OS scheduling delay. No new database operation starts afterward.

## Remaining integration requirements

- Create a restricted Unix socket and dedicated worker UID; authenticate the
  supervised process as well as its UID before interpreting process exit codes.
- Recover/expire an old active lease before creating a replacement executor after
  restart. The in-memory one-session fence is not a persistent restart fence.
- Apply fair, paced retry policy to persisted failure classifications/diagnostics.
  The library does not automatically split even a Volume report.
- Keep bounded receipt reconciliation running while admissions are paused. It is
  available after executor shutdown today; the future long-lived scheduler must
  add bounded recovery commands/turns instead of opening another ledger.
- Bind replay source identity to the writer, distinguish sealing-in-progress from
  corruption, and implement non-destructive guarded cursor repair before replay
  activation. Disable the old sync/seed/prune loop in the new mode.
- Linux sibling-cgroup isolation, kill/OOM tests, real alerts and operator canary
  decisions remain separate gates. No production configuration is enabled here.

Local tests cover wrong UID, bounded stalled framing, dropped futures, malformed
greetings, stale sequence, authenticated failures, refused reconnects, ACK admission,
EOF receipts, and protocol-vs-archive completion using a real ledger/writer/index.
They also cover committed ProtocolDone racing the session deadline and a 300-ID
multi-chunk inventory decoded by the real worker parser. Local invalid requests
and archive recovery faults have distinct error types rather than worker blame.
