# Negentropy reliability and dependency audit — 2026-09-23

## Scope and evidence

Read-only production inspection and static code/dependency audit. No production changes, restarts, upgrades, lockfile changes, or data deletion. This document is an audit deliverable, not implementation approval.

Local source HEAD: bd78416c9124be617066fe1e1c4465dfd45fb7a8.
Server checkout HEAD: 557d241c0e7e42663db59d391da335f79f70e222.
The server and local Cargo.lock and sync/negentropy.rs match exactly:
- Cargo.lock SHA-256: ee12e75a4b22299b3d6db283db1d4e709480bd6387e920058d8f55ee31ce067e
- negentropy.rs SHA-256: 4fc3a35f4ef7f234e0a4caf3e1d6c80428f8aa298e4fbe7f52583ad04c89b2bc

The running executable is /home/pensieve/pensieve/target/release/pensieve-ingest. Matching checkout files do not independently establish the binary's build provenance.

Following the user's restart, ingestion became active at 11:58:06 UTC. Negentropy's receiving counter advanced from 3,600 to 177,300 during inspection. No complete new cycle was observed. NRestarts=0 is systemd automatic-restart accounting, not evidence that the manual restart did not occur.

Final check at 12:04:58 UTC: receiving reached 263,100, still in progress, no completed cycle yet. Fresh segment 24424 sealed at 12:03:10 with 29,903 events and ClickHouse indexed exactly 29,903 at 12:03:32. Live archival/indexing therefore continued during reconciliation; receipt in negentropy itself still does not establish archival admission.

Disk: /archive 597 GiB available (90% used); /data 335 GiB available (91% used). Available RAM approximately 97 GiB; swap approximately 13 GiB used. ClickHouse indexed segment 24423 with 24,396 events at startup. Parquet startup replay is traversing 16,721 sealed segments from segment 7703; sampled replay publications have matching rows and zero rejects. These are replay results, not proof of post-restart fresh segment sealing or complete recovery.

## Executive conclusion

The code permits indefinite reconciliation lifecycle hangs and obscures partial failures. A restart restores activity, not correctness. Fix task ownership, streaming/backpressure, and durable admission together with security patching. An SDK upgrade alone cannot repair application-owned queues, metrics, or dedupe claims.

The historical hang's exact relay and phase cannot be recovered from existing INFO logs. Collector shutdown is a strong suspect because it awaits channel closure without a deadline and relies on asynchronous SDK teardown, but it is not a proven root cause. Do not describe a specific relay as responsible without new evidence.

## Findings

### P1 — Lifecycle deadline does not bound lifecycle

crates/pensieve-ingest/src/sync/negentropy.rs:373-420 wraps only client.sync() in the 900-second timeout. Connect, disconnect, client teardown and collector join are outside it. Dropping client invokes asynchronous SDK cleanup; disconnect signals relays rather than awaiting destruction of every database/channel owner.

The old SDK's relay sync loop waits on notifications without a post-handshake idle deadline. Application timeout can stop that future, but does not prove that its background tasks and collector have stopped.

Required: explicit task ownership, per-phase diagnostics, one absolute per-relay deadline including cleanup, bounded cleanup reserve, cancellation followed by abort-and-await when cooperative shutdown fails. Never rely exclusively on eventual sender drop to terminate a collector.

### P1 — All relays share one completion barrier

Locked nostr-relay-pool 0.44.0 pool/mod.rs:1038-1109 clones the local item vector per target, waits with join_all, then reports results. One slow relay delays every healthy relay's result; the outer timeout discards access to the aggregate per-relay result.

Required: independent relay tasks and results, limited concurrency, continuously reap finished tasks, and enforce a whole-cycle scheduling budget. A JoinSet/FuturesUnordered replacement without concurrency and lifetime bounds is insufficient. A relay must not share cancellation ownership with healthy relays.

### P1 — Recovery buffers full events without bounds before archival

negentropy.rs:104-145 and 332-350 use an unbounded channel and an unbounded Vec<Event>. run_periodic processes events only after all sync and collector teardown complete. During a hung cycle received events are only in memory, not in the canonical archive. Restart can discard them; later recovery depends on relay retention and the configured lookback.

