# Pensieve

Archive, explore, and analyze Nostr data at network scale.

## Overview

Pensieve is an **archive-first** Nostr indexer. It stores canonical events in a local notepack archive (source of truth), with ClickHouse as a derived analytics index.

```
[Source Adapters] → [DedupeIndex] → [SegmentWriter] → [ClickHouseIndexer]
                         ↓                ↓
                     RocksDB        Storage Box (rclone)
```

## Current Status

| Component | Status |
|-----------|--------|
| `pensieve-core` | ✅ Event validation, notepack encoding, metrics |
| `pensieve-ingest` | ⚠️ Backfill binaries ready; real-time relay ingester not yet implemented |
| `pensieve-serve` | 🚧 Placeholder |
| Deployment | ✅ Docker + systemd setup in `pensieve-deploy/` |

### Available Binaries

- **`backfill-proto`** — Import from protobuf dumps (e.g., previously created archives)
- **`backfill-jsonl`** — Import from JSONL exports (e.g., strfry dumps)

### Not Yet Implemented

- Real-time relay ingester (WebSocket connections to relays)
- HTTP API for analytics queries

## Quick Start

```bash
# Build
cargo build --release

# Run backfill from S3
./target/release/backfill-proto \
  --s3-bucket your-bucket \
  --s3-prefix nostr/segments/ \
  -o /archive/segments \
  --rocksdb-path /data/rocksdb \
  --clickhouse-url http://localhost:8123
```

## Deployment

See [`pensieve-deploy/README.md`](pensieve-deploy/README.md) for production setup with Docker, ClickHouse, Prometheus, and Grafana.

## Documentation

- [`docs/ingestion_pipeline.md`](docs/ingestion_pipeline.md) — Pipeline architecture
- [`docs/clickhouse_self_hosted.sql`](docs/clickhouse_self_hosted.sql) — ClickHouse schema

## License

See [LICENSE](LICENSE).

