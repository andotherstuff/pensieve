#!/bin/bash
#
# Stream sealed notepack segments through the resumable Parquet campaign.
#
# The source remote is never modified. One segment is downloaded into the
# bounded local spool, published and activated, and only then are the local
# input and generated staging artifacts removed. The SQLite inventory and
# durable receipt remain so subsequent runs skip completed source paths. A
# failed work unit is logged, removed from the bounded spool, and does not stop
# later source segments in the same pass; the read-only remote remains intact.

set -euo pipefail

require_env() {
    local name="$1"
    if [ -z "${!name:-}" ]; then
        echo "Required environment variable is empty: $name" >&2
        exit 2
    fi
}

require_uint() {
    local name="$1"
    local value="${!name}"
    if ! [[ "$value" =~ ^[0-9]+$ ]]; then
        echo "Environment variable must be an unsigned integer: $name" >&2
        exit 2
    fi
}

sql_quote() {
    local value="$1"
    printf "%s" "${value//\'/\'\'}"
}

completed_source() {
    local source_path="$1"
    local filename="$2"
    if [ ! -f "$STATE_DB" ]; then
        return 1
    fi
    local escaped
    escaped="$(sql_quote "$source_path")"
    if [ "$(sqlite3 "$STATE_DB" \
        "SELECT count(*) FROM work_units WHERE source_path = '$escaped' AND state IN ('published', 'source_committed');")" = "1" ]; then
        return 0
    fi

    local receipt="$RECEIPT_DIR/$filename.published"
    if [ ! -s "$receipt" ]; then
        return 1
    fi
    local receipt_id receipt_sha256
    read -r receipt_id receipt_sha256 _ <"$receipt"
    if ! [[ "$receipt_sha256" =~ ^[0-9a-f]{64}$ ]] ||
        [ "$receipt_id" != "notepack-sha256-$receipt_sha256" ]; then
        echo "Ignoring malformed publication receipt: $receipt" >&2
        return 1
    fi
    local escaped_id escaped_sha256
    escaped_id="$(sql_quote "$receipt_id")"
    escaped_sha256="$(sql_quote "$receipt_sha256")"
    [ "$(sqlite3 "$STATE_DB" \
        "SELECT count(*) FROM work_units WHERE id = '$escaped_id' AND source_sha256 = '$escaped_sha256' AND state IN ('published', 'source_committed');")" = "1" ]
}

published_identity() {
    local work_unit_id="$1"
    local escaped
    escaped="$(sql_quote "$work_unit_id")"
    sqlite3 -separator ' ' "$STATE_DB" \
        "SELECT id, source_sha256, input_events, output_rows, rejected_events FROM work_units WHERE id = '$escaped' AND state IN ('published', 'source_committed');"
}

SOURCE_REMOTE="${SOURCE_REMOTE:-}"
SOURCE_REMOTE="${SOURCE_REMOTE%/}"
STATE_DB="${STATE_DB:-/var/lib/pensieve-parquet/state/campaign.sqlite}"
STAGING_DIR="${STAGING_DIR:-/var/lib/pensieve-parquet/staging/campaign}"
INPUT_DIR="${INPUT_DIR:-/var/lib/pensieve-parquet/input}"
RECEIPT_DIR="${RECEIPT_DIR:-/var/lib/pensieve-parquet/state/receipts}"
LOCK_FILE="${LOCK_FILE:-/var/lib/pensieve-parquet/state/campaign.lock}"
CAMPAIGN_BIN="${CAMPAIGN_BIN:-/home/pensieve/pensieve/target/release/pensieve-parquet-campaign}"
SOURCE_MANIFEST_BIN="${SOURCE_MANIFEST_BIN:-/home/pensieve/pensieve/target/release/pensieve-source-manifest}"
SOURCE_MANIFEST="${SOURCE_MANIFEST:-/var/lib/pensieve-parquet/state/historical-source-manifest.json}"
HISTORICAL_MAX_SEGMENT="${HISTORICAL_MAX_SEGMENT:-}"
MIN_FREE_BYTES="${MIN_FREE_BYTES:-5368709120}"
WORKING_SPACE_MULTIPLIER="${WORKING_SPACE_MULTIPLIER:-3}"
MAX_WORK_UNITS="${MAX_WORK_UNITS:-0}"
INVENTORY_ONLY="${INVENTORY_ONLY:-0}"