The full sync-state vector is also loaded synchronously, copied into another vector, and cloned per relay by the SDK. Protocol result ID sets add further memory. Limiting the event channel alone will not bound reconciliation memory.

Required: validated events stream into a bounded ingestion queue while other relays continue. Bound both bytes and event counts, individual event size, concurrent relays, item sets, SDK result accumulation and time-window sizes. Use bounded blocking work for RocksDB scans/packing; Tokio timeout cannot preempt non-yielding synchronous work. Use adaptive time-window subdivision with an explicit cap/partial result for pathological same-timestamp cardinality.

### P1 — Failed admission leaves a dedupe claim stuck

main.rs:980-1035 claims an ID with check_and_mark_pending before packing/writing. Error exits do not release the claim. pipeline/dedupe.rs:170-184 then suppresses repeat delivery for the lifetime of the process, despite the comment promising next-cycle retry.

Required: an owned admission/reservation guard, committed only once the writer accepts responsibility; release on definite pre-admission failure. Partial writes and failures after acceptance require writer-aware recovery, not blindly deleting the claim. Test pack failure, write failure before bytes, partial write, seal failure and concurrent duplicate delivery.

### P1 — Locked relay dependency has event-integrity vulnerabilities

Cargo.lock contains nostr-relay-pool 0.44.0. RUSTSEC-2026-0224 describes verification cache poisoning; 2026-0232 describes processing unverified events. The inspected old SDK calls its verification-cache check before event.verify(). The negentropy adapter always reports NotExistent, but that does not bypass the vulnerable verification cache.

The local negentropy handler and pack_nostr_event (segment.rs:120) do not independently verify ID and signature before admission. The live relay source also documents reliance on SDK validation. This is a credible ingestion-integrity exposure, not evidence of an exploit or proof of existing archive corruption.

Required: security-patch the relay stack and add an explicit validated-event boundary before claiming dedupe IDs, archival, or relay-discovery side effects. Regression: repeated malformed/forged event followed by its valid counterpart, across multiple relays. Do not silently delete or rebuild existing archives based on an advisory match.

### P2 — Metrics can report failure as success; task death is not supervised

negentropy.rs:390-409 ignores output.success/output.failed and estimates successful relays from add_relay successes. The SDK returns Ok(output) even when individual relays fail. Sync errors/timeouts still lead to Ok((stats, events)), increment syncs_total, update last_sync_unix and later log "sync complete". Those metrics precede event handling and durability confirmation.

Gauges reset only on normal exit (315, 446-447). main.rs retains the task handle but does not monitor its failure during normal operation; shutdown aborts it without awaiting completion (1197-1201). Dropped child JoinHandles detach rather than cancel their tasks.

Required: RAII gauge cleanup (unwind/cancellation safe, not a substitute for process-down alerts), supervised child tasks, separate attempted/partial/failed/succeeded cycle states, per-relay outcomes, last_attempt and last_success, archive-admitted versus durable counters. Sync-state record errors must not increment success. Handle pending admissions explicitly rather than leaving batch totals unexplained.

### P2 — Sync inventory is incomplete and unnecessarily re-downloads known events

Only negentropy's periodic handler updates the sync inventory after observing durable dedupe state; ordinary live segment sealing does not update this index. Newly recovered events still pending at that instant wait for a later cycle to be rediscovered. check_id always returns NotExistent. This is conservative against claiming undurable events, but repeatedly downloads already archived history and does not represent the full local archive.

Cold initialization uses ClickHouse fetch_all and an approximate-item threshold (<100), not a resumable, bounded rebuild from confirmed canonical archive admissions. Trusting a derived index as the inventory can falsely claim local availability if it diverges.

Required: update a rebuildable inventory from durable segment receipts, with a replayable checkpoint. Preserve the existing rule that pending is not durable. Stream rebuilds in bounded batches; do not clear the current dedupe database.

### P2 — No relay-specific failure quarantine or verified allowlist

manager.rs:283 selects catalog targets by fresh monitor quorum, advertised NIP-77, no advertised payment requirement and RTT. This is useful discovery filtering, not evidence that reconciliation completes. No negentropy-specific consecutive-failure feedback is applied.

