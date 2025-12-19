#!/bin/bash
# Run ClickHouse migrations in production
#
# Usage:
#   ./scripts/ch-migrate.sh                          # Run all migrations
#   ./scripts/ch-migrate.sh path/to/migration.sql    # Run specific migration

set -euo pipefail

# Get the directory where this script lives
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$(dirname "$DEPLOY_DIR")"
MIGRATIONS_DIR="$PROJECT_ROOT/docs/migrations"

# ClickHouse container name (adjust if different)
CH_CONTAINER="${CH_CONTAINER:-pensieve-clickhouse-1}"
CH_DATABASE="${CH_DATABASE:-nostr}"

run_migration() {
    local file="$1"
    echo "→ Running: $(basename "$file")"
    docker exec -i "$CH_CONTAINER" clickhouse-client --database "$CH_DATABASE" < "$file"
    echo "  ✓ Complete"
}

if [ $# -eq 0 ]; then
    # Run all migrations in order
    echo "Running all migrations from $MIGRATIONS_DIR..."
    echo ""

    if [ ! -d "$MIGRATIONS_DIR" ]; then
        echo "Error: Migrations directory not found: $MIGRATIONS_DIR"
        exit 1
    fi

    for f in "$MIGRATIONS_DIR"/*.sql; do
        [ -e "$f" ] || continue  # Skip if no .sql files
        run_migration "$f"
    done

    echo ""
    echo "✓ All migrations complete"
else
    # Run specific migration
    if [ ! -f "$1" ]; then
        echo "Error: File not found: $1"
        exit 1
    fi
    run_migration "$1"
fi

