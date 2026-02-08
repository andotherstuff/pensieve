# Stability Fixes Plan

Audit conducted 2026-02-08 covering the full ingestion pipeline, relay management, and sync subsystems. Findings are grouped into tiers by severity and organized for tackling in separate sessions.

---

## Tier 1: Data Loss and Memory Safety

These issues can cause permanent event loss or undefined behavior. Fix first.

### 1.1 Unsafe raw pointer use-after-free in ClickHouse indexer

- **File:** `crates/pensieve-ingest/src/pipeline/clickhouse.rs:112-120`
- **Severity:** Critical (undefined behavior)
- **Description:** `start()` casts `&self.segments_indexed` and `&self.events_indexed` to raw pointers via `usize`, then dereferences them inside a spawned thread. If the `ClickHouseIndexer` struct is dropped while the thread is still running, the thread reads dangling memory. Today this likely works because `main.rs` joins the handle before the indexer is dropped, but any refactor, panic unwind, or early-return error path that drops the indexer early causes a use-after-free.
- **Fix:** Replace the raw pointer casts with `Arc<AtomicUsize>`, the same pattern already used for `self.running`. This is a ~10 line change.

### 1.2 Event loss on segment write failure

- **Files:** `main.rs:808-826`, `pipeline/dedupe.rs`, `pipeline/segment.rs`
- **Severity:** High (permanent data loss)
- **Description:** The ingestion flow is: (1) `check_and_mark_pending` marks event in dedupe, (2) `pack_nostr_event` serializes it, (3) `segment_writer.write()` writes it. If step 3 fails (disk full, I/O error), the event is permanently marked Pending in dedupe but never written to any segment. On re-ingestion the dedupe says "already pending" and rejects it. The event is lost from both archive and ClickHouse.
- **Fix options:**
  - A: On write failure, remove the pending marker from dedupe (rollback).
  - B: Add a startup recovery step that scans for stale Pending entries (older than some threshold) and clears them from the dedupe index so they can be re-ingested.
  - Option B is more robust because it also covers crash scenarios.

### 1.3 Crash recovery gap for buffered writes

- **File:** `pipeline/segment.rs:257`
- **Severity:** High (data loss on crash)
- **Description:** The `BufWriter` has an 8MB buffer. On unclean shutdown (SIGKILL, OOM kill, power loss), up to 8MB of buffered events are lost. Those events are stuck as Pending in dedupe forever with no recovery path. There is no mechanism to detect or clear these stale entries.
- **Fix:** Two parts:
  - Periodic `flush()` of the `BufWriter` (e.g., every 30 seconds or every N events) to reduce the window of data at risk.
  - A startup recovery step that clears stale Pending entries from the dedupe index (same as 1.2 option B -- one fix covers both).

### 1.4 No retry on ClickHouse indexing failure

- **File:** `pipeline/clickhouse.rs:150-157`
- **Severity:** High (permanent index gap)
- **Description:** When `index_segment()` fails (ClickHouse down, network blip, timeout), the `SealedSegment` message is consumed from the channel and gone. There is a `// TODO: Implement retry logic` comment. Since ClickHouse is a derived index (notepack archive is source of truth), this isn't full data loss, but the ClickHouse index becomes permanently incomplete. Manual re-indexing from the archive is the only recovery.
- **Fix:** Push failed segments to a retry queue (in-memory with periodic disk persistence) that re-attempts indexing on subsequent cycles or on next startup.

---

## Tier 2: Stability and Correctness

These issues affect reliability, resource usage, or correctness but don't cause immediate data loss.

### 2.1 Background compression thread is fire-and-forget

- **File:** `pipeline/segment.rs:373-417`
- **Severity:** High
- **Description:** `seal()` spawns a `std::thread::spawn` for gzip compression and discards the JoinHandle. Consequences:
  - If the process exits, the thread may be killed mid-write, leaving corrupt `.gz` files and orphaned uncompressed `.notepack` files.
  - `SegmentWriter::drop()` calls `seal()` which spawns another thread, then Drop returns and the process may exit before compression finishes.
  - The ClickHouse indexer notification is sent from the background thread; if the channel is closed by that point, the notification is lost.
