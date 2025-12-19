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
    @ls -lh target/release/pensieve-ingest target/release/pensieve-serve target/release/backfill-jsonl target/release/backfill-proto 2>/dev/null || true

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
# Docker / Deployment
# ============================================================================

# Start local dev services (ClickHouse, etc.)
dev-up:
    docker compose up -d

# Stop local dev services
dev-down:
    docker compose down

# View local dev service logs
dev-logs:
    docker compose logs -f

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

