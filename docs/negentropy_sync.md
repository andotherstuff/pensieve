# Negentropy Sync for Live Ingestion

**Status:** Planning
**Branch:** `negentropy`
**Created:** 2026-01-04

## Overview

This document outlines the plan to integrate NIP-77 negentropy sync into the live ingester. The goal is to periodically reconcile missing events from a curated set of trusted relays while simultaneously streaming live events.

### Why Negentropy?

The current `--catchup` mode has fundamental limitations:
- Most relays limit the number of events returned to a subscription (100-1000 events)
- There's no built-in pagination in the REQ/SUB protocol
- You get what you get after subscribing with a `since` filter

Negentropy (NIP-77) solves this by:
- Efficiently computing the **difference** between local and remote event sets
- Using range-based set reconciliation to minimize bandwidth
- Only transferring event IDs you're missing (not full events initially)
- Working even with millions of events

## Architecture

### Single Process Design

The ingester runs as a single process because:
- We need **atomic dedupe + write** semantics across live and sync sources (see "Why This Works" / "Dedupe atomicity" below)
- Single process simplifies coordination between live and sync operations
- Memory pressure is manageable with careful batching; if needed, we can fall back to **partitioned sync windows** (time/kind sharding)

### Event Convergence: The Critical Insight

**Both event sources feed into the SAME event handler function.** This is the key to understanding how the system works:

```
                    ┌────────────────────────────┐
                    │      Live RelaySource      │
                    │   (streaming from relays)  │
                    │                            │
                    │   Runs continuously,       │
                    │   receives events as they  │
                    │   happen across network    │
                    └─────────────┬──────────────┘
                                  │
                                  │ Event
                                  ▼
┌────────────────────────────────────────────────────────────────────┐
│                                                                     │
│              SHARED EVENT HANDLER (single instance)                 │
│                                                                     │
│   fn handle_event(event: &Event) -> Result<bool> {                  │
│                                                                     │
│       // Step 1: Dedupe check via RocksDB                           │
│       // This is the CRITICAL synchronization point!                │
│       // Both sources hit this same check.                          │
│       if !dedupe.check_and_mark_pending(&event.id)? {               │
│           // Already have this event (from other source or earlier) │
│           metrics::counter!("events_deduplicated").increment(1);    │
│           return Ok(true);  // Skip, continue processing            │
│       }                                                             │
│                                                                     │
│       // Step 2: Novel event - pack and write                       │
│       let packed = pack_nostr_event(event)?;                        │
│       segment_writer.write(packed)?;                                │
│       metrics::counter!("events_written").increment(1);             │
│                                                                     │
│       Ok(true)                                                      │
│   }                                                                 │
│                                                                     │
└────────────────────────────────────────────────────────────────────┘
                                  ▲
                                  │ Event
                                  │
                    ┌─────────────┴──────────────┐
                    │    Negentropy Fetcher      │
                    │   (periodic sync task)     │
                    │                            │
                    │   Runs every N minutes,    │
                    │   discovers missing events │
                    │   from trusted relays      │
                    └────────────────────────────┘
```

### Why This Works

