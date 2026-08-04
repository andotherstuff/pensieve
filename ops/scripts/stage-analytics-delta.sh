#!/bin/bash
# Resumably stage and verify only the added objects in one analytics delta plan.

set -euo pipefail

plan="${1:?usage: stage-analytics-delta.sh PLAN LOCAL_ROOT EVIDENCE_DIR}"
local_root="${2:?usage: stage-analytics-delta.sh PLAN LOCAL_ROOT EVIDENCE_DIR}"
evidence_dir="${3:?usage: stage-analytics-delta.sh PLAN LOCAL_ROOT EVIDENCE_DIR}"

s3_bucket="${S3_BUCKET:-${PENSIEVE_PARQUET_SHADOW_S3_BUCKET:-}}"
s3_endpoint_url="${S3_ENDPOINT_URL:-${PENSIEVE_PARQUET_SHADOW_S3_ENDPOINT_URL:-}}"
s3_region="${AWS_REGION:-${PENSIEVE_PARQUET_SHADOW_S3_REGION:-}}"
max_objects="${MAX_STAGE_OBJECTS:-1000}"
max_bytes="${MAX_STAGE_BYTES:-107374182400}"

for name in AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY; do
    if [ -z "${!name:-}" ]; then
        echo "Required environment variable is empty: $name" >&2
        exit 2
    fi
done
for value_name in s3_bucket s3_endpoint_url s3_region; do
    if [ -z "${!value_name}" ]; then
        echo "Required S3 setting is empty: $value_name" >&2
        exit 2
    fi
done
if [ ! -s "$plan" ]; then
    echo "Analytics delta plan is missing: $plan" >&2
    exit 2
fi
if [ "$local_root" = "/" ] || [ "$evidence_dir" = "/" ]; then
    echo "Refusing to use the filesystem root as a staging path" >&2
    exit 2
fi

run_kind="$(jq -er '.run_kind' "$plan")"
expected_objects="$(jq -er '.added_objects | length' "$plan")"
expected_bytes="$(jq -er '.added_bytes' "$plan")"
removed_objects="$(jq -er '.removed_objects | length' "$plan")"
summed_bytes="$(jq -er '[.added_objects[].byte_size] | add // 0' "$plan")"
if [ "$run_kind" != "incremental" ] || [ "$removed_objects" != "0" ]; then
    echo "Only an incremental plan with zero removed objects can be staged" >&2
    exit 2
fi
if [ "$expected_objects" = "0" ]; then
    echo "Incremental plan contains no added objects" >&2
    exit 2
fi
if [ "$expected_bytes" != "$summed_bytes" ]; then
    echo "Plan added_bytes does not match its added objects" >&2
    exit 2
fi
if [ "$expected_objects" -gt "$max_objects" ]; then
    echo "Plan has $expected_objects objects; limit is $max_objects" >&2
    exit 2
fi
if [ "$expected_bytes" -gt "$max_bytes" ]; then
    echo "Plan has $expected_bytes bytes; limit is $max_bytes" >&2
    exit 2
fi

install -d -m 0750 "$local_root" "$evidence_dir"
manifest="$evidence_dir/objects.tsv"
jq -er '.added_objects[] | [.object_key, .byte_size, .sha256] | @tsv' \
    "$plan" >"$manifest.new"
if [ -e "$manifest" ]; then
    cmp "$manifest" "$manifest.new"
fi
mv "$manifest.new" "$manifest"
test "$(wc -l <"$manifest")" = "$expected_objects"

files_from="$evidence_dir/object-keys.txt"
cut -f1 "$manifest" >"$files_from.new"
if [ -e "$files_from" ]; then
    cmp "$files_from" "$files_from.new"
fi
mv "$files_from.new" "$files_from"

export RCLONE_CONFIG="${RCLONE_CONFIG:-/dev/null}"
export RCLONE_CONFIG_ARCHIVE_TYPE=s3
export RCLONE_CONFIG_ARCHIVE_PROVIDER=Other
export RCLONE_CONFIG_ARCHIVE_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID"
export RCLONE_CONFIG_ARCHIVE_SECRET_ACCESS_KEY="$AWS_SECRET_ACCESS_KEY"
export RCLONE_CONFIG_ARCHIVE_REGION="$s3_region"
export RCLONE_CONFIG_ARCHIVE_ENDPOINT="$s3_endpoint_url"

rclone copy "archive:$s3_bucket" "$local_root" \
    --files-from "$files_from" \
    --transfers "${STAGE_TRANSFERS:-4}" \
    --checkers "${STAGE_CHECKERS:-8}" \
    --retries 20 \
    --low-level-retries 50 \
    --retries-sleep 10s \
    --contimeout 30s \
    --timeout 5m \
    --stats 1m \
    --stats-one-line \
    --log-file "$evidence_dir/rclone.log" \
    --log-level INFO

checksums="$evidence_dir/OBJECT_SHA256SUMS"
: >"$checksums.new"
while IFS=$'\t' read -r object_key byte_size sha256; do
    object_path="$local_root/$object_key"
    if [ ! -f "$object_path" ]; then
        echo "Staged object is missing: $object_key" >&2
        exit 1
    fi
    actual_size="$(stat -c '%s' "$object_path")"
    if [ "$actual_size" != "$byte_size" ]; then
        echo "Size mismatch for $object_key: expected $byte_size, got $actual_size" >&2
        exit 1
    fi
    actual_sha256="$(sha256sum "$object_path" | cut -d' ' -f1)"
    if [ "$actual_sha256" != "$sha256" ]; then
        echo "SHA-256 mismatch for $object_key" >&2
        exit 1
    fi
    printf '%s  %s\n' "$sha256" "$object_key" >>"$checksums.new"
done <"$manifest"
mv "$checksums.new" "$checksums"

actual_objects="$(find "$local_root" -type f | wc -l)"
actual_bytes="$(find "$local_root" -type f -printf '%s\n' | awk '{sum += $1} END {printf "%.0f\n", sum}')"
test "$actual_objects" = "$expected_objects"
test "$actual_bytes" = "$expected_bytes"

jq -n \
    --arg snapshot_id "$(jq -er '.snapshot_id' "$plan")" \
    --arg previous_run_id "$(jq -er '.previous_run_id' "$plan")" \
    --argjson objects "$actual_objects" \
    --argjson bytes "$actual_bytes" \
    '{snapshot_id:$snapshot_id, previous_run_id:$previous_run_id,
      verified_objects:$objects, verified_bytes:$bytes, status:"passed"}' \
    >"$evidence_dir/verification.json.new"
mv "$evidence_dir/verification.json.new" "$evidence_dir/verification.json"
sha256sum "$plan" "$manifest" "$files_from" "$checksums" \
    "$evidence_dir/verification.json" >"$evidence_dir/SHA256SUMS"
sync -f "$evidence_dir/SHA256SUMS"
