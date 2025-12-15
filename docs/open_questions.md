# Open questions / design notes

This file tracks unresolved design questions and tradeoffs. It is intentionally short and high-signal.

## Coverage / relay set management
- Proxy signals (for now): **event-rate stability** and **relay overlap** (diminishing returns / uniqueness).
- Relay policy: keep connecting to relays that continue to yield **novel event ids**; apply exponential backoff for relays that go quiet, and periodically re-check low-volume relays.
- Open: how do we compute/track relay overlap cheaply enough to drive decisions?

## ClickHouse correctness with duplicate inserts
- `events_local` deduplication by `id` does **not** automatically make downstream materialized views duplicate-safe.
- Decision: prefer **at-most-once insertion into ClickHouse per event id** (Option A).
- Open:
  - What durable store do we use for a global “seen event id” index (SQLite/LMDB/RocksDB/etc.) and what are its operational limits at very large scale?
  - How do we make “buffering/spooling + at-most-once” safe across crashes when writing to multiple sinks (archive + ClickHouse)?
  - Defense-in-depth: do we also want some duplicate-tolerant derived tables/queries (e.g., `countDistinct`) for safety?
  - See also: `docs/ingestion_pipeline.md` (archive-first vs state-spool).

## Deletions (NIP-09) for analytics
- Decision: prefer a join/index table derived from deletion events so queries can optionally filter out deleted content without mutating the base events table.
- Semantics (so far):
  - “Replies to event X” / “does event X have replies?” should filter out **deleted replies** and treat **deleted parents** as “no results”.
  - Global rollups (e.g., “total replies/day”) do not necessarily need to account for parent deletion.
- Note: pre-aggregated `SummingMergeTree` counters (like `comment_counts`) can’t “subtract” deletions; deletion-aware counts usually require per-event relationship tables (reply_event_id → parent_event_id) or query-time filtering against base events + `deleted_event_ids`.
- Open: do we need to also index deletions of addressable events (`a` tags), or is `e`-only enough for now?

## Archive integrity / verification
- The archive stores canonical Nostr events only (no ingestion metadata).
- Should we add shard-level integrity features later (format version headers, checksums per shard/block, corruption detection strategy)?

