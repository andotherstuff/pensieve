# Isolated worker: durable failure diagnostics

This inactive slice preserves bounded failure information before scheduler policy
is introduced. It launches no workers and changes no production configuration.

After a reconciliation failure, the worker drains its already-captured candidates
through the normal admission ACK path, then sends one terminal `AttemptFailed`
instead of `ProtocolDone`. A broken IPC connection, cancellation or deadline can
prevent the report from arriving; received obligations still remain in the ledger.
Neither a report nor an ACK proves archive durability or completes a job.

The report has a fixed failure class, an outstanding-ID count, and at most 128
sorted unique IDs. It includes no relay-provided free-form error text. On the
first failed fetch batch, the outstanding count/sample can include later IDs not
yet requested. It is not proof of permanent absence, not a complete missing-ID
list, and not permission to skip or expire events. Retries retain the whole gap.

The parent checks the active lease, exact next upload sequence, diagnostic bounds,
and ledger budget before recording the report in the same transactional authority
as event receipts. A report is terminal for that attempt, including after reconnect
or reopen: additional receipts and protocol completion are rejected. Retry policy
is deliberately separate; this slice does not split windows or mark failures done.
Worker Volume hints require supervised process binding and verified byte-budget
semantics before any future split policy can use them.

Ledger schema 5 retains `failure_reports`, keyed by job and attempt, alongside the
bounded maintenance cursor added after the schema-4 diagnostic slice. This is an
undeployed prototype: earlier schema versions are rejected without mutation, not
migrated automatically. Existing schema-5 ledgers open even when over the admission
ceiling so lease expiry, retry and receipt recovery remain available.
Diagnostic history survives retries and reopen; no retention deletion is added.
The existing total ledger budget bounds growth and rejects new writes when full.
The new wire variant fails closed on an older parent, so a future deployment must
install matching parent/worker binaries before activation. This is not an online
mixed-version rollout protocol.

There is one terminal failure message, `AttemptFailed`, and every accepted failure
persists the same attempt fence. There is no legacy unfenced failure path.

Tests cover report validation, old-schema rejection, stale leases, repeated/wrong-sequence
reports, transactional failure, budget refusal, receipt preservation, retry/reopen,
and a partially fetched relay batch that reports failure without false completion.
An explicit zero-receipt failure/reopen regression rejects a fresh session's empty
ProtocolDone, proving the failure fence independently of prior receipt counts.