1. **RocksDB is the single source of truth for "have we seen this event?"**
   - Both sources check the same `DedupeIndex`
   - **Important:** `check_and_mark_pending()` must be atomic across sources (today it's a read+write)
   - Plan: make `check_and_mark_pending()` a **single critical section** (e.g., internal mutex) so only one source can "win" a given event ID
   - If live streaming gets an event first, negentropy fetch will skip it (and vice versa)

2. **SegmentWriter handles concurrent writes**
   - Events from both sources write to the same segment
   - The writer is already designed to handle this (mutex-protected)

3. **Coordination is minimal (but explicit)**
   - Live and negentropy can run independently, but they share the same `DedupeIndex` + `SegmentWriter`
   - The only required coordination is making the dedupe check+mark atomic so duplicates cannot slip through under concurrency

### Full Component Flow

```
┌─────────────────────────────────────────────────────────────────────┐
│                         pensieve-ingest                              │
├─────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  ┌──────────────────┐       ┌──────────────────┐                    │
│  │   Live Ingester  │       │  Negentropy Sync │                    │
│  │   (RelaySource)  │       │     (Periodic)   │                    │
│  │                  │       │                  │                    │
│  │ • Stream events  │       │ • Every 10 min   │                    │
│  │ • All relays     │       │ • Trusted relays │                    │
│  │ • Continuous     │       │ • Batch fetch    │                    │
│  └────────┬─────────┘       └────────┬─────────┘                    │
│           │                          │                              │
│           │ Event                    │ Event                        │
│           │                          │                              │
│           └──────────┬───────────────┘                              │
│                      │                                              │
│                      ▼                                              │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                  Shared Event Handler                         │   │
│  │                                                               │   │
│  │  handle_event(event) called from BOTH sources                 │   │
│  │                                                               │   │
│  │  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐        │   │
│  │  │   Dedupe    │───▶│    Pack     │───▶│    Write    │        │   │
│  │  │  (RocksDB)  │    │ (notepack)  │    │  (segment)  │        │   │
│  │  └─────────────┘    └─────────────┘    └─────────────┘        │   │
│  │        │                                                      │   │
│  │        │ duplicate?                                           │   │
│  │        ▼                                                      │   │
│  │      skip                                                     │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│                              ▼                                      │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                    SegmentWriter                              │   │
│  │                                                               │   │
│  │  • Append to current segment (thread-safe)                    │   │
│  │  • Seal when threshold reached                                │   │
│  │  • Index to ClickHouse                                        │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                      │
└─────────────────────────────────────────────────────────────────────┘
```

## nostr-sdk Negentropy API

Based on the nostr-sdk 0.44 source, the key methods are:

```rust
// Sync events with specific relays (negentropy reconciliation)
pub async fn sync_with<I, U>(
    &self,
    urls: I,
    filter: Filter,
    opts: &SyncOptions,
) -> Result<Output<Reconciliation>, Error>

// The Reconciliation result contains:
pub struct Reconciliation {
    /// Event IDs that exist locally but not on the relay
    pub local: HashSet<EventId>,
    /// Event IDs that were received from the relay
    pub received: HashSet<EventId>,
    /// Event IDs that were sent to the relay
    pub sent: HashSet<EventId>,
}

// SyncOptions configures the sync direction
pub struct SyncOptions {
    /// Direction: Up (send), Down (receive), Both
    pub direction: SyncDirection,
    /// Initial message size
    pub initial_message_size: u16,
    /// ...
}
```

### The nostr-sdk Database Challenge

nostr-sdk's `sync_with` method requires a `NostrDatabase` implementation:
- It calls `database.negentropy_items(filter)` to get local event IDs + timestamps
- It uses these to compute the initial fingerprint for the negentropy protocol
- After reconciliation, it automatically fetches missing events via REQ

**Problem:** Our architecture uses RocksDB for dedupe (event IDs only, no timestamps or full events) and notepack segments for storage. We don't have a `NostrDatabase`-compatible store.

### Our Approach: Two Options

#### Option A: Separate RocksDB for Sync State (Recommended)

Maintain a separate RocksDB database for negentropy sync state:

```rust
// Key: [timestamp_be (8 bytes)][event_id (32 bytes)]
// Value: empty
//
// Big-endian timestamp prefix enables efficient ordered range scans
// (via IteratorMode::From) to get all events since a given timestamp.
```

Then implement `NostrDatabase` trait backed by this RocksDB:

```rust
impl NostrDatabase for SyncStateDb {
    async fn negentropy_items(&self, filter: Filter) -> Result<Vec<(EventId, Timestamp)>> {
        // Use an ordered iterator starting at key [since_be][0x00...]
        // Return (event_id, created_at) pairs
        self.get_items_since(filter.since.unwrap_or(0))
    }

    async fn save_event(&self, event: &Event) -> Result<bool> {
        // Record in sync state for future negentropy comparisons
        self.record(&event.id.as_bytes(), event.created_at.as_secs())?;
        Ok(true)
    }
}
```

**Why RocksDB over SQLite?**
- Faster for simple key-value operations
- Lighter overhead (no SQL engine)
- LZ4 compression built-in
- Consistent with existing codebase patterns

#### Option B: Bypass nostr-sdk, Implement NIP-77 Directly

Implement negentropy protocol ourselves:
- More control, fewer dependencies
- Significant implementation effort
- Would need to handle `NEG-OPEN`, `NEG-MSG`, `NEG-CLOSE` manually

**Recommendation:** Option A - separate RocksDB is fast, light, and familiar.

### How Negentropy Sync Actually Works

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Negentropy Sync Cycle                            │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  1. BUILD FILTER                                                    │
│     filter = Filter::new().since(now - 7 days)                      │
│                                                                     │
│  2. QUERY LOCAL SYNC STATE                                          │
│     local_items = sync_state_db.negentropy_items(filter)            │
│     // Returns [(event_id, timestamp), ...]                         │
│                                                                     │
│  3. NEGENTROPY RECONCILIATION (nostr-sdk handles this)              │
│     ┌─────────────┐          ┌─────────────────────┐                │
│     │   Client    │          │    Trusted Relay    │                │
│     └──────┬──────┘          └──────────┬──────────┘                │
│            │                            │                           │
│            │  NEG-OPEN (filter, fingerprint of local_items)         │
│            │───────────────────────────▶│                           │
│            │                            │                           │
│            │  NEG-MSG (relay's fingerprint)                         │
│            │◀───────────────────────────│                           │
│            │                            │                           │
│            │        ... repeat until reconciled ...                 │
│            │                            │                           │
│            │  Result: "you're missing event IDs: [A, B, C, ...]"    │
│            │                            │                           │
│                                                                     │
│  4. FETCH MISSING EVENTS                                            │
│     for each missing_id:                                            │
│         event = fetch_event_by_id(missing_id)                       │
│                                                                     │
│  5. PROCESS THROUGH SHARED HANDLER                                  │
│     for each event:                                                 │
│         handle_event(event)  // Same handler as live streaming!     │
│         sync_state_db.save(event.id, event.created_at)              │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### Key Points

1. **Events from negentropy go through the SAME `handle_event()` function** as live streaming
2. **RocksDB dedupe catches duplicates** - if live streaming already got the event, we skip it
3. **Sync state database is separate from the main storage** - it only tracks what we've synced, not the full events
4. **nostr-sdk handles the negentropy protocol** - we just provide the local state and receive missing events

## Trusted Relays for Negentropy

Not all relays support NIP-77. We need a curated list of relays that:
1. **Support NIP-77** negentropy protocol
2. **Have comprehensive archives** (store lots of historical data)
3. **Are reliable** (good uptime, low latency)

### Suggested Initial List

```txt
# Known NIP-77 relays with good archives
wss://relay.damus.io
wss://relay.primal.net
```

### Configuration

```rust
/// Configuration for negentropy sync
pub struct NegentropySyncConfig {
    /// Relay URLs to sync from (must support NIP-77)
    pub relays: Vec<String>,

    /// How often to run sync (e.g., every 10 minutes)
    pub interval: Duration,

    /// How far back to sync (e.g., last 7 days)
    /// This creates the `since` filter
    pub lookback: Duration,

    /// Maximum events to fetch per sync cycle
    pub max_events_per_cycle: usize,

    /// Timeout for negentropy protocol messages
    pub protocol_timeout: Duration,

    /// Timeout for fetching missing events
    pub fetch_timeout: Duration,
}
```

## Implementation Plan

### Phase 0: Seed Sync State from ClickHouse (bootstrap)

If the sync-state DB starts empty, negentropy will believe we have *no* local items in the lookback
window and can trigger a huge first-cycle delta (wasted bandwidth + higher chance of relay blocking).
To avoid that cold-start behavior, do a best-effort seed from ClickHouse **before** the first sync:

- **When to seed**: if `--negentropy` is enabled and `sync_state_db` is empty (or below a small threshold)
- **Query**: pull `(id, created_at)` from `events_local` for the lookback window (example: 7-day default):

```sql
SELECT
  id,
  toUnixTimestamp(created_at) AS created_at
FROM events_local
WHERE created_at >= now() - INTERVAL 7 DAY
```

- **Insert**:
  - Decode `id` (hex string) into a 32-byte event ID
  - `sync_state_db.record(&event_id_bytes, created_at)`

Notes:
- This is best-effort: if ClickHouse is unavailable, skip seeding and let sync proceed (but consider shrinking the initial sync window).
- This seeds the *local set representation* for negentropy; dedupe remains the source of truth for writes.

### Phase 1: Negentropy Sync Module

Create `crates/pensieve-ingest/src/sync/negentropy.rs`:

```rust
pub struct NegentropySyncer {
    config: NegentropySyncConfig,
    client: Client,  // nostr-sdk client for negentropy
    running: Arc<AtomicBool>,
}

impl NegentropySyncer {
    /// Run a single sync cycle
    pub async fn sync_once(&self) -> Result<SyncStats, Error> {
        // 1. Build filters for the lookback window
        //
        // Start with a single lookback filter for simplicity.
        // If we hit `NEG-ERR: blocked` or the local item set is too large (memory/latency),
        // fall back to partitioned sync (time and/or kind sharding).
        let since = Timestamp::now() - self.config.lookback;
        let filter = Filter::new().since(since);

        // 2. Run negentropy reconciliation
        let opts = SyncOptions::default()
            .direction(SyncDirection::Down);
        let output = self.client.sync_with(
            &self.config.relays,
            filter,
            &opts,
        ).await?;

        // 3. Fetch missing events
        // The events in output.received are already fetched by nostr-sdk
        // We need to process them through our pipeline

        // 4. Return stats
        Ok(SyncStats {
            events_discovered: output.received.len(),
            // ...
        })
    }

    /// Run periodic sync in background
    pub async fn run_periodic<F>(&self, handler: F) -> Result<(), Error>
    where
        F: Fn(&Event) -> Result<bool, Error> + Send + Sync,
    {
        while self.running.load(Ordering::SeqCst) {
            self.sync_once().await?;
            tokio::time::sleep(self.config.interval).await;
        }
        Ok(())
    }
}
```

### Phase 2: Integrate with Live Ingester

Modify `main.rs` to run both:

```rust
// Spawn negentropy sync task
let sync_handle = if args.negentropy_enabled {
    let syncer = NegentropySyncer::new(negentropy_config);
    Some(tokio::spawn(async move {
        syncer.run_periodic(|event| {
            // Same handler as live ingestion
            // NOTE: this is only correct if the dedupe check+mark is atomic across tasks
            handle_event(event)
        }).await
    }))
} else {
    None
};

// Run live ingestion (existing code)
relay_source.run_async(|relay_url, event| {
    handle_event(event)
}).await?;

// Cleanup
if let Some(handle) = sync_handle {
    handle.abort();
}
```

### Phase 3: Sync State Database

Add a **separate RocksDB database** for negentropy sync state. This is independent from:
- RocksDB dedupe index (event IDs only, for deduplication)
- Notepack segments (full event storage)
- ClickHouse (analytics)

**Why a separate RocksDB (not SQLite)?**
- Faster for this workload (simple key-value operations)
- Lighter overhead (no SQL query engine)
- Consistent with existing codebase patterns
- Independent lifecycle - can prune/rebuild without affecting dedupe

**Key structure for efficient time-range queries:**

```rust
// crates/pensieve-ingest/src/sync/state.rs

/// Sync state database for negentropy reconciliation.
///
/// Stores (timestamp, event_id) pairs to enable efficient time-range queries.
/// Key format: [timestamp_be (8 bytes)][event_id (32 bytes)]
/// Value: empty (we only need to know the key exists)
///
/// Using big-endian timestamp as the leading bytes allows RocksDB to efficiently
/// scan all events >= a given timestamp using an ordered iterator
/// (IteratorMode::From). Note: `prefix_iterator()` is not appropriate for range scans.
pub struct SyncStateDb {
    db: rocksdb::DB,
}

impl SyncStateDb {
    pub fn new(path: &Path) -> Result<Self> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);

        // Optional: optimize for prefix seeks (timestamp prefix).
        // We still use range iteration (IteratorMode::From), not prefix_iterator().
        opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(8));

        let db = rocksdb::DB::open(&opts, path)?;
        Ok(Self { db })
    }

    /// Build key from timestamp and event_id
    fn make_key(created_at: u64, event_id: &[u8; 32]) -> [u8; 40] {
        let mut key = [0u8; 40];
        key[0..8].copy_from_slice(&created_at.to_be_bytes()); // Big-endian for sorting
        key[8..40].copy_from_slice(event_id);
        key
    }

    /// Get all events since a timestamp (for negentropy)
    pub fn get_items_since(&self, since: u64) -> Result<Vec<([u8; 32], u64)>> {
        let mut items = Vec::new();

        // Iterate from key [since_be][0x00...] to end
        let start_key = Self::make_key(since, &[0u8; 32]);
        let iter =
            self.db
                .iterator(rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward));
        for item in iter {
            let (key, _) = item?;
            if key.len() == 40 {
                let ts = u64::from_be_bytes(key[0..8].try_into().unwrap());
                let mut event_id = [0u8; 32];
                event_id.copy_from_slice(&key[8..40]);
                items.push((event_id, ts));
            }
        }

        Ok(items)
    }

    /// Record that we've processed an event
    pub fn record(&self, event_id: &[u8; 32], created_at: u64) -> Result<()> {
        let key = Self::make_key(created_at, event_id);
        self.db.put(&key, &[])?; // Empty value
        Ok(())
    }

    /// Prune entries older than the given timestamp
    pub fn prune_before(&self, before: u64) -> Result<usize> {
        // Prefer range deletion over per-key deletes for performance.
        let start_key = [0u8; 40];
        let end_key = Self::make_key(before, &[0u8; 32]);
        self.db.delete_range(&start_key, &end_key)?;

        // Exact count would require scanning; return 0 (or track metrics elsewhere).
        Ok(0)
    }
}
```

**Size estimate:**
- 40 bytes per key (8 byte timestamp + 32 byte event ID), no value
- With LZ4 compression: ~25-30 bytes per event effective
- ~1M events/day → ~25-30MB/day
- 7-day window → ~175-210MB total
- Periodic pruning keeps it bounded

**Integration with shared handler:**

```rust
fn handle_event(event: &Event, source: EventSource) {
    // Step 1: Dedupe check (RocksDB) - ALWAYS
    if !dedupe.check_and_mark_pending(&event.id)? {
        return Ok(true);  // Skip duplicate
    }

    // Step 2: Pack and write (SegmentWriter) - ALWAYS
    let packed = pack_nostr_event(event)?;
    segment_writer.write(packed)?;

    // Step 3: Record in sync state - ONLY for negentropy tracking
    // This is optional - we only need it if negentropy is enabled
    if let Some(sync_db) = &sync_state_db {
        sync_db.record(event.id.as_bytes(), event.created_at.as_secs())?;
    }

    Ok(true)
}
```

### Phase 4: CLI Arguments

```rust
/// Enable periodic negentropy sync with trusted relays
#[arg(long)]
negentropy: bool,

/// Negentropy sync interval in seconds
#[arg(long, default_value = "600")]  // 10 minutes
negentropy_interval_secs: u64,

/// Negentropy lookback window in days
#[arg(long, default_value = "7")]
negentropy_lookback_days: u64,

/// Path to negentropy trusted relays file
#[arg(long)]
negentropy_relays: Option<PathBuf>,
```

## Challenges & Considerations

### 1. Dedupe Atomicity / Concurrency (Correctness)

This plan assumes the dedupe gate (`check_and_mark_pending()`) is atomic. In the current codebase
it is implemented as a **read followed by a write**, which can race if live ingestion and negentropy
run concurrently.

To prevent double-writes:
- Make `check_and_mark_pending()` a **single critical section** (e.g., internal mutex), *or*
- Serialize all event handling through a single consumer.

### 2. Database Adapter for nostr-sdk

nostr-sdk's `sync_with` method requires a `NostrDatabase` implementation:

```rust
pub trait NostrDatabase: Send + Sync {
    async fn negentropy_items(&self, filter: Filter) -> Result<Vec<(EventId, Timestamp)>>;
    // ... other methods
}
```

We need to implement this trait backed by our sync state storage.

### 3. Cold-Start Sync State Seeding (ClickHouse)

If the sync-state DB is empty, negentropy will treat the local set as empty and can request a very
large delta. We mitigate this with a best-effort seed from ClickHouse (`events_local`) for the last
lookback window (default 7 days) before running the first sync.

### 4. Event Deduplication

Events from negentropy sync must go through the same dedupe pipeline:
- Check RocksDB for duplicates
- Only write novel events to segments
- This is already handled by our event handler

### 5. Rate Limiting

Some relays may rate-limit negentropy requests:
- Implement exponential backoff
- Track relay health for sync operations
- Consider reducing sync frequency if rate limited
- If we see `NEG-ERR: blocked`, shrink scope by **sharding** the sync (smaller time windows and/or kinds)

### 6. Memory Pressure

Large sync windows can discover many missing events:
- Process in batches
- Limit `max_events_per_cycle`
- Monitor memory usage
- If memory/latency is too high, use **partitioned sync** (time buckets, optionally kind buckets) to keep `negentropy_items()` bounded

### 7. Relay Support Detection

Before syncing, verify relay supports NIP-77:
- Try `NEG-OPEN`, expect `NEG-MSG` or `NEG-ERR`
- `NEG-ERR: blocked` means supported but query too big
- No response or `NOTICE` means not supported
- Cache support status per relay

## Success Metrics

Track these metrics to evaluate negentropy sync effectiveness:

```rust
// Prometheus metrics
gauge!("negentropy_last_sync_unix")
counter!("negentropy_syncs_total")
counter!("negentropy_events_discovered_total")
counter!("negentropy_events_fetched_total")
counter!("negentropy_relay_errors_total", "relay" => relay_url, "error" => error_type)
histogram!("negentropy_sync_duration_seconds")
```

## Testing Plan

1. **Unit tests:** Mock nostr-sdk client, verify sync logic
2. **Integration tests:**
   - Stand up test relay with known event set
   - Run sync, verify missing events discovered
3. **Production validation:**
   - Deploy to staging with negentropy enabled
   - Compare event coverage vs live-only ingestion
   - Monitor for duplicate handling

## Open Questions

1. **Should negentropy sync use the same relay connections as live ingestion?**
   - Pro: Simpler, fewer connections
   - Con: May interfere with live event flow
   - Initial plan: Separate client instance

2. **How to handle relays that drop NIP-77 support?**
   - Monitor for `NEG-ERR: blocked` patterns
   - Automatic demotion from trusted list?
   - Alert when trusted relay fails repeatedly

3. **What's the optimal sync window?**
   - Too short: May miss events during downtime
   - Too long: Large sync sets, slow reconciliation
   - Start with 7 days, tune based on metrics

4. **Should we sync specific event kinds only?**
   - Full sync: All kinds
   - Targeted: High-value kinds (1, 6, 7, 30023, etc.)
   - Initial plan: Full sync, can optimize later

## Next Steps

1. [ ] Make dedupe check+mark atomic across live + sync (critical section or single-consumer)
2. [ ] Create basic `NegentropySyncer` struct
3. [ ] Implement `NostrDatabase` adapter for sync state
4. [ ] Add ClickHouse seeding step for last lookback window into sync-state DB
5. [ ] Add CLI arguments for negentropy config
6. [ ] Test with single trusted relay
7. [ ] Add metrics and logging
8. [ ] Expand to full trusted relay list
9. [ ] Document relay NIP-77 support status