for name in SOURCE_REMOTE AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_REGION \
    S3_BUCKET S3_ENDPOINT_URL S3_ARCHIVE_PREFIX HISTORICAL_MAX_SEGMENT; do
    require_env "$name"
done
for name in HISTORICAL_MAX_SEGMENT MIN_FREE_BYTES WORKING_SPACE_MULTIPLIER \
    MAX_WORK_UNITS INVENTORY_ONLY; do
    require_uint "$name"
done
if [ "$WORKING_SPACE_MULTIPLIER" -lt 2 ]; then
    echo "WORKING_SPACE_MULTIPLIER must be at least 2" >&2
    exit 2
fi
if [ "$INVENTORY_ONLY" -gt 1 ]; then
    echo "INVENTORY_ONLY must be 0 or 1" >&2
    exit 2
fi
if [ ! -x "$CAMPAIGN_BIN" ]; then
    echo "Campaign binary is not executable: $CAMPAIGN_BIN" >&2
    exit 2
fi
if [ ! -x "$SOURCE_MANIFEST_BIN" ]; then
    echo "Source manifest binary is not executable: $SOURCE_MANIFEST_BIN" >&2
    exit 2
fi

install -d -m 0750 "$INPUT_DIR" "$STAGING_DIR" "$RECEIPT_DIR" \
    "$(dirname "$LOCK_FILE")" "$(dirname "$SOURCE_MANIFEST")"
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
    echo "Another Parquet archive campaign holds $LOCK_FILE" >&2
    exit 3
fi

source_inventory="$(mktemp "$RECEIPT_DIR/.source-inventory.XXXXXX")"
trap 'rm -f "$source_inventory"' EXIT

if [ ! -e "$SOURCE_MANIFEST" ]; then
    source_inventory_json="$(mktemp "$RECEIPT_DIR/.source-inventory.XXXXXX.json")"
    trap 'rm -f "$source_inventory" "${source_inventory_json:-}"' EXIT
    rclone lsjson "$SOURCE_REMOTE" \
        --files-only \
        --max-depth 1 \
        --include 'segment-*.notepack' \
        --include 'segment-*.notepack.gz' >"$source_inventory_json"
    "$SOURCE_MANIFEST_BIN" build \
        --rclone-lsjson "$source_inventory_json" \
        --max-segment-number "$HISTORICAL_MAX_SEGMENT" \
        --output "$SOURCE_MANIFEST"
    rm -f "$source_inventory_json"
fi

"$SOURCE_MANIFEST_BIN" verify \
    --manifest "$SOURCE_MANIFEST" \
    --expected-max-segment-number "$HISTORICAL_MAX_SEGMENT"
"$SOURCE_MANIFEST_BIN" entries \
    --manifest "$SOURCE_MANIFEST" >"$source_inventory"
if [ "$INVENTORY_ONLY" -eq 1 ]; then
    exit 0
fi

