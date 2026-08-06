# Pensieve - Nostr Archive & Analytics
# Run `just` to see available recipes

# Default recipe - show available commands
default:
    @just --list

# ============================================================================
# Development
# ============================================================================

# Run all precommit checks (fmt, clippy, test)
precommit: fmt clippy test
    @echo "✓ All precommit checks passed"

# Check code compiles without building
check:
    cargo check --workspace --all-targets

# Run clippy lints
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Format code
fmt:
    cargo fmt --all

# Check formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# Run tests
test:
    cargo test --workspace

# Run tests with output
test-verbose:
    cargo test --workspace -- --nocapture

# Regenerate the checked-in canonical Parquet interoperability corpus.
parquet-fixtures:
    cargo run -p pensieve-parquet --example generate_parquet_fixtures

# Read the canonical fixture with independent PyArrow and DuckDB implementations.
parquet-interop:
    uv run scripts/verify_parquet_interop.py

# Run the resumable sealed-notepack publication campaign.
parquet-campaign *ARGS:
    cargo run -p pensieve-lake --bin pensieve-parquet-campaign -- {{ARGS}}

# Freeze, inspect, or audit a bounded historical-source manifest.
source-manifest *ARGS:
    cargo run -p pensieve-lake --bin pensieve-source-manifest -- {{ARGS}}

# Recover exact event IDs from an operator-supplied relay set.
recover-events *ARGS:
    cargo run -p pensieve-ingest --bin recover-events -- {{ARGS}}

# Preserve the complete prefix and terminal evidence of a truncated notepack.
salvage-notepack *ARGS:
    cargo run -p pensieve-parquet --bin pensieve-notepack-salvage -- {{ARGS}}

# Export, merge, or verify deterministic active-file lake catalogs.
lake-catalog *ARGS:
    cargo run -p pensieve-lake --bin pensieve-lake-catalog -- {{ARGS}}

# Build and optionally publish exact Slice A analytics.
analytics-build *ARGS:
    cargo run -p pensieve-analytics -- {{ARGS}}

# Plan the object delta from the currently published analytics run.
analytics-plan *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics-plan -- {{ARGS}}

# Apply one verified append-only analytics delta.
analytics-incremental *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics-incremental -- {{ARGS}}

# Compare current Postgres Slice A products with deduplicated ClickHouse values.
analytics-compare *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics-compare -- {{ARGS}}

# ============================================================================
# Build
# ============================================================================

# Build debug binaries
build:
    cargo build --workspace

# Build release binaries
build-release:
    cargo build --workspace --release

# Build and show binary sizes
build-release-sizes: build-release
    @echo "\nBinary sizes:"
    @ls -lh target/release/pensieve-ingest target/release/pensieve-serve target/release/pensieve-preview target/release/backfill-jsonl target/release/backfill-proto 2>/dev/null || true

# Clean build artifacts
clean:
    cargo clean

clean-data:
    rm -rf data/dedupe data/segments data/relay-stats.db
    mkdir -p data/dedupe data/segments

# ============================================================================
# Run
# ============================================================================

# Run the ingester (debug)
run-ingest *ARGS:
    cargo run --bin pensieve-ingest -- {{ARGS}}

# Run the serve API (debug)
run-serve *ARGS:
    cargo run --bin pensieve-serve -- {{ARGS}}

# Run the JSONL backfill tool (debug)
run-backfill-jsonl *ARGS:
    cargo run --bin backfill-jsonl -- {{ARGS}}

# Run the proto backfill tool (debug)
run-backfill-proto *ARGS:
    cargo run --bin backfill-proto -- {{ARGS}}

# Run the preview server (debug)
run-preview *ARGS:
    cargo run --bin pensieve-preview -- {{ARGS}}

# Test the preview crate
test-preview:
    cargo test -p pensieve-preview --lib -- --nocapture

# Test the preview crate (verbose)
test-preview-verbose:
    cargo test -p pensieve-preview -- --nocapture

# Test coverage for the preview crate (requires cargo-llvm-cov)
coverage-preview:
    cargo llvm-cov --lib -p pensieve-preview --html --open

# Test coverage summary for the preview crate
coverage-preview-summary:
    cargo llvm-cov --lib -p pensieve-preview

# Run the relay cleanup tool
run-relay-cleanup *ARGS:
    cargo run --bin relay-cleanup -- {{ARGS}}

# ============================================================================
# Documentation
# ============================================================================

# Generate documentation
doc:
    cargo doc --workspace --no-deps

# Generate and open documentation
doc-open:
    cargo doc --workspace --no-deps --open

# ============================================================================
# Production Deployment
# ============================================================================

# Stop the ingester service
prod-ingest-stop:
    sudo systemctl stop pensieve-ingest

# Start the ingester service
prod-ingest-start:
    sudo systemctl start pensieve-ingest

# Restart the ingester service
prod-ingest-restart:
    sudo systemctl restart pensieve-ingest

# Stop the API service
prod-api-stop:
    sudo systemctl stop pensieve-api

# Start the API service
prod-api-start:
    sudo systemctl start pensieve-api

# Restart the API service
prod-api-restart:
    sudo systemctl restart pensieve-api

# Stop both ingester and API
prod-stop: prod-ingest-stop prod-api-stop
    @echo "✓ All services stopped"

# Start both ingester and API
prod-start: prod-api-start prod-ingest-start
    @echo "✓ All services started"

# Restart both ingester and API
prod-restart: prod-ingest-restart prod-api-restart
    @echo "✓ All services restarted"

# Restart production Grafana
prod-grafana-restart:
    cd ~/pensieve/ops/production && docker compose --env-file /etc/pensieve/pensieve.env restart grafana

# Show status of all Pensieve services
prod-status:
    sudo systemctl status pensieve-api pensieve-ingest --no-pager

# View ingester logs
prod-logs-ingest:
    journalctl -u pensieve-ingest -f

# View API logs
prod-logs-api:
    journalctl -u pensieve-api -f

# ============================================================================
# ClickHouse
# ============================================================================

# Container name for local dev
CH_CONTAINER := env_var_or_default("CH_CONTAINER", "pensieve-clickhouse")
CH_DB := env_var_or_default("CH_DB", "nostr")

# Run a ClickHouse migration (idempotent - safe to run multiple times)
ch-migrate file:
    @echo "Running migration: {{file}}"
    @docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} < {{file}}
    @echo "✓ Migration complete"

# Run all pending migrations in order
ch-migrate-all:
    @echo "Running all migrations..."
    @for f in docs/migrations/*.sql; do \
        echo "→ $f"; \
        docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} < "$f" || exit 1; \
    done
    @echo "✓ All migrations complete"

# Initialize ClickHouse with full schema (fresh deployment)
ch-init:
    @echo "Initializing ClickHouse schema..."
    @docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} < docs/clickhouse_self_hosted.sql
    @echo "✓ Schema initialized"

# Run a raw ClickHouse query
ch-query query:
    @docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} --query "{{query}}"

# Show ClickHouse tables and views
ch-tables:
    @docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} \
        --query "SELECT name, engine FROM system.tables WHERE database = '{{CH_DB}}' ORDER BY engine, name"

# ============================================================================
# Utilities
# ============================================================================

# Show dependency tree
deps:
    cargo tree

# Update dependencies
update:
    cargo update

# Audit dependencies for security vulnerabilities
audit:
    cargo audit

# Count lines of code
loc:
    @tokei crates/ || find crates -name "*.rs" | xargs wc -l
