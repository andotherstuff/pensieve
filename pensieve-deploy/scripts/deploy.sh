#!/bin/bash
#
# Deploy/update Pensieve
#
# Usage:
#   ./deploy.sh          # Pull latest and restart
#   ./deploy.sh --build  # Rebuild images locally

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(dirname "$SCRIPT_DIR")"

cd "$DEPLOY_DIR"

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

log "Deploying Pensieve..."

# Pull latest config
if [ -d .git ]; then
    log "Pulling latest configuration..."
    git pull --ff-only
fi

# Pull latest images
log "Pulling Docker images..."
docker compose pull

# Restart services
log "Restarting services..."
docker compose down
docker compose up -d

# Wait for health checks
log "Waiting for services to be healthy..."
sleep 10

# Check status
docker compose ps

log "Deployment complete!"

# Show quick stats
log "ClickHouse status:"
docker compose exec -T clickhouse clickhouse-client \
    --query "SELECT 'events', count() FROM nostr.events_local" 2>/dev/null || echo "  (table not ready yet)"