- **Fix:** Store the compression `JoinHandle` in the `SegmentWriter` and join it in `Drop` and in the explicit shutdown sequence. This ensures compression completes before the process exits.

### 2.2 ClickHouse indexer loads entire segment into memory

- **File:** `pipeline/clickhouse.rs:192-200`
- **Severity:** Medium
- **Description:** `read_segment()` deserializes all events into a `Vec<EventRow>` before inserting into ClickHouse. A 256MB compressed segment can decompress to 500MB+, and the `EventRow` structs with all their String fields easily double that to 1GB+. The `batch_size` config field is defined (default 10,000) but never actually used -- all events go in a single insert.
- **Fix:** Stream events from the segment file in batches of `batch_size`, inserting each batch into ClickHouse before loading the next. This caps memory usage to roughly `batch_size * avg_event_size`.

### 2.3 `created_at` truncated from u64 to u32

- **File:** `pipeline/clickhouse.rs:65,279`
- **Severity:** Medium
- **Description:** `EventRow.created_at` is defined as `u32`, and `note.created_at` (a u64 Nostr timestamp) is cast with `as u32`. Events with `created_at > u32::MAX` (year 2106, or malicious/buggy events with large timestamps) will have silently wrong timestamps in ClickHouse. The ClickHouse `DateTime` type is u32-based, but `DateTime64` supports larger ranges.
- **Fix:** Either upgrade the ClickHouse column to `DateTime64` and change the Rust type to `u64`, or validate/clamp the timestamp before insertion and log a warning for out-of-range values.

### 2.4 `reconcile_relay_status` misses relays removed from nostr-sdk

- **File:** `source/relay.rs:296-301`
- **Severity:** Medium
- **Description:** Reconciliation iterates `client.relays().await`, which only returns relays nostr-sdk is currently tracking. If a relay was added, failed to connect, and nostr-sdk silently removed it from its internal pool, reconciliation never sees it. The relay stays in `active` status in the SQLite database indefinitely. These "ghost active" relays don't waste connections but inflate metrics and can affect optimization decisions.
- **Fix:** Also query the SQLite database for all relays with status `active`, and if any are not present in `client.relays()`, mark them as disconnected.

### 2.5 `negentropy::sync_once` can panic on task abort

- **File:** `sync/negentropy.rs:365-367`
- **Severity:** Medium
- **Description:** `Arc::try_unwrap().expect()` panics if another Arc clone still exists. The collector task clones the Arc. If the task is aborted during shutdown (main.rs:872 calls `.abort()`), the Arc inside the cancelled future may not have been dropped, causing the `expect()` to panic.
- **Fix:** Replace `Arc::try_unwrap().expect()` with `Arc::try_unwrap().unwrap_or_else(|arc| arc.lock().unwrap().drain(..).collect())` or similar fallback that handles the case where the Arc has multiple owners.

### 2.6 Negentropy sync loads unbounded events into memory

- **File:** `sync/negentropy.rs:276,290` and `sync/state.rs:113-131`
- **Severity:** Medium
- **Description:** Two memory concerns in the negentropy sync:
  - `collected_events` accumulates all events received during a sync cycle in a `Vec<Event>` behind a Mutex. For a 14-day lookback against a large relay, the delta could be millions of events consuming gigabytes.
  - `get_items_since()` loads all matching entries from the sync state RocksDB into a Vec. For a 14-day window with millions of events, this is hundreds of MB.
- **Fix:** Process events in streaming batches rather than collecting them all, or add a cap on the number of events per sync cycle.

---

## Tier 3: Discovery Effectiveness and Optimization

These issues affect how well the system discovers and evaluates new relays.

### 3.1 Scoring cold-start: new relays always score 0