The live configured negentropy relay file failed to load and the process fell back to defaults. effective_relays then adds catalog targets. A file alone therefore does not create a pinned set.

Required: explicit allowlist mode that disables catalog additions; missing explicitly configured file fails the negentropy subsystem closed with an alert, without stopping healthy live ingestion. Promote discovered relays only after bounded probes. Persist failure streaks and jittered exponential cooldown; half-open probes restore recovered relays. Do not let transient global disk/network errors permanently blacklist every relay.

### P2 — Coverage can silently age out; time semantics need a contract

The rolling filter uses since only. After sufficiently long outages, old gaps age beyond the 14-day lookback. The adapter ignores until and other filter fields; changing to bounded windows without fixing this would reconcile different sets. Future timestamps are not bounded by an until cutoff.

Required: freeze since/until per job, test inclusive boundaries, retain unfinished window coverage and retry it before expiry. Explicitly report partial/unknown coverage; a successful sample relay is not "all history complete". Durable inventory pruning must respect unfinished coverage. This is not authorization for an unbounded historical backfill.

### P2 — Missing lifecycle watchdog and failure-injection coverage

No negentropy alert rules were found under ops/production/prometheus. Current negentropy.rs tests cover config defaults, not hanging network/collector phases. State tests exercise small local DB operations, not end-to-end interruption semantics.

Required alerts: active cycle age >20 minutes; no successful cycle >1 hour after startup grace; dead worker; all relays cooling down; queue saturation; durable-progress starvation. Success should distinguish any relay success from complete configured coverage. Alerts must use new truthful metrics, not current last_sync_unix. Avoid unbounded relay-label cardinality and URL credentials in logs.

## Dependency audit

cargo audit --json inspected 606 locked packages against RustSec DB commit 6477ec04375b913e13f38d966dc49eba9d178cb8, updated 2026-09-23T09:46:24+02:00.

Result: 32 advisory/package-version matches across 11 package names, not 32 confirmed remotely exploitable defects. Also 5 unmaintained, 9 unsound and 5 yanked-package warnings. A lockfile scan includes optional and non-production paths; reachability and actual deployed builds need separate classification.

cargo tree -p pensieve-ingest confirms the old Nostr stack, bytes, both h2 lines, aws-lc-sys, lz4_flex, rustls/webpki and time are in its resolved dependency tree. Old h2 0.3 / webpki 0.101 come through the AWS Smithy/Hyper 0.14 transport stack. Do not try to repair incompatible transitive lines by forcing an arbitrary lockfile version.

