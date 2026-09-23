#!/bin/bash
#
# Sync archive segments to Hetzner Storage Box
#
# This script:
# 1. Uploads sealed local segments to Storage Box (if not already there)
# 2. Optionally cleans up old local segments if disk is getting full
#
# Environment variables (from .env):
#   LOCAL_RETENTION_DAYS    - Keep segments locally for at least this many days (default: 90)
#   DISK_CLEANUP_THRESHOLD  - Start cleanup when disk usage exceeds this % (default: 80)
#   RETENTION_DRY_RUN       - Verify and report eligible files without deleting (default: false)

set -euo pipefail

# Configuration
ARCHIVE_DIR="${ARCHIVE_PATH:-/archive/segments}"
REMOTE_NAME="storagebox"
REMOTE_PATH="${STORAGE_BOX_PATH:-pensieve/archive}"
RETENTION_DAYS="${LOCAL_RETENTION_DAYS:-90}"
CLEANUP_THRESHOLD="${DISK_CLEANUP_THRESHOLD:-80}"
RETENTION_DRY_RUN="${RETENTION_DRY_RUN:-false}"

if ! [[ "$RETENTION_DAYS" =~ ^[0-9]+$ ]]; then
    echo "LOCAL_RETENTION_DAYS must be a non-negative integer" >&2
    exit 1
fi

if ! [[ "$CLEANUP_THRESHOLD" =~ ^[0-9]+$ ]] || (( CLEANUP_THRESHOLD > 100 )); then
    echo "DISK_CLEANUP_THRESHOLD must be an integer from 0 through 100" >&2
    exit 1
fi

case "$RETENTION_DRY_RUN" in
    true|false) ;;
    *)
        echo "RETENTION_DRY_RUN must be true or false" >&2
        exit 1
        ;;
esac

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

# Use rclone copy (not sync) to avoid deleting remote files.
# Include only final sealed names: active `.notepack.open` files and temporary
# `.notepack.gz.open` compression outputs must never become remote archive
# inputs.
# --ignore-existing: skip files that already exist on remote
rclone copy "$ARCHIVE_DIR" "$REMOTE_NAME:$REMOTE_PATH" \
    --filter '+ /segment-*.notepack' \
    --filter '+ /segment-*.notepack.gz' \
    --filter '- **' \
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

    # Capture one fresh remote inventory after the upload. Deletion requires an
    # exact path-and-size match; remote existence alone is not sufficient.
    REMOTE_INVENTORY=$(mktemp)
    trap 'rm -f "$REMOTE_INVENTORY"' EXIT
    rclone lsjson "$REMOTE_NAME:$REMOTE_PATH" \
        --files-only \
        --recursive \
        > "$REMOTE_INVENTORY"

    declare -A REMOTE_SIZES=()
    while IFS=$'\t' read -r remote_path remote_size; do
        REMOTE_SIZES["$remote_path"]="$remote_size"
    done < <(jq -r '.[] | select(.IsDir == false) | [.Path, (.Size | tostring)] | @tsv' "$REMOTE_INVENTORY")

    CLEANED=0
    CLEANED_BYTES=0
    ELIGIBLE=0
    MISSING_REMOTE=0
    SIZE_MISMATCH=0
    while IFS= read -r -d '' file; do
        filename=$(basename "$file")
        local_size=$(stat -c '%s' "$file")
        remote_size="${REMOTE_SIZES[$filename]:-}"

        if [ -z "$remote_size" ]; then
            log "Keeping (not present in remote inventory): $filename"
            ((MISSING_REMOTE += 1))
        elif [ "$local_size" != "$remote_size" ]; then
            log "Keeping (size mismatch local=$local_size remote=$remote_size): $filename"
            ((SIZE_MISMATCH += 1))
        else
            ((ELIGIBLE += 1))
            ((CLEANED_BYTES += local_size))
            if [ "$RETENTION_DRY_RUN" = "false" ]; then
                rm -f -- "$file"
                ((CLEANED += 1))
            fi
        fi
    done < <(
        find "$ARCHIVE_DIR" -maxdepth 1 -type f \
            \( -name 'segment-*.notepack' -o -name 'segment-*.notepack.gz' \) \
            -mtime +"$RETENTION_DAYS" -print0 | sort -z
    )

    if [ "$RETENTION_DRY_RUN" = "true" ]; then
        log "Dry run: $ELIGIBLE segment(s), $CLEANED_BYTES byte(s) eligible; $MISSING_REMOTE missing remotely; $SIZE_MISMATCH size mismatch(es)."
    else
        log "Cleaned up $CLEANED segment(s), $CLEANED_BYTES byte(s); $MISSING_REMOTE missing remotely; $SIZE_MISMATCH size mismatch(es)."
    fi

    # Report new disk usage
    DISK_USAGE_NEW=$(df "$ARCHIVE_DIR" | awk 'NR==2 {gsub(/%/,""); print $5}')
    log "Disk usage after cleanup: ${DISK_USAGE_NEW}%"
else
    log "Disk usage below threshold, skipping cleanup."
fi

log "Archive sync finished."
