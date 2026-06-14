#!/bin/bash
#
# Sync archive segments to Hetzner Storage Box
#
# This script:
# 1. Uploads all local segments to Storage Box (if not already there)
# 2. Optionally cleans up old local segments if disk is getting full
#
# Environment variables (from .env):
#   LOCAL_RETENTION_DAYS    - Keep segments locally for at least this many days (default: 180)
#   DISK_CLEANUP_THRESHOLD  - Start cleanup when disk usage exceeds this % (default: 80)

set -euo pipefail

# Configuration
ARCHIVE_DIR="${ARCHIVE_PATH:-/archive/segments}"
REMOTE_NAME="storagebox"
REMOTE_PATH="${STORAGE_BOX_PATH:-pensieve/archive}"
RETENTION_DAYS="${LOCAL_RETENTION_DAYS:-180}"
CLEANUP_THRESHOLD="${DISK_CLEANUP_THRESHOLD:-80}"

# Logging
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

log "Starting archive sync..."
log "Local:  $ARCHIVE_DIR"
log "Remote: $REMOTE_NAME:$REMOTE_PATH"

# ═══════════════════════════════════════════════════════════════════════════
# Step 1: Sync to Storage Box
# ═══════════════════════════════════════════════════════════════════════════

log "Syncing to Storage Box..."

# Use rclone copy (not sync) to avoid deleting remote files
# --ignore-existing: skip files that already exist on remote
rclone copy "$ARCHIVE_DIR" "$REMOTE_NAME:$REMOTE_PATH" \
    --ignore-existing \
    --transfers 4 \
    --checkers 8 \
    --stats 30s \
    --stats-one-line \
    --log-level INFO

log "Sync complete."

# ═══════════════════════════════════════════════════════════════════════════
# Step 2: Clean up old local segments (if disk is full)
# ═══════════════════════════════════════════════════════════════════════════

# Get disk usage percentage for archive mount
DISK_USAGE=$(df "$ARCHIVE_DIR" | awk 'NR==2 {gsub(/%/,""); print $5}')
log "Disk usage: ${DISK_USAGE}%"

if [ "$DISK_USAGE" -gt "$CLEANUP_THRESHOLD" ]; then
    log "Disk usage exceeds ${CLEANUP_THRESHOLD}%, cleaning up old segments..."

    # Find segments older than RETENTION_DAYS that exist on remote
    CLEANED=0
    while IFS= read -r -d '' file; do
        filename=$(basename "$file")

        # Check if file exists on remote
        if rclone ls "$REMOTE_NAME:$REMOTE_PATH/$filename" &>/dev/null; then
            log "Removing local copy (exists on remote): $filename"
            rm -f "$file"
            ((CLEANED++))
        else
            log "Keeping (not yet on remote): $filename"
        fi
    done < <(find "$ARCHIVE_DIR" -name "*.notepack" -mtime +"$RETENTION_DAYS" -print0 | sort -z)

    log "Cleaned up $CLEANED segment(s)."

    # Report new disk usage
    DISK_USAGE_NEW=$(df "$ARCHIVE_DIR" | awk 'NR==2 {gsub(/%/,""); print $5}')
    log "Disk usage after cleanup: ${DISK_USAGE_NEW}%"
else
    log "Disk usage below threshold, skipping cleanup."
fi

log "Archive sync finished."