| Advisory | Locked package | Issue | Patched requirement from advisory |
|---|---|---|---|
| RUSTSEC-2026-0045 | aws-lc-sys 0.34.0 | Timing Side-Channel in AES-CCM Tag Verification in AWS-LC | >=0.38.0 |
| RUSTSEC-2026-0044 | aws-lc-sys 0.34.0 | AWS-LC X.509 Name Constraints Bypass via Wildcard/Unicode CN | >=0.39.0 |
| RUSTSEC-2026-0048 | aws-lc-sys 0.34.0 | CRL Distribution Point Scope Check Logic Error in AWS-LC | >=0.39.0 |
| RUSTSEC-2026-0047 | aws-lc-sys 0.34.0 | PKCS7_verify Signature Validation Bypass in AWS-LC | >=0.38.0 |
| RUSTSEC-2026-0046 | aws-lc-sys 0.34.0 | PKCS7_verify Certificate Chain Validation Bypass in AWS-LC | >=0.38.0 |
| RUSTSEC-2026-0007 | bytes 1.11.0 | Integer overflow in `BytesMut::reserve` | >=1.11.1 |
| RUSTSEC-2026-0204 | crossbeam-epoch 0.9.18 | Invalid pointer dereference in `fmt::Pointer` impl for `Atomic` and `Shared` when the underlying pointer is invalid | >=0.9.20 |
| RUSTSEC-2026-0258 | h2 0.3.27 | h2 unbounded empty DATA frames | >=0.4.16 |
| RUSTSEC-2026-0258 | h2 0.4.12 | h2 unbounded empty DATA frames | >=0.4.16 |
| RUSTSEC-2026-0041 | lz4_flex 0.11.5 | Decompressing invalid data can leak information from uninitialized memory or reused output buffer | >=0.11.6, <0.12.0; >=0.12.1 |
| RUSTSEC-2026-0216 | nostr 0.44.2 | Remote Denial of Service via malformed NIP‑44 v2 payload | >=0.44.5, <0.45.0-alpha.1; >=0.45.0-alpha.5 |
| RUSTSEC-2026-0226 | nostr 0.44.2 | Wallet event parsers accept unauthenticated events | >=0.44.7 |
| RUSTSEC-2026-0227 | nostr 0.44.2 | NIP-44 v2 decryption permits resource exhaustion | >=0.44.7 |
| RUSTSEC-2026-0228 | nostr 0.44.2 | NIP-04 parsing amplifies malformed ciphertext memory use | >=0.44.7 |
| RUSTSEC-2026-0219 | nostr 0.44.2 | Remote Denial of Service via malformed NIP-04 IV | >=0.44.6, <0.45.0-alpha.1; >=0.45.0-alpha.6 |
| RUSTSEC-2026-0229 | nostr 0.44.2 | NIP-98 authorization parsing permits resource exhaustion | >=0.44.7 |
| RUSTSEC-2026-0230 | nostr 0.44.2 | Empty NIP-50 search filters can panic | >=0.44.7 |
| RUSTSEC-2026-0225 | nostr 0.44.2 | Debug output exposes NIP-46 and NIP-60 credentials | >=0.44.7 |
| RUSTSEC-2026-0232 | nostr-relay-pool 0.44.0 | Processing of unverified relay events | >=0.44.3 |
| RUSTSEC-2026-0224 | nostr-relay-pool 0.44.0 | Verification cache poisoning allows forged Nostr events to bypass signature validation | >=0.44.2 |
| RUSTSEC-2026-0231 | nostr-relay-pool 0.44.0 | Relay authentication challenges can exhaust memory | >=0.44.3 |
| RUSTSEC-2026-0185 | quinn-proto 0.11.13 |  Remote memory exhaustion in quinn-proto from unbounded out-of-order stream reassembly | >=0.11.15 |
| RUSTSEC-2026-0037 | quinn-proto 0.11.13 | Denial of service in Quinn endpoints | >=0.11.14 |
| RUSTSEC-2026-0285 | rustls 0.23.35 | TLS 1.3 handshake messages incorrectly accepted across encryption level boundaries | >=0.23.45 |
| RUSTSEC-2026-0104 | rustls-webpki 0.101.7 | Reachable panic in certificate revocation list parsing | >=0.103.13, <0.104.0-alpha.1; >=0.104.0-alpha.7 |
| RUSTSEC-2026-0098 | rustls-webpki 0.101.7 | Name constraints for URI names were incorrectly accepted | >=0.103.12, <0.104.0-alpha.1; >=0.104.0-alpha.6 |
| RUSTSEC-2026-0099 | rustls-webpki 0.101.7 | Name constraints were accepted for certificates asserting a wildcard name | >=0.103.12, <0.104.0-alpha.1; >=0.104.0-alpha.6 |
| RUSTSEC-2026-0104 | rustls-webpki 0.103.8 | Reachable panic in certificate revocation list parsing | >=0.103.13, <0.104.0-alpha.1; >=0.104.0-alpha.7 |
| RUSTSEC-2026-0098 | rustls-webpki 0.103.8 | Name constraints for URI names were incorrectly accepted | >=0.103.12, <0.104.0-alpha.1; >=0.104.0-alpha.6 |
| RUSTSEC-2026-0099 | rustls-webpki 0.103.8 | Name constraints were accepted for certificates asserting a wildcard name | >=0.103.12, <0.104.0-alpha.1; >=0.104.0-alpha.6 |
| RUSTSEC-2026-0049 | rustls-webpki 0.103.8 | CRLs not considered authoritative by Distribution Point due to faulty matching logic | >=0.103.10 |
| RUSTSEC-2026-0009 | time 0.3.44 | Denial of Service via Stack Exhaustion | >=0.3.47 |

