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
| `pensieve-ingest` | ✅ Live relay ingestion + optional inventoried Parquet shadow sink |
| `pensieve-negentropy-worker` | Experimental one-job isolated worker; `--socket PATH --parent-uid UID`. The parent scheduler runs behind `pensieve-ingest --isolated-negentropy` (not enabled by default); unit in `ops/systemd/pensieve-negentropy-worker.service`. See [worker contract](docs/negentropy_worker.md) and [runtime](docs/negentropy_runtime.md). |
| `pensieve-parquet` | 🚧 Accepted V1 writer, strict validator, fixtures, and notepack converter prototype |
| `pensieve-lake` | 🚧 Resumable notepack campaign, SQLite inventory, and immutable local/S3 publication |
| `pensieve-serve` | ✅ Authenticated analytics HTTP API (~30 routes, served from ClickHouse). See [API docs](crates/pensieve-serve/README.md) |
| `pensieve-preview` | ✅ Static HTML preview pages for Nostr events/profiles, plus the `preview-lookups` compact-lookup operator tool |
| `pensieve-analytics` | 🚧 DuckDB batch tools that build exact analytics from Parquet snapshots into local DuckDB databases and evidence-checked artifacts (research tooling; not served by the API) |
| Deployment | ✅ Docker + systemd setup in `ops/` |

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
  --metrics-port 9091
```

**Features:**
- Generates ephemeral keypair for NIP-42 authentication
- Auto-discovers relays via NIP-65 (kind:10002 events)
- Disconnects from paid/whitelist relays that reject auth
- **Relay quality tracking** (SQLite) with scoring and optimization
- Optional Parquet shadow publication from durably sealed notepack work units
- Graceful shutdown on Ctrl+C

Live archive segments are periodically sealed independently of Parquet. Configure
`--archive-seal-interval-secs` / `PENSIEVE_ARCHIVE_SEAL_INTERVAL_SECS` (default 300).
Zero disables this archive timer. An active Parquet shadow's nonzero
`PENSIEVE_PARQUET_SHADOW_MAX_BATCH_AGE_SECS` can request a shorter cadence; disabling
that legacy timer alone no longer disables archive sealing. Set both to zero to
disable periodic sealing entirely (not suitable for isolated reconciliation).
The timer performs one synchronous seal at a time and is joined at shutdown;
its cadence is not a hard wall-clock bound on slow disk operations.

**Options:**
| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output-dir` | `./segments` | Output directory for notepack segments |
| `--rocksdb-path` | `./data/dedupe` | RocksDB path for deduplication |
| `--clickhouse-url` | (none) | ClickHouse URL for indexing |
| `--clickhouse-db` | `nostr` | ClickHouse database name |
| `--seed-relays` | (none) | Comma-separated seed relay URLs; overrides the seed file |
| `--seed-file` | `./data/relays/seed.txt` | Seed relay file (one URL per line, `#` comments); if absent or empty, 10 built-in public relays are used |
| `--max-relays` | `30` | Maximum concurrent relay connections |
| `--no-discovery` | `false` | Disable NIP-65 relay discovery |
| `--segment-size` | `268435456` | Max segment size before sealing (256MB) |
| `--archive-seal-interval-secs` | `300` | Archive seal cadence independent of Parquet; zero withdraws this cadence request |
| `--no-compress` | `false` | Disable gzip compression |
| `--metrics-port` | `9091` | Prometheus metrics port (0 to disable) |
| `--relay-db-path` | `./data/relay-stats.db` | SQLite path for relay quality tracking |
| `--import-discovered-json` | (none) | Import relays from a discovery JSON file as `discovered` tier |
| `--score-interval-secs` | `300` | Score recomputation interval |

This table covers the basics; run `pensieve-ingest --help` (or `just run-ingest --help`)
for the full list. Other flags used in production include `--tor-proxy` (SOCKS5 for
`.onion` relays), `--nip66-monitor-relays`, `--negentropy` and the
`--isolated-negentropy*` family, `--blocked-giftwrap-recipients`
(env `PENSIEVE_BLOCKED_GIFTWRAP_RECIPIENTS`), `--archive-seal-interval-secs`
(env `PENSIEVE_ARCHIVE_SEAL_INTERVAL_SECS`), and the `--parquet-shadow-*` family
(env `PENSIEVE_PARQUET_SHADOW_*`; see [`ops/parquet-shadow.env.example`](ops/parquet-shadow.env.example)).

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
| `-i, --input` | (required unless `--index-segments`) | Input JSONL file or directory |
| `-o, --output` | (required unless `--index-segments`) | Output directory for segments |
| `--rocksdb-path` | (none) | RocksDB path for deduplication |
| `--clickhouse-url` | (none) | ClickHouse URL for indexing |
| `--skip-validation` | `false` | Skip ID/signature verification |
| `--limit` | (none) | Limit number of files to process |
| `--progress-interval` | `100000` | Log progress every N events |
| `--metrics-port` | `9091` | Prometheus metrics port |

See `backfill-jsonl --help` for the remaining flags (e.g. `--index-segments`, `--progress-file`, `--no-compress`).

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

**Bootstrap from a relay discovery file:**

`--import-discovered-json` imports relays as `discovered` tier (eligible for swapping).
The file must be a JSON object with a `functioning_relays` array of relay URL strings;
URLs are normalized, and invalid or blocked URLs are skipped.

```json
{ "functioning_relays": ["wss://relay.example.com", "wss://nostr.example.org"] }
```

```bash
./target/release/pensieve-ingest \
  --import-discovered-json ./data/relays/relay_discovery_results.json \
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

## Configuration

Environment variables are documented in example files:

| File | Use | Copy to |
|------|-----|---------|
| [`env.local.example`](env.local.example) | Local native dev for `pensieve-serve` / `pensieve-preview` | `.env` (repo root) |
| [`env.production.example`](env.production.example) | Production Docker + systemd stack | `/etc/pensieve/pensieve.env` |
| [`ops/parquet-shadow.env.example`](ops/parquet-shadow.env.example) | `pensieve-ingest` Parquet shadow settings | `/etc/pensieve/parquet-shadow.env` |
| [`ops/parquet-campaign.env.example`](ops/parquet-campaign.env.example) | Parquet campaign service | `/etc/pensieve/parquet-campaign.env` |
| [`ops/analytics.env.example`](ops/analytics.env.example) | DuckDB analytics batch tools | `/etc/pensieve/analytics.env` |

Only `pensieve-serve` and `pensieve-preview` load a `.env` file (default `.env`; override
with `--dotenv <path>` or `DOTENV_PATH`). `pensieve-ingest` never loads `.env`: it is
configured with CLI flags, and its env-aliased flags are read from the process
environment only (e.g. a systemd `EnvironmentFile`).

## Deployment

See [`ops/RUNBOOK.md`](ops/RUNBOOK.md) for production setup and the deploy/cutover procedure.

## Documentation

- [`docs/ingestion_pipeline.md`](docs/ingestion_pipeline.md) — Pipeline architecture and design decisions
- [`docs/clickhouse_self_hosted.sql`](docs/clickhouse_self_hosted.sql) — Base ClickHouse schema for fresh installs (not a production snapshot)
- [`docs/migrations/README.md`](docs/migrations/README.md) — ClickHouse migrations and production schema state
- [`docs/migration-plan.md`](docs/migration-plan.md) — Lakehouse migration plan (its Postgres serving store was abandoned on 2026-10-12)

## License

See [LICENSE](LICENSE).
