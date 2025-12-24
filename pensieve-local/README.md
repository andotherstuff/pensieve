# Pensieve Local Development

Local Docker environment for developing and testing Pensieve.

## Architecture

- **Docker**: ClickHouse, Prometheus, Grafana (infrastructure)
- **Native binaries**: API server, Ingester (run separately for easier debugging)

## Quick Start

```bash
# From this directory
cd pensieve-local

# Start all services
docker compose up -d

# Check status
docker compose ps

# View logs
docker compose logs -f
```

## Services

| Service | URL | Credentials |
|---------|-----|-------------|
| ClickHouse | http://localhost:8123/play | (none) |
| Prometheus | http://localhost:9090 | (none) |
| Grafana | http://localhost:3000 | `admin` / `admin` |

## Running the API Server

The API runs as a native binary (not in Docker):

```bash
# From project root
export PENSIEVE_API_TOKENS=dev-token
just run-serve
```

Or with all options:

```bash
PENSIEVE_API_TOKENS=dev-token \
CLICKHOUSE_URL=http://localhost:8123 \
RUST_LOG=info,pensieve_serve=debug \
cargo run --bin pensieve-serve
```

### Testing the API

```bash
# Health check (no auth required)
curl http://localhost:8080/health

# Get stats (requires auth)
curl -H "Authorization: Bearer dev-token" \
  http://localhost:8080/api/v1/stats

# List kinds
curl -H "Authorization: Bearer dev-token" \
  http://localhost:8080/api/v1/kinds
```

See `crates/pensieve-serve/README.md` for full API documentation.

## Running the Ingester

From the project root:

```bash
# Build
cargo build --package pensieve-ingest

# Create data directories
mkdir -p ./data/dedupe ./data/segments

# Run with local services
./target/debug/pensieve-ingest \
  --max-relays 20 \
  --seed-file ./data/relays/seed.txt \
  --output ./data/segments \
  --rocksdb-path ./data/dedupe \
  --relay-db-path ./data/relay-stats.db \
  --clickhouse-url http://localhost:8123 \
  --metrics-port 9091 \
  --score-interval-secs 60
```

## Grafana Dashboards

Pre-configured dashboards are automatically loaded:

### Pensieve Ingestion
Navigate to: **Dashboards → Pensieve → Pensieve Ingestion**

Panels include:
- Event throughput (received/processed/deduplicated per second)
- Dedupe ratio
- Relay connection counts
- Optimization activity

### Pensieve Backfill
Navigate to: **Dashboards → Pensieve → Pensieve Backfill**

For monitoring `backfill-jsonl` and `backfill-proto` jobs. Panels include:
- Status (Running/Stopped)
- Valid/Duplicate/Invalid event counts
- Processing rate (events/sec)
- Throughput (bytes/sec)
- Segments sealed
- Data size comparison (input vs output)
- Efficiency metrics (duplicate rate, invalid rate, compression savings)

**Note:** Backfill jobs must be run with `--metrics-port` to export metrics.

## Cleanup

```bash
# Stop services
docker compose down

# Stop and remove volumes (full reset)
docker compose down -v
```

## Directory Structure

```
pensieve-local/
├── docker-compose.yml      # Main compose file
├── prometheus.yml          # Prometheus scrape config
├── clickhouse/             # ClickHouse config
├── grafana/
│   ├── provisioning/
│   │   ├── datasources/    # Auto-configures Prometheus + ClickHouse
│   │   └── dashboards/     # Auto-loads dashboards
│   └── dashboards/
│       ├── ingestion.json  # Live ingestion monitoring
│       └── backfill.json   # Backfill job monitoring
└── README.md               # This file
```

## Why Native Binaries?

The Rust services (API, Ingester) run as native binaries rather than in Docker:
- **Faster iteration**: No container rebuild needed
- **Easier debugging**: Attach debugger directly, check logs in terminal
- **Better I/O**: Direct filesystem access for RocksDB/segments

In production, systemd manages these binaries. Locally, just run them in your terminal.

