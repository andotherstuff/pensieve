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
| `pensieve-ingest` | ✅ Live relay ingestion + backfill binaries + relay quality tracking |
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
- **Relay quality tracking** (SQLite) with scoring and optimization
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
| `--relay-db-path` | `./data/relay-stats.db` | SQLite path for relay quality tracking |
| `--import-relays-csv` | (none) | Import relays from georelays CSV |
| `--score-interval-secs` | `300` | Score recomputation interval |

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
- **RelayManager** (SQLite) — Tracks per-relay quality metrics and optimizes connections

## Relay Quality Tracking

The ingestion daemon tracks per-relay quality metrics to prioritize high-value relays:

**Metrics tracked:**
- **Novel event rate** — Events/hour that passed deduplication (not seen before)
- **Uptime** — Connection success rate over time
- **Connection history** — Attempts, successes, failures

**Scoring:**
```
score = (novel_rate_normalized × 0.7) + (uptime × 0.3)
```

Relays are ranked by score. Seed relays (manually curated) get a floor score of 0.5 to prevent eviction.

**Slot optimization:**
- Every 5 minutes (configurable), scores are recomputed
- Up to 5% of relay slots can be swapped per cycle
- Low-scoring discovered relays are replaced with higher-scoring unconnected ones
- Relays with 10+ consecutive failures are blocked

**Bootstrap from georelays:**

You can import relay lists from the [georelays](https://github.com/permissionlesstech/georelays) project:

```bash
# Download the georelays CSV
curl -o data/relays/georelays.csv \
  https://raw.githubusercontent.com/permissionlesstech/georelays/main/nostr_relays.csv

# Run with import
./target/release/pensieve-ingest \
  --import-relays-csv ./data/relays/georelays.csv \
  -o ./segments
```

**Query relay stats:**

```bash
sqlite3 ./data/relay-stats.db "
  SELECT url, score, novel_rate_7d, uptime_7d
  FROM relays r
  JOIN relay_scores s ON r.url = s.relay_url
  ORDER BY score DESC
  LIMIT 20;
"
```

## Deployment

See [`pensieve-deploy/README.md`](pensieve-deploy/README.md) for production setup with Docker, ClickHouse, Prometheus, and Grafana.

## Documentation

- [`docs/ingestion_pipeline.md`](docs/ingestion_pipeline.md) — Pipeline architecture and design decisions
- [`docs/clickhouse_self_hosted.sql`](docs/clickhouse_self_hosted.sql) — ClickHouse schema

## License

See [LICENSE](LICENSE).
