# Pensieve

Archive, explore, and analyze Nostr data at network scale.

## Overview

Pensieve is an **archive-first** Nostr indexer. It stores canonical events in a local notepack archive (source of truth), with ClickHouse as a derived analytics index.

```
┌─────────────────────────────────────────────────────────────────┐
│                        Event Sources                            │
│  ┌───────────┐    ┌───────────┐    ┌────────────────────────┐  │
│  │   JSONL   │    │ Protobuf  │    │     Live Relays        │  │
│  │   Files   │    │  Archives │    │ (WebSocket + NIP-65)   │  │
│  └─────┬─────┘    └─────┬─────┘    └───────────┬────────────┘  │
└────────┼────────────────┼──────────────────────┼───────────────┘
         │                │                      │
         └────────────────┴──────────────────────┘
                          │
                          ▼
                  ┌───────────────┐
                  │  DedupeIndex  │  RocksDB - tracks seen event IDs
                  └───────┬───────┘
                          │
                          ▼
                  ┌───────────────┐
                  │ SegmentWriter │  Notepack segments (gzipped)
                  └───────┬───────┘
                          │
              ┌───────────┴───────────┐
              │                       │
              ▼                       ▼
      ┌───────────────┐       ┌───────────────┐
      │ Storage Box   │       │  ClickHouse   │
      │   (rclone)    │       │    Index      │
      └───────────────┘       └───────────────┘
```

## Current Status

| Component | Status |
|-----------|--------|
| `pensieve-core` | ✅ Event validation, notepack encoding, metrics |
| `pensieve-ingest` | ✅ Live relay ingestion + backfill binaries |
| `pensieve-serve` | 🚧 Placeholder |
| Deployment | ✅ Docker + systemd setup in `pensieve-deploy/` |

## Ingestion Modes

Pensieve supports three event sources, all feeding into the same pipeline:

### 1. Live Relay Ingestion (Real-time)

Connect to Nostr relays via WebSocket and stream events in real-time. Includes automatic relay discovery via NIP-65.

```bash
# Basic usage (connects to default seed relays)
./target/release/pensieve-ingest \
  -o ./segments \
  --rocksdb-path ./data/dedupe

# Full options
./target/release/pensieve-ingest \
  -o ./segments \
  --rocksdb-path ./data/dedupe \
  --clickhouse-url http://localhost:8123 \
  --seed-relays "wss://relay.damus.io,wss://nos.lol,wss://relay.primal.net" \
  --max-relays 50 \
  --metrics-port 9090
```

**Features:**
- Generates ephemeral keypair for NIP-42 authentication
- Auto-discovers relays via NIP-65 (kind:10002 events)
- Disconnects from paid/whitelist relays that reject auth
- Graceful shutdown on Ctrl+C

**Options:**
| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `./segments` | Output directory for notepack segments |
| `--rocksdb-path` | `./data/dedupe` | RocksDB path for deduplication |
| `--clickhouse-url` | (none) | ClickHouse URL for indexing |
| `--clickhouse-db` | `nostr` | ClickHouse database name |
| `--seed-relays` | 8 public relays | Comma-separated relay URLs |
| `--max-relays` | `100` | Maximum concurrent relay connections |
| `--no-discovery` | `false` | Disable NIP-65 relay discovery |
| `--segment-size` | `268435456` | Max segment size before sealing (256MB) |
| `--no-compress` | `false` | Disable gzip compression |
| `--metrics-port` | `9090` | Prometheus metrics port (0 to disable) |

### 2. JSONL Backfill (Batch)

Import events from JSONL files (one JSON event per line). Useful for importing strfry dumps or other JSON exports.

```bash
# Single file
./target/release/backfill-jsonl \
  -i events.jsonl \
  -o ./segments

# Directory of files
./target/release/backfill-jsonl \
  -i ./jsonl-data/ \
  -o ./segments \
  --rocksdb-path ./data/dedupe \
  --clickhouse-url http://localhost:8123

# Fast mode (skip validation, trust input)
./target/release/backfill-jsonl \
  -i ./jsonl-data/ \
  -o ./segments \
  --skip-validation
```

**Options:**
| Flag | Default | Description |
|------|---------|-------------|
| `-i, --input` | (required) | Input JSONL file or directory |
| `-o, --output` | (required) | Output directory for segments |
| `--rocksdb-path` | (none) | RocksDB path for deduplication |
| `--clickhouse-url` | (none) | ClickHouse URL for indexing |
| `--skip-validation` | `false` | Skip ID/signature verification |
| `--limit` | (none) | Limit number of files to process |
| `--progress-interval` | `100000` | Log progress every N events |
| `--metrics-port` | `9091` | Prometheus metrics port |

### 3. Protobuf Backfill (Batch)

Import events from length-delimited protobuf files. Supports local files or S3 with resumable progress.

```bash
# Local files
./target/release/backfill-proto \
  -i ./proto-segments/ \
  -o ./segments \
  --rocksdb-path ./data/dedupe

# S3 source with resume support
./target/release/backfill-proto \
  --s3-bucket your-bucket \
  --s3-prefix nostr/segments/ \
  -o ./segments \
  --rocksdb-path ./data/dedupe \
  --clickhouse-url http://localhost:8123

# Limit for testing
./target/release/backfill-proto \
  --s3-bucket your-bucket \
  --s3-prefix nostr/segments/ \
  -o ./segments \
  --limit 5
```

**Options:**
| Flag | Default | Description |
|------|---------|-------------|
| `-i, --input` | (local) | Input protobuf file or directory |
| `--s3-bucket` | (S3) | S3 bucket name |
| `--s3-prefix` | (S3) | S3 key prefix |
| `-o, --output` | (required) | Output directory for segments |
| `--rocksdb-path` | (none) | RocksDB path for deduplication |
| `--clickhouse-url` | (none) | ClickHouse URL for indexing |
| `--gzip` | auto | Force gzip decompression |
| `--skip-validation` | `false` | Skip ID/signature verification |
| `--progress-file` | auto | Path for S3 resume progress |
| `--temp-dir` | system | Temp directory for S3 downloads |
| `--metrics-port` | `9091` | Prometheus metrics port |

## Quick Start

```bash
# Build all binaries
cargo build --release

# Start live ingestion (simplest form)
./target/release/pensieve-ingest -o ./segments --rocksdb-path ./data/dedupe

# Or run a backfill from local JSONL files
./target/release/backfill-jsonl -i ./events.jsonl -o ./segments
```

## Pipeline Components

All ingestion modes use the same core pipeline:

- **DedupeIndex** (RocksDB) — Tracks seen event IDs to prevent duplicates
- **SegmentWriter** — Writes events to gzipped notepack segments, seals at size threshold
- **ClickHouseIndexer** — Indexes sealed segments into ClickHouse for analytics

## Deployment

See [`pensieve-deploy/README.md`](pensieve-deploy/README.md) for production setup with Docker, ClickHouse, Prometheus, and Grafana.

## Documentation

- [`docs/ingestion_pipeline.md`](docs/ingestion_pipeline.md) — Pipeline architecture and design decisions
- [`docs/clickhouse_self_hosted.sql`](docs/clickhouse_self_hosted.sql) — ClickHouse schema

## License

See [LICENSE](LICENSE).
