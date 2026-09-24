# Parent maintenance commands (inactive library)

The existing dedicated parent owner now exposes explicit expire, same-window retry,
durability retry and archive maintenance commands. These are mechanisms, not retry
policy or a scheduler. There is no listener, process launch, main wiring or deployment.
Single-attempt progress and bounded failure-report read commands use the same
owner, so an uncertain result can be investigated without reopening the ledger.

One maintenance turn selects at most 32 unresolved jobs using an explicitly selected
partial-index keyset (`INDEXED BY unresolved_jobs`; bundled SQLite query-plan regression) and
checks at most 256 receipts **total**, not 256 per job. The cursor is persisted in
the receipt_totals singleton. All unresolved non-live states are visited, including
retry_wait, split and blocked receipt holders, and awaiting jobs with zero receipts.
Completed, queued and currently leased jobs are excluded by the index and query;
they consume no recovery turns or per-job writes. A known end of the selected
keyspace resets the cursor in the same turn; callers continue after zero changes.
Existing per-job completion propagation retains its separate 63-level split-tree
bound; those ancestor checks do not count as newly selected jobs or receipt reads.

Each successfully handled job advances the local cursor, persisted once at turn
end or on error. Reconciliation and cursor update are separate durable commits:
a crash between them safely repeats at most one bounded turn rather than
skipping a receipt. An error does not advance past the failed job; earlier successful
reconciliation remains committed. Within each job the existing receipt keyset cursor
prevents one missing old event from starving later receipts. Across jobs the new
cursor prevents that incomplete job from monopolizing recovery.

Recovery and lease release bypass admission byte/row ceilings, never actual disk
errors. A received event without ProtocolDone remains an unresolved gap even after
its receipt is archived and compacted. Maintenance never promotes such failures to
success. Stale attempts cannot trigger retry_durability. Dropping a response does not
undo a committed command; callers must reread durable state after cancellation.
In particular, external cancellation can race with an owner that already committed
ProtocolDone or a failure report. A lost result alone is not a retry classification:
read the job/attempt/report first, then choose the state-appropriate recovery.
The session awaits its owner, which alone enforces the absolute deadline and
preserves terminal results; external future cancellation remains ambiguous. Archive recovery
and invalid request errors now have distinct types and must not become relay blame.
The maintenance owner checks the writer recovery latch before and after the turn
and returns `RecoveryRequired`, including a latch raised by concurrent live admission.
Earlier committed per-job recovery remains committed; a recovery error does not
mean the entire turn rolled back. Durable state must be reread before retry decisions.

The owner remains monopolized by one session for up to nine minutes plus any
uninterruptible disk operation. Maintenance runs **between exchanges**, not in
parallel with an upload. Scheduling that alternates recovery turns with admissions
and guarantees freshness remains a future slice.

Future scheduling must budget recovery cadence against unresolved work, not completed
history. Drain sufficient bounded recovery turns between admissions.
The explicit `now` arguments are caller timestamps, not execution-time clock reads.
A command queued behind an externally cancelled session or slow disk operation can
use a stale timestamp and shorten its effective retry delay. Before activation, the
scheduler must arrange execution-time deadline calculation rather than treating a
queued caller timestamp as a guaranteed minimum backoff from command completion.

This changes the unshipped prototype ledger to schema 5 (schema 4 failure reports
plus the recovery cursor). Older prototype versions fail closed without mutation;
there is no automatic migration or instruction to delete obligations. Preserve an
older ledger and stop for an explicit recovery plan if one contains real work.
