# Pensieve Local Development

Local Docker environment for developing and testing Pensieve.

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

## Grafana Dashboard

A pre-configured "Pensieve Ingestion" dashboard is automatically loaded.

Navigate to: **Dashboards → Pensieve → Pensieve Ingestion**

Panels include:
- Event throughput (received/processed/deduplicated per second)
- Dedupe ratio
- Relay connection counts
- Optimization activity

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
├── grafana/
│   ├── provisioning/
│   │   ├── datasources/    # Auto-configures Prometheus
│   │   └── dashboards/     # Auto-loads dashboards
│   └── dashboards/
│       └── ingestion.json  # Ingestion monitoring dashboard
└── README.md               # This file
```