attempted=0
published=0
failures=0
while IFS=$'\t' read -r filename remote_bytes; do
    [ -n "$filename" ] || continue
    if ! [[ "$filename" =~ ^segment-[0-9]+\.notepack(\.gz)?$ ]]; then
        echo "Skipping unexpected source name: $filename" >&2
        continue
    fi
    if ! [[ "$remote_bytes" =~ ^[0-9]+$ ]]; then
        echo "Manifest has invalid source size: $filename bytes=$remote_bytes" >&2
        exit 4
    fi

    local_input="$INPUT_DIR/$filename"
    if completed_source "$local_input" "$filename"; then
        echo "Already published: $filename"
        continue
    fi
    if [ "$MAX_WORK_UNITS" -ne 0 ] && [ "$attempted" -ge "$MAX_WORK_UNITS" ]; then
        break
    fi

    remote_source="$SOURCE_REMOTE/$filename"
    available_bytes="$(df --output=avail -B1 "$INPUT_DIR" | tail -n 1 | tr -d ' ')"
    required_bytes=$((MIN_FREE_BYTES + remote_bytes * WORKING_SPACE_MULTIPLIER))
    if [ "$available_bytes" -lt "$required_bytes" ]; then
        echo "Insufficient spool space for $filename: available=$available_bytes required=$required_bytes" >&2
        exit 5
    fi

    if [ -f "$local_input" ]; then
        local_bytes="$(stat -c %s "$local_input")"
        if [ "$local_bytes" -ne "$remote_bytes" ]; then
            echo "Existing spool input has wrong size: $local_input" >&2
            exit 6
        fi
        echo "Resuming local input: $filename ($local_bytes bytes)"
    else
        partial="$INPUT_DIR/.$filename.partial"
        rm -f -- "$partial"
        echo "Downloading: $remote_source ($remote_bytes bytes)"
        rclone copyto "$remote_source" "$partial" \
            --retries 5 \
            --low-level-retries 10 \
            --stats 30s \
            --stats-one-line
        local_bytes="$(stat -c %s "$partial")"
        if [ "$local_bytes" -ne "$remote_bytes" ]; then
            echo "Downloaded source size mismatch: expected=$remote_bytes actual=$local_bytes" >&2
            exit 7
        fi
        sync -f "$partial"
        mv "$partial" "$local_input"
        sync -f "$INPUT_DIR"
    fi

    source_sha256="$(sha256sum "$local_input")"
    source_sha256="${source_sha256%% *}"
    work_unit_id="notepack-sha256-$source_sha256"
    attempted=$((attempted + 1))
    if "$CAMPAIGN_BIN" "$local_input" \
        --state-db "$STATE_DB" \
        --staging-dir "$STAGING_DIR" \
        --s3-bucket "$S3_BUCKET" \
        --s3-region "$AWS_REGION" \
        --s3-endpoint-url "$S3_ENDPOINT_URL" \
        --object-prefix "$S3_ARCHIVE_PREFIX" \
        --cleanup-published-staging; then
        :
    else
        campaign_status=$?
        failures=$((failures + 1))
        echo "Campaign attempt failed; source remains remote and processing will continue: $filename status=$campaign_status" >&2
        rm -f -- "$local_input"
        sync -f "$INPUT_DIR"
        continue
    fi

    identity="$(published_identity "$work_unit_id")"
    if [ -z "$identity" ]; then
        echo "Campaign exited successfully without a published inventory row: $filename" >&2
        exit 8
    fi
    receipt="$RECEIPT_DIR/$filename.published"
    receipt_partial="$receipt.partial"
    printf "%s\n" "$identity" >"$receipt_partial"
    chmod 0640 "$receipt_partial"
    sync -f "$receipt_partial"
    mv "$receipt_partial" "$receipt"
    sync -f "$RECEIPT_DIR"

    rm -f -- "$local_input"
    sync -f "$INPUT_DIR"
    published=$((published + 1))
    read -r _ _ input_events output_rows rejected_events <<<"$identity"
    if [ "$input_events" -eq 0 ] && [ "$output_rows" -eq 0 ] && [ "$rejected_events" -eq 0 ]; then
        echo "Recorded empty source and cleaned: $filename"
    else
        echo "Published and cleaned: $filename"
    fi
done <"$source_inventory"

echo "Campaign pass complete: attempted=$attempted published=$published failed=$failures"
