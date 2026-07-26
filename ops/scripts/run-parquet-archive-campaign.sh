#!/bin/bash
#
# Stream sealed notepack segments through the resumable Parquet campaign.
#
# The source remote is never modified. One segment is downloaded into the
# bounded local spool, published and activated, and only then are the local
# input and generated staging artifacts removed. The SQLite inventory and
# durable receipt remain so subsequent runs skip completed source paths.

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
CAMPAIGN_BIN="${CAMPAIGN_BIN:-/srv/pensieve/repo/target/release/pensieve-parquet-campaign}"
MIN_FREE_BYTES="${MIN_FREE_BYTES:-5368709120}"
WORKING_SPACE_MULTIPLIER="${WORKING_SPACE_MULTIPLIER:-3}"
MAX_WORK_UNITS="${MAX_WORK_UNITS:-0}"
INVENTORY_ONLY="${INVENTORY_ONLY:-0}"

for name in SOURCE_REMOTE AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_REGION \
    S3_BUCKET S3_ENDPOINT_URL S3_ARCHIVE_PREFIX; do
    require_env "$name"
done
for name in MIN_FREE_BYTES WORKING_SPACE_MULTIPLIER MAX_WORK_UNITS INVENTORY_ONLY; do
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

install -d -m 0750 "$INPUT_DIR" "$STAGING_DIR" "$RECEIPT_DIR" "$(dirname "$LOCK_FILE")"
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
    echo "Another Parquet archive campaign holds $LOCK_FILE" >&2
    exit 3
fi

source_inventory_raw="$(mktemp "$RECEIPT_DIR/.source-inventory-raw.XXXXXX")"
source_inventory="$(mktemp "$RECEIPT_DIR/.source-inventory.XXXXXX")"
source_inventory_meta="$(mktemp "$RECEIPT_DIR/.source-inventory-meta.XXXXXX")"
trap 'rm -f "$source_inventory_raw" "$source_inventory" "$source_inventory_meta"' EXIT
rclone lsf "$SOURCE_REMOTE" \
    --files-only \
    --max-depth 1 \
    --include 'segment-*.notepack' \
    --include 'segment-*.notepack.gz' >"$source_inventory_raw"

# The legacy archive may contain both the plain and gzip representation of the
# same sealed segment. Prefer gzip and never process both. A plain-only segment
# is eligible only below the highest gzip segment number: the plain file at or
# above that high-water mark may still be the live, unsealed segment.
if ! awk -v metadata="$source_inventory_meta" '
    function segment_number(name, value) {
        value = name
        sub(/^segment-/, "", value)
        sub(/[.]notepack([.]gz)?$/, "", value)
        return value + 0
    }

    /^segment-[0-9]+[.]notepack[.]gz$/ {
        base = $0
        sub(/[.]gz$/, "", base)
        gzip_by_base[base] = $0
        number = segment_number($0)
        if (!have_gzip || number > max_gzip) {
            max_gzip = number
        }
        have_gzip = 1
        next
    }

    /^segment-[0-9]+[.]notepack$/ {
        plain[$0] = 1
        next
    }

    {
        print "Skipping unexpected source name: " $0 > "/dev/stderr"
    }

    END {
        if (!have_gzip) {
            print "No sealed gzip segments found; refusing to infer a safe source high-water mark" > "/dev/stderr"
            exit 42
        }

        selected = 0
        paired = 0
        skipped_high_water = 0
        for (base in gzip_by_base) {
            print gzip_by_base[base]
            selected++
        }
        for (name in plain) {
            if (name in gzip_by_base) {
                paired++
            } else if (segment_number(name) < max_gzip) {
                print name
                selected++
            } else {
                skipped_high_water++
            }
        }
        print max_gzip, selected, paired, skipped_high_water > metadata
        close(metadata)
    }
' "$source_inventory_raw" >"$source_inventory"; then
    echo "Unable to select a safe sealed source inventory" >&2
    exit 4
fi
sort -V -o "$source_inventory" "$source_inventory"
read -r max_sealed_gzip selected_sources paired_sources skipped_high_water <"$source_inventory_meta"
echo "Source inventory: selected=$selected_sources max_sealed_gzip=$max_sealed_gzip paired_plain_skipped=$paired_sources high_water_plain_skipped=$skipped_high_water"
if [ "$INVENTORY_ONLY" -eq 1 ]; then
    exit 0
fi

processed=0
while IFS= read -r filename; do
    [ -n "$filename" ] || continue
    if ! [[ "$filename" =~ ^segment-[0-9]+\.notepack(\.gz)?$ ]]; then
        echo "Skipping unexpected source name: $filename" >&2
        continue
    fi

    local_input="$INPUT_DIR/$filename"
    if completed_source "$local_input" "$filename"; then
        echo "Already published: $filename"
        continue
    fi
    if [ "$MAX_WORK_UNITS" -ne 0 ] && [ "$processed" -ge "$MAX_WORK_UNITS" ]; then
        break
    fi

    remote_source="$SOURCE_REMOTE/$filename"
    remote_bytes="$(rclone size --json "$remote_source" | jq -r '.bytes')"
    if ! [[ "$remote_bytes" =~ ^[0-9]+$ ]]; then
        echo "Unable to determine nonnegative source size: $remote_source" >&2
        exit 4
    fi
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
    "$CAMPAIGN_BIN" "$local_input" \
        --state-db "$STATE_DB" \
        --staging-dir "$STAGING_DIR" \
        --s3-bucket "$S3_BUCKET" \
        --s3-region "$AWS_REGION" \
        --s3-endpoint-url "$S3_ENDPOINT_URL" \
        --object-prefix "$S3_ARCHIVE_PREFIX" \
        --cleanup-published-staging

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
    processed=$((processed + 1))
    read -r _ _ input_events output_rows rejected_events <<<"$identity"
    if [ "$input_events" -eq 0 ] && [ "$output_rows" -eq 0 ] && [ "$rejected_events" -eq 0 ]; then
        echo "Recorded empty source and cleaned: $filename"
    else
        echo "Published and cleaned: $filename"
    fi
done <"$source_inventory"

echo "Campaign pass complete: processed=$processed"
