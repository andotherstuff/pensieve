# Ingestion pipeline

This doc describes the ingestion architecture for Pensieve, including the options we considered and the decisions we made.

---

## Decisions

### Architecture: Archive-first (Option A)

We're using the **archive-first** approach: the archive is an append-only log (the source of truth), and ClickHouse is a derived index built by consuming that log.

**Why not state-spool (Option B)?**
- State-spool requires an ever-growing DB tracking per-event state across sinks
- It has the same crash ambiguity problems as archive-first, but with more operational complexity
- Archive-first gives us natural buffering during downstream outages and clean rebuild semantics

### Seen-ID index: RocksDB (embedded)

We're using **RocksDB** as an embedded key-value store to track which event IDs we've already archived (for deduplication).

**Why not Redis?**
- Redis is memory-bound. At Nostr scale (billions of events), we'd need 80–100+ GB of RAM just for keys.
- Redis persistence (RDB/AOF) has its own crash semantics and slow restart times for large datasets.
- RocksDB is SSD-backed, handles billions of keys efficiently, and has no network latency (embedded).

**Key properties:**
- Keys: 32-byte event IDs
- Values: empty (existence check only)
- Rebuildable from the archive if ever lost/corrupted
- Tuned for write-heavy workload with bloom filters for fast "not found" lookups

### Segment framing: Length-prefixed

Archive segments use **length-prefixed framing**: `[u32 length][event blob][u32 length][event blob]...`

**Why?**
- Trivial to compute byte offsets for checkpointing
- Easy to skip records without parsing the event
- No ambiguity about record boundaries if corruption occurs
- Can add CRC32 per record later if we want corruption detection

### Event serialization format: notepack

We're using **[notepack](https://docs.rs/notepack/latest/notepack/)** as the binary encoding for individual events.

**Why notepack?**
- Nostr-specific binary format by jb55 (Damus creator)
- Stores 32-byte hex fields (id, pubkey, sig) as raw bytes → ~128 bytes smaller per event
- Streaming parser that yields fields without full deserialization
- Rust-native, version field for forward compatibility
- At scale (billions of events), the ~128 bytes/event savings is significant (~120 GB at 1B events)

**Why not CBOR?**
- CBOR is general-purpose and doesn't optimize for Nostr's specific structure
- The space savings from notepack's Nostr-native encoding are substantial at archive scale

### Checkpoint granularity: Per-sealed-segment

We checkpoint at the **segment level**: when a segment is sealed and synced to remote storage, we record "segment N fully consumed" for ClickHouse indexing.

**Why?**
- Simpler than per-record checkpoints
- Bounds crash recovery to at most one segment's worth of events
- Tail reconciliation on restart queries ClickHouse for event IDs in the uncommitted segment

### Archive partitioning: By ingestion time

Archive segments are partitioned by **ingestion time** (when we validated and accepted the event), not `created_at`.

**Why?**
- Simpler: events go into the current segment regardless of their claimed timestamp
- Avoids edge cases with backdated events, clock skew, and backfills
- `created_at` is still stored in the event and used for ClickHouse time filtering

### Deployment: Single bare-metal server

Initial deployment is a **single bare-metal server** with NVMe storage.

**Why?**
- No virtualization overhead, full control over I/O scheduling
- RocksDB can be tuned aggressively (direct I/O, large block cache, parallel compaction)
- Simplifies architecture: no distributed coordination for the seen-ID index

---

## Background: Options considered

The rest of this doc describes the two architectures we evaluated and their tradeoffs.

## Goals / constraints
- **Correctness**: validate NIP-01 ids + signatures on ingest; invalid events are never stored.
- **Canonical archive**: long-term storage contains canonical events only (no ingestion metadata, no duplicates by event id).
- **Analytics correctness**: prefer **at-most-once insertion into ClickHouse per event id**, because some derived tables/materialized views are not duplicate-safe.
- **Resilience**: ingestion continues through downstream outages (ClickHouse and/or object storage).

## Option A: archive-first (log + checkpoint)

### Mental model
Treat the archive as an **append-only log** (like Kafka). ClickHouse is a **derived index** built by consuming that log.

- Producer: relay ingester
- Log: local append-only segment files (notepack / length-delimited records)
- Remote storage: sync sealed segments to Hetzner Storage Box (via rclone) for durability + rebuilds
- Consumer: ClickHouse indexer
- Checkpoint: (segment_key, offset) stored durably

