# AGENTS.md

This file provides context for AI coding agents working on the Pensieve codebase.

## Project Overview

Pensieve is an **archive-first Nostr indexer** written in Rust. It ingests events from Nostr relays, stores them in a canonical notepack archive (source of truth), and indexes them into ClickHouse for analytics.

**Key architecture decisions:**
- Archive-first: notepack segments are the source of truth; ClickHouse is a derived index
- RocksDB for deduplication (billions of event IDs, SSD-backed)
- Length-prefixed framing for segment files
- Notepack binary format for ~128 bytes/event savings over JSON

## Quick Reference

### Build & Development Commands

All commands use `just` (see `justfile`):

```bash
# Build
just build              # Debug build
just build-release      # Release build

# Check & Lint
just check              # Compile check only
just clippy             # Run clippy (strict: -D warnings)
just fmt                # Format code
just fmt-check          # Check formatting without modifying

# Test
just test               # Run all tests
just test-verbose       # Run tests with output

# Precommit (run before committing)
just precommit          # Runs fmt, clippy, test

# Run binaries (debug mode)
just run-ingest         # Live relay ingestion
just run-serve          # Serve API (placeholder)
just run-backfill-jsonl # JSONL backfill
just run-backfill-proto # Protobuf backfill

# Local dev environment
just dev-up             # Start Docker services (ClickHouse, Prometheus, Grafana)
just dev-down           # Stop Docker services
just dev-logs           # View Docker logs
```

### ClickHouse Commands

```bash
just ch-init                  # Initialize schema from docs/clickhouse_self_hosted.sql
just ch-migrate <file>        # Run a single migration
just ch-migrate-all           # Run all migrations in docs/migrations/
just ch-query "<sql>"         # Run raw SQL query
just ch-tables                # List tables and views
```

## Project Structure

```
pensieve/
├── crates/
│   ├── pensieve-core/       # Core types, validation, notepack encoding, metrics
│   ├── pensieve-ingest/     # Ingestion pipeline (relay source, dedupe, segments, ClickHouse)
│   │   └── src/
│   │       ├── bin/         # CLI binaries (backfill-jsonl, backfill-proto)
│   │       ├── pipeline/    # Core pipeline: dedupe.rs, segment.rs, clickhouse.rs
│   │       ├── relay/       # Relay quality tracking and management
│   │       └── source/      # Event sources: relay.rs, jsonl.rs, proto.rs
│   └── pensieve-serve/      # API server (placeholder)
├── docs/
│   ├── clickhouse_self_hosted.sql  # Full ClickHouse schema
│   ├── migrations/                  # Incremental schema migrations
│   └── ingestion_pipeline.md        # Architecture decisions
├── pensieve-local/          # Local dev Docker Compose + Grafana dashboards
├── pensieve-deploy/         # Production deployment configs
├── data/                    # Local data directories (gitignored)
│   ├── dedupe/              # RocksDB dedupe index
│   ├── relays/              # Relay discovery data
│   └── segments/            # Notepack segment files
└── justfile                 # Task runner commands
```

## Code Style & Conventions

### Rust Configuration
- **Edition**: 2024
- **Minimum Rust version**: 1.90
- **Formatting**: Default `cargo fmt` (no custom `.rustfmt.toml`)
- **Linting**: `cargo clippy -- -D warnings` (all warnings are errors)

### Coding Patterns

Review [rust-code-style.mdc](./.cursor/rules/rust-code-style.mdc) for full code style details.

### Documentation

- All public items should have doc comments (`///` or `//!`)
- Module-level docs at the top of each file explaining purpose
- Follow the existing pattern in `crates/pensieve-core/src/lib.rs`

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `nostr`, `nostr-sdk` | Nostr protocol types and relay connectivity |
| `notepack` | Binary encoding format for Nostr events |
| `rocksdb` | Embedded key-value store for dedupe index |
| `clickhouse` | ClickHouse client for analytics indexing |
| `tokio` | Async runtime |
| `axum` | HTTP server framework (for pensieve-serve) |
| `tracing` | Structured logging |
| `metrics` + `metrics-exporter-prometheus` | Metrics export |
| `clap` | CLI argument parsing |

## Domain Knowledge

### Nostr Concepts
- **Event**: The fundamental data unit (has `id`, `pubkey`, `kind`, `content`, `tags`, `sig`, `created_at`)
- **NIP**: Nostr Implementation Possibility (protocol spec documents)
- **NIP-01**: Basic protocol (event structure, signatures)
- **NIP-42**: Client authentication
- **NIP-65**: Relay list metadata (used for relay discovery)
- **Event kinds**: Integers identifying event types (e.g., 1 = text note, 10002 = relay list)

### Notepack Format
- Binary encoding by jb55 (Damus creator)
- Stores hex fields (id, pubkey, sig) as raw 32/64 bytes
- ~128 bytes smaller per event than JSON
- Streaming parser for efficient reading

### Pipeline Flow
1. **Source** (relay/JSONL/protobuf) → validated events
2. **DedupeIndex** (RocksDB) → filter duplicates by event ID
3. **SegmentWriter** → append to gzipped notepack segment
4. **ClickHouseIndexer** → index sealed segments for analytics

### Segment Lifecycle
- Events written to current segment with length-prefix framing
- Segment sealed when reaching size threshold (default 256MB)
- Sealed segments synced to remote storage and indexed to ClickHouse

## Testing

```bash
# Run all tests
just test

# Run with output visible
just test-verbose

# Run specific test
cargo test -p pensieve-core test_name
```

Currently, test coverage is focused on core validation and encoding logic in `pensieve-core`.

## Common Tasks

### Adding a new CLI flag
1. Add the field to the `Args` struct in the relevant binary's `main.rs`
2. Use `#[arg(...)]` attributes from clap
3. Update the README.md if it's a user-facing flag

### Adding a new metric
1. Use `gauge!()`, `counter!()`, or `histogram!()` from the `metrics` crate
2. Follow naming convention: `<component>_<metric_name>` (e.g., `ingest_events_received_total`)
3. Register in the metrics initialization if using labels

### Modifying ClickHouse schema
1. Add a migration file in `docs/migrations/` (e.g., `002_description.sql`)
2. Ensure idempotency (use `IF NOT EXISTS`, `CREATE OR REPLACE`, etc.)
3. Run with `just ch-migrate docs/migrations/002_description.sql`

### Working with the dedupe index
- Keys: 32-byte event IDs
- Values: empty (existence check only)
- Use `check_and_mark_pending()` before writing, `mark_archived()` after sealing

## Security Notes

- Events are validated (ID + signature) per NIP-01 before archiving
- Invalid events are never stored
- The daemon generates ephemeral keypairs for NIP-42 relay authentication

## License

PolyForm Noncommercial License 1.0.0 - see `LICENSE` file.

