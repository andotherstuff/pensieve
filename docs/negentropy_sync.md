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
- RocksDB dedupe index doesn't support concurrent writers
- Single process simplifies coordination between live and sync operations
- Memory pressure is manageable with careful batching

### Component Flow

```
┌─────────────────────────────────────────────────────────────────────┐
│                         pensieve-ingest                              │
├─────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  ┌──────────────────┐       ┌──────────────────┐                    │
│  │   Live Ingester  │       │  Negentropy Sync │                    │
│  │   (RelaySource)  │       │     (Periodic)   │                    │
│  │                  │       │                  │                    │
│  │ • Stream events  │       │ • Sync every N   │                    │
│  │ • All relays     │       │   minutes        │                    │
│  │ • Discovery      │       │ • Trusted relays │                    │
│  │                  │       │   only           │                    │
│  └────────┬─────────┘       └────────┬─────────┘                    │
│           │                          │                              │
│           │     Events               │     Event IDs                │
│           ▼                          ▼                              │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                      Event Handler                            │   │
│  │                                                               │   │
│  │  1. Dedupe check (RocksDB)                                    │   │
│  │  2. Pack to notepack                                          │   │
│  │  3. Write to segment                                          │   │
│  │  4. Record relay quality                                      │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│                              ▼                                      │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                    SegmentWriter                              │   │
│  │                                                               │   │
│  │  • Write to current segment                                   │   │
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

### How It Works

1. Client builds a filter for events to sync (e.g., all events since X)
2. Client calls `sync_with(trusted_relays, filter, opts)` with `direction = Down`
3. nostr-sdk:
   - Queries local database for matching event IDs + timestamps
   - Sends negentropy `NEG-OPEN` with initial fingerprint
   - Exchanges `NEG-MSG` until reconciliation complete
   - Returns which event IDs the relay has that we don't
4. For each missing event ID, fetch the actual event via `REQ`
5. Process through the normal ingestion pipeline

## Trusted Relays for Negentropy

Not all relays support NIP-77. We need a curated list of relays that:
1. **Support NIP-77** negentropy protocol
2. **Have comprehensive archives** (store lots of historical data)
3. **Are reliable** (good uptime, low latency)

### Suggested Initial List

```txt
# Known NIP-77 relays with good archives
wss://relay.damus.io
wss://relay.nostr.band
wss://purplepag.es
wss://relay.primal.net
wss://nostr.wine
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
        // 1. Build filter: all events in lookback window
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

### Phase 3: Database Requirements

The nostr-sdk negentropy requires a local database to compare against. Options:

1. **Use RocksDB dedupe index** 
   - Already have event IDs
   - Need to add timestamps
   - Implement `NostrDatabase` trait adapter

2. **Add separate LMDB/SQLite for sync state**
   - Store (event_id, created_at) pairs
   - Only for negentropy-relevant window
   - Cleaner separation of concerns

**Recommendation:** Option 2 - Add a small SQLite database for sync state. This:
- Keeps the dedupe index focused on its purpose
- Allows independent sync window management
- Easier to reset/rebuild if needed

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

### 1. Database Adapter for nostr-sdk

nostr-sdk's `sync_with` method requires a `NostrDatabase` implementation:

```rust
pub trait NostrDatabase: Send + Sync {
    async fn negentropy_items(&self, filter: Filter) -> Result<Vec<(EventId, Timestamp)>>;
    // ... other methods
}
```

We need to implement this trait backed by our sync state storage.

### 2. Event Deduplication

Events from negentropy sync must go through the same dedupe pipeline:
- Check RocksDB for duplicates
- Only write novel events to segments
- This is already handled by our event handler

### 3. Rate Limiting

Some relays may rate-limit negentropy requests:
- Implement exponential backoff
- Track relay health for sync operations
- Consider reducing sync frequency if rate limited

### 4. Memory Pressure

Large sync windows can discover many missing events:
- Process in batches
- Limit `max_events_per_cycle`
- Monitor memory usage

### 5. Relay Support Detection

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

1. [ ] Create basic `NegentropySyncer` struct
2. [ ] Implement `NostrDatabase` adapter for sync state
3. [ ] Add CLI arguments for negentropy config
4. [ ] Test with single trusted relay
5. [ ] Add metrics and logging
6. [ ] Expand to full trusted relay list
7. [ ] Document relay NIP-77 support status