Priorities:
1. Minimal compatible security update: nostr >=0.44.7 within 0.44, relay-pool >=0.44.3, compatible SDK/database companions. Prove resolution, test admission and NIP-42, then review audit again. These are patch floors, not a tested lockfile proposal.
2. Separately evaluate nostr-sdk 0.45.2 / nostr 0.45.x. Published 0.45 release notes include negentropy idle timeout and substantial sync/stream API changes, dedicated Authenticator, proxy changes and removal of old Tor feature. Worth migrating, but not a drop-in version bump or replacement for application lifecycle ownership.
3. Patch bytes/lz4/time/current TLS lines; migrate AWS transport dependencies to eliminate old affected h2/webpki lines. Check toolchain/MSRV before choosing versions. Assess crypto-specific advisory reachability without assuming all AWS-LC functionality is used.
4. Resolve unsound/unmaintained/yanked warnings and inspect the git-pinned notepack revision. RustSec absence is not an audit of native RocksDB/DuckDB or git dependencies.
5. Add scheduled and PR dependency audit CI, narrowly justified time-limited exceptions, and a repeatable deployed-binary provenance manifest. Existing just audit exists, but CI inspected here does not run it.

Primary sources:
- https://rustsec.org/advisories/RUSTSEC-2026-0224.html
- https://rustsec.org/advisories/RUSTSEC-2026-0232.html
- https://rustsec.org/advisories/RUSTSEC-2026-0243.html
- https://docs.rs/crate/nostr-sdk/0.45.2/source/CHANGELOG.md
- https://docs.rs/crate/nostr/0.45.5

NIP-04/44, wallet, NIP-98 and search advisories require call-path/feature assessment; merely receiving an encrypted event does not imply decrypting it. No claim of exploitation is made.

## Proposed implementation slices — approval required

1. **Integrity and regressions.** Security patch floor; validate before admission; repair reservation ownership. Deterministic malicious-relay and admission-failure tests. Keep this independently reviewable.
2. **Bounded relay worker.** Explicit connect/reconcile/drain/close phases, absolute lifetime deadline, idle timeout, cancellation and joined child cleanup. Initially two concurrent allowlisted relays; tune from measurements. Streaming bounded admission rather than return Vec<Event>. Test a silent relay alongside a healthy relay and hangs at every teardown phase.
3. **Durable inventory and resource limits.** Sealed-segment receipt integration; bounded item windows and prune batches; retry coverage checkpoints; no false durable claims. Crash/restart at every boundary; same-timestamp and future-event tests.
4. **Operational policy.** INFO phase/outcome logs, honest success metrics, supervisor, persisted cooldown, explicit allowlist, alert rules and runbook. Verify healthy progress even if every other relay hangs.
5. **Controlled rollout and SDK modernization.** Decide whether the 0.45 migration belongs in slice 2 before doing duplicate API work. Preserve the minimal security patch as an independent change. Full just precommit before each commit; canary with a tiny relay set and explicit memory/network budgets; observe several 30-minute cycles, cancellation, restart, live ingestion, Parquet and ClickHouse. Expand catalog only after those gates.

Acceptance criteria:
- Every relay and cycle finishes or reports bounded partial/failure within configured deadlines.
- A hanging relay cannot delay healthy relay admission.
- No orphan tasks, sockets, collectors or stuck gauges after timeout/cancel/panic.
- Queue, item-set and worker memory bounds demonstrated under flood and same-timestamp cases.
- Failed pre-admission events remain retryable; only durable archive receipts advance inventory.
- Invalid events cannot poison dedupe or enter the archive.
- Per-relay and overall coverage remain truthful, including outages longer than lookback.
- No ingestion throughput regression or unexpected restart; disk reserve stays above agreed floor.

## Limitations

No production fault injection, archive-wide validity scan, upgrade experiment or full test-suite run was performed. The exact historical hang location and deployed binary dependency provenance remain unproven. This audit establishes concrete source defects and locked-dependency risks, not an assertion that every possible negentropy failure has been discovered.
