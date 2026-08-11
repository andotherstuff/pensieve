#!/bin/bash
# Resumably stage and verify only catalog objects absent from a local lake root.

set -euo pipefail

snapshot="${1:?usage: stage-missing-analytics-objects.sh SNAPSHOT LOCAL_ROOT EVIDENCE_DIR}"
local_root="${2:?usage: stage-missing-analytics-objects.sh SNAPSHOT LOCAL_ROOT EVIDENCE_DIR}"
evidence_dir="${3:?usage: stage-missing-analytics-objects.sh SNAPSHOT LOCAL_ROOT EVIDENCE_DIR}"

s3_bucket="${PENSIEVE_PARQUET_SHADOW_S3_BUCKET:-${S3_BUCKET:-}}"
s3_endpoint_url="${PENSIEVE_PARQUET_SHADOW_S3_ENDPOINT_URL:-${S3_ENDPOINT_URL:-}}"
s3_region="${PENSIEVE_PARQUET_SHADOW_S3_REGION:-${AWS_REGION:-}}"
max_objects="${MAX_STAGE_OBJECTS:-2000}"
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
if [ ! -s "$snapshot" ]; then
    echo "Analytics snapshot is missing: $snapshot" >&2
    exit 2
fi
if [ "$local_root" = "/" ] || [ "$evidence_dir" = "/" ]; then
    echo "Refusing to use the filesystem root as a staging path" >&2
    exit 2
fi

install -d -m 0750 "$local_root" "$evidence_dir"
manifest="$evidence_dir/missing-objects.tsv"
if [ ! -e "$manifest" ]; then
    jq -er '.objects[] | [.object_key, .byte_size, .sha256] | @tsv' "$snapshot" |
        while IFS=$'\t' read -r object_key byte_size sha256; do
            object_path="$local_root/$object_key"
            if [ -e "$object_path" ]; then
                if [ ! -f "$object_path" ]; then
                    echo "Catalog object path is not a regular file: $object_key" >&2
                    exit 1
                fi
                actual_size="$(stat -c '%s' "$object_path")"
                if [ "$actual_size" != "$byte_size" ]; then
                    echo "Existing object size mismatch for $object_key" >&2
                    exit 1
                fi
            else
                printf '%s\t%s\t%s\n' "$object_key" "$byte_size" "$sha256"
            fi
        done >"$manifest.new"
    mv "$manifest.new" "$manifest"
fi

expected_objects="$(wc -l <"$manifest")"
expected_bytes="$(awk -F '\t' '{sum += $2} END {printf "%.0f\n", sum}' "$manifest")"
if [ "$expected_objects" -gt "$max_objects" ]; then
    echo "Missing set has $expected_objects objects; limit is $max_objects" >&2
    exit 2
fi
if [ "$expected_bytes" -gt "$max_bytes" ]; then
    echo "Missing set has $expected_bytes bytes; limit is $max_bytes" >&2
    exit 2
fi

files_from="$evidence_dir/object-keys.txt"
cut -f1 "$manifest" >"$files_from.new"
if [ -e "$files_from" ]; then
    cmp "$files_from" "$files_from.new"
fi
mv "$files_from.new" "$files_from"

if [ "$expected_objects" -gt 0 ]; then
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
fi

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

snapshot_id="$(jq -er '.snapshot_id' "$snapshot")"
jq -n \
    --arg snapshot_id "$snapshot_id" \
    --argjson objects "$expected_objects" \
    --argjson bytes "$expected_bytes" \
    '{snapshot_id:$snapshot_id, staged_and_verified_objects:$objects,
      staged_and_verified_bytes:$bytes, status:"passed"}' \
    >"$evidence_dir/verification.json.new"
mv "$evidence_dir/verification.json.new" "$evidence_dir/verification.json"
sha256sum "$snapshot" "$manifest" "$files_from" "$checksums" \
    "$evidence_dir/verification.json" >"$evidence_dir/SHA256SUMS"
sync -f "$evidence_dir/SHA256SUMS"

printf 'Staged and verified %s missing objects (%s bytes) for %s\n' \
    "$expected_objects" "$expected_bytes" "$snapshot_id"
