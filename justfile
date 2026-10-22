# Pensieve - Nostr Archive & Analytics
# Run `just` to see available recipes

# Default recipe - show available commands
default:
    @just --list

# ============================================================================
# Development
# ============================================================================

# Run all precommit checks (fmt rewrites files; clippy, test, ops-test)
precommit: fmt clippy test ops-test
    @echo "✓ All precommit checks passed"

# Read-only operational tooling regressions (standard-library Python only).
ops-test:
    python3 -B -m unittest discover -s ops/tests -p 'test_*.py'

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

# Build exact Slice A analytics rollups into a local DuckDB database.
analytics-build *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics -- {{ARGS}}

# Build or advance bounded pubkey first-seen (identity) state.
analytics-identity *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics-identity -- {{ARGS}}

# Build or advance bounded fixed-activity state.
analytics-activity *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics-activity -- {{ARGS}}

# Build and validate exact bounded cohort-retention evidence.
analytics-cohort-retention *ARGS:
    cargo run -p pensieve-analytics --bin pensieve-analytics-cohort-retention -- {{ARGS}}

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

# Delete local dedupe/relay state. Note: the ingester's default --output-dir is
# ./segments, not data/segments, so default-run archives survive this.
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

# Test the preview crate (lib unit tests only)
test-preview:
    cargo test -p pensieve-preview --lib -- --nocapture

# Test the preview crate (all targets, including tests/ and bins)
test-preview-verbose:
    cargo test -p pensieve-preview -- --nocapture

# Test coverage for the preview crate (requires cargo-llvm-cov)
coverage-preview:
    cargo llvm-cov --lib -p pensieve-preview --html --open

# Test coverage summary for the preview crate
coverage-preview-summary:
    cargo llvm-cov --lib -p pensieve-preview

# Run the compact preview lookup operator tool (plan/backfill/status/validate/bench)
run-preview-lookups *ARGS:
    cargo run --bin preview-lookups -- {{ARGS}}

# Run the API rollup operator tool for migration 019 (plan/backfill/status/validate/resolve)
run-api-rollups *ARGS:
    cargo run --bin api-rollups -- {{ARGS}}

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

# Show status of the API and ingester (preview: `systemctl status pensieve-preview`)
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

# ClickHouse container (production Compose name; override with CH_CONTAINER)
CH_CONTAINER := env_var_or_default("CH_CONTAINER", "pensieve-clickhouse")
CH_DB := env_var_or_default("CH_DB", "nostr")

# Run one ClickHouse migration. Re-running is NOT always safe: several drop,
# truncate or backfill tables. Read the file and docs/migrations/README.md first.
ch-migrate file:
    @echo "Running migration: {{file}}"
    @docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} < {{file}}
    @echo "✓ Migration complete"

# FRESH, EMPTY DATABASES ONLY. Re-runs every top-level docs/migrations/*.sql
# (no ledger, prepared/ excluded), including drops, TRUNCATE (009) and full
# backfills. Never run against production; use ch-migrate <file> there.
ch-migrate-all:
    @echo "Running all migrations..."
    @for f in docs/migrations/*.sql; do \
        echo "→ $f"; \
        docker exec -i {{CH_CONTAINER}} clickhouse-client --database {{CH_DB}} < "$f" || exit 1; \
    done
    @echo "✓ All migrations complete"

# Load the base schema on a fresh deployment (the nostr database must exist)
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

# Disposable ClickHouse for integration tests (same 26.8 LTS line as production)
CH_TEST_CONTAINER := env_var_or_default("CH_TEST_CONTAINER", "pensieve-ch-test")
CH_TEST_PORT := env_var_or_default("CH_TEST_PORT", "18123")

# Start a disposable ClickHouse 26.8 LTS for integration tests
ch-test-up:
    @docker run -d --rm --name {{CH_TEST_CONTAINER}} -p 127.0.0.1:{{CH_TEST_PORT}}:8123 \
        -e CLICKHOUSE_SKIP_USER_SETUP=1 --ulimit nofile=262144:262144 \
        clickhouse/clickhouse-server:26.8@sha256:8b28eb09479c36c4533eec7a6268917d04ba3a6fd90e3f4205f70e761b6d7779 >/dev/null
    @until curl -sf http://localhost:{{CH_TEST_PORT}}/ping >/dev/null; do sleep 1; done
    @echo "✓ ClickHouse test server on http://localhost:{{CH_TEST_PORT}}"

# Stop the disposable ClickHouse test server
ch-test-down:
    @docker rm -f {{CH_TEST_CONTAINER}} >/dev/null && echo "✓ ClickHouse test server removed"

# Run ClickHouse integration tests (start a server with `just ch-test-up`)
test-clickhouse:
    PENSIEVE_TEST_CLICKHOUSE_URL=${PENSIEVE_TEST_CLICKHOUSE_URL:-http://localhost:{{CH_TEST_PORT}}} \
        cargo test -p pensieve-preview --test compact_lookups -- --ignored
    PENSIEVE_TEST_CLICKHOUSE_URL=${PENSIEVE_TEST_CLICKHOUSE_URL:-http://localhost:{{CH_TEST_PORT}}} \
        cargo test -p pensieve-ingest --test clickhouse_index_recovery -- --ignored
    PENSIEVE_TEST_CLICKHOUSE_URL=${PENSIEVE_TEST_CLICKHOUSE_URL:-http://localhost:{{CH_TEST_PORT}}} \
        cargo test -p pensieve-serve --test api_rollups -- --ignored
    PENSIEVE_TEST_CLICKHOUSE_URL=${PENSIEVE_TEST_CLICKHOUSE_URL:-http://localhost:{{CH_TEST_PORT}}} \
        cargo test -p pensieve-serve --test derived_views -- --ignored
    PENSIEVE_TEST_CLICKHOUSE_URL=${PENSIEVE_TEST_CLICKHOUSE_URL:-http://localhost:{{CH_TEST_PORT}}} \
        cargo test -p pensieve-serve --test kind_activity -- --ignored
    PENSIEVE_TEST_CLICKHOUSE_URL=${PENSIEVE_TEST_CLICKHOUSE_URL:-http://localhost:{{CH_TEST_PORT}}} \
        cargo test -p pensieve-serve --test hourly_pubkeys -- --ignored

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
