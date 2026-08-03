#!/bin/bash
# Resumably stage exactly one active-raw snapshot and verify every local byte.

set -euo pipefail

snapshot="${1:?usage: stage-active-raw-snapshot.sh SNAPSHOT LOCAL_ROOT EVIDENCE_DIR}"
local_root="${2:?usage: stage-active-raw-snapshot.sh SNAPSHOT LOCAL_ROOT EVIDENCE_DIR}"
evidence_dir="${3:?usage: stage-active-raw-snapshot.sh SNAPSHOT LOCAL_ROOT EVIDENCE_DIR}"
catalog_bin="${CATALOG_BIN:-/home/pensieve/pensieve/target/release/pensieve-lake-catalog}"

for name in RCLONE_CONFIG AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_REGION S3_BUCKET \
    S3_ENDPOINT_URL; do
    if [ -z "${!name:-}" ]; then
        echo "Required environment variable is empty: $name" >&2
        exit 2
    fi
done
if [ ! -s "$snapshot" ]; then
    echo "Snapshot is missing: $snapshot" >&2
    exit 2
fi
if [ ! -x "$catalog_bin" ]; then
    echo "Catalog binary is not executable: $catalog_bin" >&2
    exit 2
fi

install -d -m 0750 "$local_root" "$evidence_dir"
files_from="$evidence_dir/object-keys.txt"
jq -r '.objects[].object_key' "$snapshot" >"$files_from.new"
if [ -e "$files_from" ]; then
    cmp "$files_from" "$files_from.new"
fi
mv "$files_from.new" "$files_from"

expected_objects="$(jq -r '.totals.objects' "$snapshot")"
expected_bytes="$(jq -r '.totals.object_bytes' "$snapshot")"
test "$(wc -l <"$files_from")" = "$expected_objects"

export RCLONE_CONFIG_ARCHIVE_TYPE=s3
export RCLONE_CONFIG_ARCHIVE_PROVIDER=Other
export RCLONE_CONFIG_ARCHIVE_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID"
export RCLONE_CONFIG_ARCHIVE_SECRET_ACCESS_KEY="$AWS_SECRET_ACCESS_KEY"
export RCLONE_CONFIG_ARCHIVE_REGION="$AWS_REGION"
export RCLONE_CONFIG_ARCHIVE_ENDPOINT="$S3_ENDPOINT_URL"

rclone copy "archive:$S3_BUCKET" "$local_root" \
    --files-from "$files_from" \
    --transfers "${STAGE_TRANSFERS:-8}" \
    --checkers "${STAGE_CHECKERS:-16}" \
    --retries 20 \
    --low-level-retries 50 \
    --retries-sleep 10s \
    --contimeout 30s \
    --timeout 5m \
    --stats 1m \
    --stats-one-line \
    --log-file "$evidence_dir/rclone.log" \
    --log-level INFO

"$catalog_bin" verify-local \
    --snapshot "$snapshot" \
    --local-object-root "$local_root" \
    | tee "$evidence_dir/verification.txt"

actual_objects="$(find "$local_root" -type f | wc -l)"
actual_bytes="$(find "$local_root" -type f -printf '%s\n' | awk '{sum += $1} END {printf "%.0f\n", sum}')"
test "$actual_objects" = "$expected_objects"
test "$actual_bytes" = "$expected_bytes"
sha256sum "$snapshot" "$files_from" "$evidence_dir/verification.txt" \
    >"$evidence_dir/SHA256SUMS"
sync -f "$evidence_dir/SHA256SUMS"
