#!/bin/bash
# Run a bounded plan of exact notepack-to-Parquet comparisons with retained evidence.

set -euo pipefail

plan="${1:?usage: run-parquet-seven-field-verification.sh PLAN EVIDENCE_DIR}"
evidence_dir="${2:?usage: run-parquet-seven-field-verification.sh PLAN EVIDENCE_DIR}"
comparator="${COMPARATOR_BIN:-/home/pensieve/pensieve/target/release/pensieve-parquet-compare}"

for name in SOURCE_REMOTE RCLONE_CONFIG AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_REGION \
    S3_BUCKET S3_ENDPOINT_URL; do
    if [ -z "${!name:-}" ]; then
        echo "Required environment variable is empty: $name" >&2
        exit 2
    fi
done
if [ ! -x "$comparator" ]; then
    echo "Comparator is not executable: $comparator" >&2
    exit 2
fi
if [ ! -s "$plan" ]; then
    echo "Comparison plan is empty: $plan" >&2
    exit 2
fi

install -d -m 0750 "$evidence_dir"
scratch="$(mktemp -d "$evidence_dir/.scratch.XXXXXX")"
cleanup() {
    case "$scratch" in
        "$evidence_dir"/.scratch.*) rm -rf -- "$scratch" ;;
        *) echo "Refusing unsafe scratch cleanup: $scratch" >&2 ;;
    esac
}
trap cleanup EXIT

cp "$plan" "$evidence_dir/plan.tsv"
export RCLONE_CONFIG_ARCHIVE_TYPE=s3
export RCLONE_CONFIG_ARCHIVE_PROVIDER=Other
export RCLONE_CONFIG_ARCHIVE_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID"
export RCLONE_CONFIG_ARCHIVE_SECRET_ACCESS_KEY="$AWS_SECRET_ACCESS_KEY"
export RCLONE_CONFIG_ARCHIVE_REGION="$AWS_REGION"
export RCLONE_CONFIG_ARCHIVE_ENDPOINT="$S3_ENDPOINT_URL"

while IFS=$'\t' read -r label source_kind source fragment work_unit_id; do
    if [ -z "$label" ] || [[ ! "$label" =~ ^[a-zA-Z0-9._-]+$ ]]; then
        echo "Invalid comparison label: $label" >&2
        exit 2
    fi
    if [ ! -s "$fragment" ]; then
        echo "Catalog fragment is missing: $fragment" >&2
        exit 2
    fi
    comparison_dir="$(mktemp -d "$scratch/$label.XXXXXX")"
    case "$source_kind" in
        remote)
            source_path="$comparison_dir/$source"
            rclone copyto "${SOURCE_REMOTE%/}/$source" "$source_path"
            ;;
        local)
            source_path="$source"
            if [ ! -s "$source_path" ]; then
                echo "Local comparison source is missing: $source_path" >&2
                exit 2
            fi
            ;;
        *)
            echo "Unknown comparison source kind: $source_kind" >&2
            exit 2
            ;;
    esac

    if [ -z "$work_unit_id" ]; then
        work_unit_id="$(
            jq -r --arg source "$source" \
                '.work_units[] | select(.source_name == $source) | .work_unit_id' \
                "$fragment"
        )"
    fi
    if [ -z "$work_unit_id" ] || [ "$(printf '%s\n' "$work_unit_id" | wc -l)" -ne 1 ]; then
        echo "Comparison $label did not resolve exactly one work unit" >&2
        exit 3
    fi

    mapfile -t objects < <(
        jq -r --arg work "$work_unit_id" \
            '.objects[] | select(.work_unit_id == $work) |
             [.object_key, (.byte_size | tostring), .sha256] | @tsv' \
            "$fragment"
    )
    if [ "${#objects[@]}" -eq 0 ]; then
        echo "Comparison $label resolved no active Parquet objects" >&2
        exit 3
    fi

    comparator_args=(--source "$source_path")
    : >"$evidence_dir/$label.objects.tsv"
    part=0
    for object in "${objects[@]}"; do
        IFS=$'\t' read -r object_key expected_bytes expected_sha256 <<<"$object"
        parquet="$comparison_dir/part-$(printf '%05d' "$part").parquet"
        rclone copyto "archive:$S3_BUCKET/$object_key" "$parquet"
        actual_bytes="$(stat -c %s "$parquet")"
        actual_sha256="$(sha256sum "$parquet" | cut -d ' ' -f1)"
        if [ "$actual_bytes" != "$expected_bytes" ] || [ "$actual_sha256" != "$expected_sha256" ]; then
            echo "Downloaded object differs from catalog: $object_key" >&2
            exit 4
        fi
        printf '%s\t%s\t%s\n' "$object_key" "$actual_bytes" "$actual_sha256" \
            >>"$evidence_dir/$label.objects.tsv"
        comparator_args+=(--parquet "$parquet")
        part=$((part + 1))
    done

    "$comparator" "${comparator_args[@]}" | tee "$evidence_dir/$label.json"
    sha256sum "$source_path" >"$evidence_dir/$label.source.sha256"
    rm -rf -- "$comparison_dir"
done <"$plan"

sha256sum "$evidence_dir"/*.json "$evidence_dir"/*.sha256 "$evidence_dir"/*.tsv \
    >"$evidence_dir/SHA256SUMS"
sync -f "$evidence_dir/SHA256SUMS"
