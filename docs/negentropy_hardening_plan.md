# Negentropy hardening implementation

Authorized 2026-09-23. Branch: `codex/negentropy-hardening`.
See `negentropy_audit_20260923.md` for the original findings and limitations.

## Gates and progress

- [x] Integrity patch: compatible Nostr security floors, independent validation
  before discovery/admission, owned pending claims and regression tests.
  Full `just precommit` passed, including admission and writer-failure regressions.
  No deployment. Disposable-Postgres integration tests remain ignored by default.
- [x] Writer failure gate: distinguish definite pre-admission failure from partial
  frame I/O and post-admission seal failure. Do not blindly release ambiguous
  claims or seal a malformed `.open` file. Preserve existing bytes. Recovery is
  explicitly operator-driven; no automatic repair is implemented. Prometheus
  syntax validation passed for both new rules; deployment and notification routing
  are not yet verified.
- [ ] Bounded independent relay workers: full-lifecycle deadline, idle deadline,
  structured child ownership, bounded streaming admission, per-relay results.
- [ ] Durable inventory: sealed-receipt updates, bounded scans/windows and
  resumable coverage. Never equate pending with durable.
- [ ] Operations: truthful metrics, supervisor, alerts, persisted relay backoff,
  explicit allowlist that does not silently expand through the catalog.
- [ ] Canary: controlled deployment only after tests and `just precommit` pass;
  several completed cycles with healthy live sealing/Parquet/ClickHouse indexing.

## Current integrity patch details

- Locked `nostr` 0.44.7 and `nostr-relay-pool` 0.44.3; SDK remains 0.44.1.
- Independent ID/signature verification in live relay source before discovery,
  and in negentropy adapter before queueing. Do not triple-verify again in main.
- Closed negentropy admission channel returns an error rather than success.
- Owned dedupe reservations release on packing/open failure or unwind; duplicates
  cannot release another worker's reservation. Fail closed on dedupe DB errors.
- Writer takes ownership immediately before frame I/O; ambiguous write/seal
  failures deliberately retain ownership and latch the writer-wide recovery gate.
- Frame length conversion is checked rather than truncating to u32.
- Regression tests cover forged repeat delivery followed by a valid event,
  forged signature, closed receiver, duplicate reservation, unwind, ownership
  transfer, and segment-open failure.

The updated lockfile audit has 21 remaining vulnerability matches outside Nostr,
down from 32. This is not a clean whole-workspace security audit. TLS/AWS transport,
bytes/lz4/time and other findings remain tracked in the original report.

## Verification environment

Use `CARGO_TARGET_DIR=/Volumes/Worktrees/pensieve-negentropy-target` and
`CARGO_BUILD_JOBS=4` to keep native build outputs off the nearly full system disk.
The worktree volume had approximately 787 GiB free at start. Full workspace gates
compile bundled DuckDB and RocksDB. Do not build on or restart production to
work around local verification failures.

## Required failure tests before rollout

### Approved writer failure policy

The user confirmed fail-closed admission on 2026-09-23. The implementation now
serializes archive mutations, latches errors/panics, persists a seal checkpoint,
refuses startup with interrupted files/markers, and supplies alert rules plus
`docs/archive_failure_recovery.md`. Automatic repair remains intentionally absent;
recovery requires validation and reconciliation before the operator clears the gate.

Inspection found that `SegmentWriter::write` can fail after writing a length or
part of a payload; current handlers continue, and later `seal` does not validate
framing. Clearing that event's pending claim alone is not safe. A seal can also
fail after taking the current segment out of the writer, leaving other pending
claims without a live owner. This needs a writer-wide failure state and explicit
recovery, not merely per-event retry.

Recommended policy: fail archive admission closed on uncertain frame/seal I/O,
preserve `.open`/sealed evidence and pending ownership, alert, and resume only
after deterministic recovery. This can stop ingestion during a disk fault;
this availability tradeoff is approved. Do not
silently roll forward through possibly malformed archive bytes.

1. Silent relay plus healthy relay: healthy events reach archive promptly.
2. Hung connect, reconcile, disconnect and collector: all bounded; no orphan tasks.
3. Parent cancellation/panic: child work stops, gauges clear, next cycle runs.
4. Full admission queue, oversized events and huge item sets: memory remains capped;
   output records partial coverage, never success for discarded data.
5. Pre-write, partial-frame, flush and seal failure: retained bytes remain recoverable;
   definitely unadmitted events can retry without suppressing another owner's claim.
6. Restart at each durable-inventory boundary: no false local inventory entries.
7. Outage beyond rolling lookback: incomplete coverage remains visible and retryable.
8. Missing allowlist, cooldown and half-open probes: live ingestion remains independent.

## Production observations during implementation

At 12:22 UTC the user-restarted process remained active, NRestarts=0, with one
finished negentropy cycle and in-progress=0. Counters reported 87,553 events
written, 297,195 received in the last batch, and zero handler failures. These old
metrics do not prove all-relay success or durable admission of every event.
Space remained approximately 597 GiB archive / 335 GiB data. No production changes
were made by this implementation task.