- **File:** `relay/scoring.rs:74`, `relay/manager.rs:700-714`
- **Severity:** Medium (discovery effectiveness)
- **Description:** New relays have `novel_rate_7d = 0` and `uptime_7d = 0`, producing a score of 0.0. The exploration loop (3 slots per 5-minute cycle, 24-hour cooldown) is the only way they get tried. With 10,000 discovered relays, it would take ~11.5 days to try each one once. After the first try, if a relay fails it moves to `failing` and is never tried again (per our bug fixes).
- **Fix options:**
  - Increase `exploration_slots` (e.g., from 3 to 10).
  - Give newly-discovered relays a small initial score (e.g., 0.1) so they're competitive in the swap logic, not just exploration.
  - Reduce `exploration_min_age_secs` (e.g., from 24h to 6h) to try relays more frequently.
  - Add a "new relay bonus" that decays over time.

### 3.2 Hourly stats only flushed on hour boundary

- **File:** `relay/manager.rs:568-582`
- **Severity:** Medium (scoring accuracy)
- **Description:** `maybe_flush_hour()` only triggers when the hour rolls over. If the process runs for less than 1 hour (or during the first hour of operation), the in-memory accumulators never make it to SQLite. This means `recompute_scores()` has no hourly data to work with -- all relays score 0 and the optimization loop makes no meaningful decisions.
- **Fix:** Also flush on optimization cycles (not just hour boundaries). Add a `flush_if_stale()` method that flushes if data is older than N minutes, and call it from `recompute_scores()`.

### 3.3 Exploration slots are additive (push connections above max)

- **File:** `source/relay.rs:749-781`
- **Severity:** Low (mitigated)
- **Description:** Each optimization cycle disconnects up to `max_swap_percent` (5%) of connected relays, then connects swap replacements (same count), then also connects `exploration_slots` (3) additional relays. Exploration is additive -- there's no corresponding disconnect. Over multiple cycles, this pushed the connection count above `max_relays`. This is now mitigated by the ConnectionGuard we added, which hard-caps connections, but the optimization loop doesn't know about the guard and wastes cycles suggesting connections that will be rejected.
- **Fix:** Make the optimization loop aware of the current connection count vs max, and only suggest exploration relays if there's headroom. Or have exploration slots replace the lowest-scoring non-seed connected relays.

### 3.4 `known_relays` HashSet grows without bound

- **File:** `source/relay.rs:879-881`
- **Severity:** Low
- **Description:** Every discovered relay URL is inserted into `known_relays` but never removed. Over weeks with discovery enabled, this set grows to tens of thousands of entries. The set is checked on every NIP-65 event with a read lock.
- **Fix:** Periodically prune `known_relays` to match the relay manager's database, or cap its size and evict the oldest entries.

---

## Tier 4: Low Priority / Housekeeping

### 4.1 Global dedupe mutex serializes all event processing

- **File:** `pipeline/dedupe.rs:157`
- **Description:** A single `Mutex` protects all event ID checks. Under high throughput (10K+ events/sec), this is a bottleneck. A sharded lock (hash first byte of event_id, use N locks) would allow parallelism.

### 4.2 `batch_size` config is unused in ClickHouse indexer

- **File:** `pipeline/clickhouse.rs:46`
- **Description:** `ClickHouseConfig::batch_size` is set to 10,000 in Default but never used. All events are inserted in a single call.

### 4.3 Division by zero in compression log

- **File:** `pipeline/segment.rs:387`
- **Description:** If a segment is sealed with zero events, `(compressed_bytes / size_bytes) * 100.0` produces NaN. Cosmetic.

### 4.4 Error types lose source context

- **File:** `error.rs`
- **Description:** Several error variants use `String` instead of wrapping source errors (e.g., `Serialization(String)`, `Database(String)`). This loses error chains and backtraces, making debugging harder.

---

## Suggested Session Order

1. **Session: Data Safety** -- Fix 1.1, 1.2, 1.3 (add startup Pending recovery + periodic BufWriter flush + replace raw pointers)
2. **Session: Shutdown Robustness** -- Fix 2.1, 1.4 (compression thread join + ClickHouse retry queue)
3. **Session: Discovery Effectiveness** -- Fix 3.1, 3.2, 3.3 (scoring cold-start, flush timing, exploration awareness)
4. **Session: Memory & Performance** -- Fix 2.2, 2.6, 4.1 (batched ClickHouse indexing, negentropy streaming, dedupe sharding)
5. **Session: Cleanup** -- Fix 2.3, 2.4, 2.5, 3.4, and Tier 4 items