### How batching to remote storage works with checkpoints
Remote storage objects are immutable; you generally **don't append** to them. Instead:
1. Write events to a **local segment file** (e.g., `archive/2025-12-13/segment-000123.notepack`).
2. Rotate/"seal" the segment on size/time (e.g., 256MB or 1 minute).
3. Sync the sealed segment to remote storage via rclone (that's your batch).

Your checkpoint can be:
- **Object-level**: “segment is fully processed” (simple but coarse), or
- **Record-level**: `(segment_key, record_index)` or `(segment_key, byte_offset)` for crash-safe resumption mid-segment.

Record-level offsets are easy if the segment uses:
- A length-prefix framing (u32 length + notepack bytes), so you can resume from a byte offset cleanly.

### Out-of-order events (created_at vs ingestion order)
In practice, events will arrive “out of order” relative to `created_at` (backfills, relay catch-up, clock skew).

Archive-first handles this cleanly if you **do not require** the archive to be physically ordered by `created_at`.

Recommended default:
- Partition/segment the archive by **ingestion time** (when we validated + accepted the event), not `created_at`.
- Store `created_at` inside the canonical event (it’s already an event field), and let ClickHouse queries use it for time filtering/partitioning.

If you *do* want an archive prefix by `created_at` day/week, still fine:
- A “day” is a **bucket containing many immutable segment objects**, not one file.
- Late events simply go into a **new segment object under that day**, even if earlier segments for that day were already sealed and uploaded.

### The canonical-dedupe requirement
To keep the archive canonical (no duplicates), you still need a **global “seen event id” index**.

Minimal version:
- key: `event_id`
- value: a small bitset (e.g., `ARCHIVED`)

This index is **derived** (rebuildable from the archive), but keeping it online prevents multi-relay duplication from bloating the archive.

### ClickHouse indexing with checkpoints (and crash safety)
The ClickHouse indexer reads the archive log in order and inserts events in batches.

Core loop:
1. Read next N events starting from checkpoint.
2. `INSERT` batch into ClickHouse.
3. Advance checkpoint.

The subtlety: if the process crashes after (2) but before (3), the next run will re-read the same events.

To preserve “at-most-once” without a full per-event ClickHouse state DB, bound the uncertainty to a small window:
- Only ever “replay” the uncommitted tail (e.g., the last 10k–100k events).
- On startup (or before retrying a tail batch), query ClickHouse for ids in that window and **skip those already present**.

This gives you:
- Fast steady-state (no per-event ClickHouse bookkeeping)
- Bounded recovery cost (only check the tail)
- At-most-once inserts in practice, assuming the tail window is bounded and ClickHouse lookups are acceptable for that size.

### When Option A is a good fit
- You want a clean separation: **archive is truth**, ClickHouse is derived.
- You want buffering “for free” by growing the local archive log when ClickHouse is down.
- You want rebuilds/backfills by replaying the archive.

### Where Option A can bite you
- You must be explicit about **framing** (so offsets are meaningful).
- You must design a **tail reconciliation** path for crash ambiguity.
- If the “tail” grows too big during outages, recovery becomes more expensive (mitigated by smaller checkpoints, better backoff, and/or per-segment checkpoints).

## Option B: state-spool (per-event state machine)

### Mental model
Use a durable DB as a work queue keyed by `event_id`, tracking state across sinks:
- `ARCHIVED` (and optionally a pointer to archive location)
- `CLICKHOUSE_INSERTED`
- plus transient “in-flight”/retry metadata

Workers:
- Archive writer consumes validated events and marks `ARCHIVED`.
- ClickHouse writer consumes `ARCHIVED && !CLICKHOUSE_INSERTED` and marks inserted on success.

### Pros
- Very explicit: you can introspect progress and retry logic per event.
- Easy to prioritize/partition work (e.g., by time).

### Cons
- The DB can become very large (it is effectively an ever-growing index of all event ids).
- You still have the same cross-system “crash between insert and state update” ambiguity, which usually requires:
  - a `PENDING` state + reconciliation against ClickHouse, or
  - idempotent insert semantics at the sink.

## Evolution path

If we later need deeper operational introspection ("why is ClickHouse behind?" "what's stuck?"), we can evolve toward Option B-style richer state tracking without changing the archive format. The archive remains the source of truth either way.
