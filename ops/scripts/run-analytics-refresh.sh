#!/bin/bash
# Advance the shadow analytics checkpoint from the live Parquet inventory.

set -euo pipefail

umask 027

repo_root="${PENSIEVE_REPO_ROOT:-/home/pensieve/pensieve}"
catalog_bin="${PENSIEVE_LAKE_CATALOG_BIN:-$repo_root/target/release/pensieve-lake-catalog}"
plan_bin="${PENSIEVE_ANALYTICS_PLAN_BIN:-$repo_root/target/release/pensieve-analytics-plan}"
incremental_bin="${PENSIEVE_ANALYTICS_INCREMENTAL_BIN:-$repo_root/target/release/pensieve-analytics-incremental}"
stage_script="${PENSIEVE_ANALYTICS_STAGE_SCRIPT:-$repo_root/ops/scripts/stage-analytics-delta.sh}"

state_root="${PENSIEVE_ANALYTICS_REFRESH_STATE_ROOT:-/var/lib/pensieve-analytics/refresh}"
generations_root="$state_root/generations"
runs_root="$state_root/runs"
current_link="$state_root/current"
deltas_root="${PENSIEVE_ANALYTICS_DELTA_ROOT:-/archive/analytics/deltas}"
backups_root="${PENSIEVE_ANALYTICS_BACKUP_ROOT:-/archive/analytics/backups}"
work_database="${PENSIEVE_ANALYTICS_WORK_DATABASE:-/archive/analytics/slice-a.duckdb}"
live_inventory="${PENSIEVE_ANALYTICS_LIVE_INVENTORY:-/archive/segments/.parquet-shadow/inventory.sqlite}"
inventory_id="${PENSIEVE_ANALYTICS_LIVE_INVENTORY_ID:-production-live-shadow}"
lock_file="${PENSIEVE_ANALYTICS_REFRESH_LOCK:-/run/pensieve-analytics-refresh/refresh.lock}"
retain_backups="${PENSIEVE_ANALYTICS_RETAIN_BACKUPS:-3}"
retain_deltas="${PENSIEVE_ANALYTICS_RETAIN_DELTAS:-2}"
min_archive_free_bytes="${PENSIEVE_ANALYTICS_MIN_ARCHIVE_FREE_BYTES:-536870912000}"
require_ingest_active="${PENSIEVE_ANALYTICS_REQUIRE_INGEST_ACTIVE:-1}"
additional_fragment_source="${PENSIEVE_ANALYTICS_ADDITIONAL_FRAGMENT:-}"
identity_enabled="${PENSIEVE_ANALYTICS_IDENTITY_ENABLED:-0}"
identity_root="${PENSIEVE_ANALYTICS_IDENTITY_ROOT:-/archive/analytics/pubkey-first-seen}"

s3_bucket="${PENSIEVE_PARQUET_SHADOW_S3_BUCKET:-${S3_BUCKET:-}}"
s3_endpoint_url="${PENSIEVE_PARQUET_SHADOW_S3_ENDPOINT_URL:-${S3_ENDPOINT_URL:-}}"
s3_region="${PENSIEVE_PARQUET_SHADOW_S3_REGION:-${AWS_REGION:-}}"

for path in "$catalog_bin" "$plan_bin" "$incremental_bin" "$stage_script"; do
    if [ ! -x "$path" ]; then
        echo "Required executable is missing: $path" >&2
        exit 2
    fi
done
for command_name in cp flock git jq rclone sha256sum sync systemctl; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "Required command is missing: $command_name" >&2
        exit 2
    fi
done
for path in "$work_database" "$live_inventory"; do
    if [ ! -f "$path" ]; then
        echo "Required input is missing: $path" >&2
        exit 2
    fi
done
if [ -n "$additional_fragment_source" ] && [ ! -s "$additional_fragment_source" ]; then
    echo "Additional catalog fragment is missing: $additional_fragment_source" >&2
    exit 2
fi
for value_name in s3_bucket s3_endpoint_url s3_region; do
    if [ -z "${!value_name}" ]; then
        echo "Required S3 setting is empty: $value_name" >&2
        exit 2
    fi
done
if ! [[ "$retain_backups" =~ ^[1-9][0-9]*$ ]]; then
    echo "PENSIEVE_ANALYTICS_RETAIN_BACKUPS must be a positive integer" >&2
    exit 2
fi
if ! [[ "$retain_deltas" =~ ^[1-9][0-9]*$ ]]; then
    echo "PENSIEVE_ANALYTICS_RETAIN_DELTAS must be a positive integer" >&2
    exit 2
fi
if ! [[ "$min_archive_free_bytes" =~ ^[1-9][0-9]*$ ]]; then
    echo "PENSIEVE_ANALYTICS_MIN_ARCHIVE_FREE_BYTES must be a positive integer" >&2
    exit 2
fi
if [ "$require_ingest_active" != "0" ] && [ "$require_ingest_active" != "1" ]; then
    echo "PENSIEVE_ANALYTICS_REQUIRE_INGEST_ACTIVE must be 0 or 1" >&2
    exit 2
fi
if [ "$identity_enabled" != "0" ] && [ "$identity_enabled" != "1" ]; then
    echo "PENSIEVE_ANALYTICS_IDENTITY_ENABLED must be 0 or 1" >&2
    exit 2
fi

install -d -m 0750 "$state_root" "$generations_root" "$runs_root" \
    "$deltas_root" "$backups_root" "$(dirname "$lock_file")"
archive_free_bytes="$(df -PB1 "$deltas_root" | awk 'NR == 2 {print $4}')"
if [ "$archive_free_bytes" -lt "$min_archive_free_bytes" ]; then
    echo "Archive free space is $archive_free_bytes bytes; required minimum is $min_archive_free_bytes" >&2
    exit 2
fi
ingest_active_before="$(systemctl is-active pensieve-ingest 2>/dev/null || true)"
ingest_restarts_before="$(systemctl show pensieve-ingest -p NRestarts --value 2>/dev/null || true)"
if [ "$require_ingest_active" = "1" ] \
    && { [ "$ingest_active_before" != "active" ] \
        || ! [[ "$ingest_restarts_before" =~ ^[0-9]+$ ]]; }; then
    echo "pensieve-ingest is not healthy before analytics refresh: active=$ingest_active_before restarts=$ingest_restarts_before" >&2
    exit 2
fi
exec 9>"$lock_file"
if ! flock -n 9; then
    echo "Another analytics refresh holds $lock_file; leaving it in control"
    exit 0
fi

if [ ! -L "$current_link" ]; then
    echo "Current analytics generation symlink is missing: $current_link" >&2
    exit 2
fi
current_generation="$(readlink -f "$current_link")"
case "$current_generation" in
    "$generations_root"/*) ;;
    *)
        echo "Current generation escapes $generations_root: $current_generation" >&2
        exit 2
        ;;
esac
current_snapshot="$current_generation/active-raw.json"
previous_fragment="$current_generation/production-live.json"
for path in "$current_snapshot" "$previous_fragment"; do
    if [ ! -s "$path" ]; then
        echo "Current generation input is missing: $path" >&2
        exit 2
    fi
done
identity_baseline_evidence=""
if [ "$identity_enabled" = "1" ]; then
    identity_baseline_evidence="$current_generation/identity-evidence.json"
    if [ ! -s "$identity_baseline_evidence" ]; then
        echo "Current identity evidence is missing: $identity_baseline_evidence" >&2
        exit 2
    fi
    install -d -m 0750 "$identity_root"
fi

started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
run_stamp="$(date -u +%Y%m%dT%H%M%SZ)"
run_dir="$runs_root/$run_stamp-$$"
mkdir -m 0750 "$run_dir"
status_file="$run_dir/status.json"
partial_backup=""
partial_generation=""
partial_link=""

finish() {
    exit_code=$?
    if [ -n "$partial_backup" ] && [ -f "$partial_backup" ]; then
        rm -f -- "$partial_backup"
    fi
    if [ -n "$partial_generation" ] && [ -d "$partial_generation" ]; then
        partial_name="$(basename "$partial_generation")"
        if [[ "$partial_name" =~ ^\.[0-9a-f]{64}\.partial\.[0-9]+$ ]] \
            && [ "$partial_generation" = "$generations_root/$partial_name" ]; then
            rm -rf --one-file-system -- "$partial_generation"
        fi
    fi
    if [ -n "$partial_link" ] && [ -L "$partial_link" ]; then
        rm -f -- "$partial_link"
    fi
    if [ "$exit_code" -ne 0 ] && [ ! -e "$run_dir/SUCCESS" ]; then
        jq -n \
            --arg started_at "$started_at" \
            --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
            --argjson exit_code "$exit_code" \
            '{status:"failed", started_at:$started_at,
              completed_at:$completed_at, exit_code:$exit_code}' \
            >"$status_file.new"
        mv "$status_file.new" "$status_file"
    fi
}
trap finish EXIT

replacement_fragment="$run_dir/production-live.json"
target_snapshot="$run_dir/active-raw.json"
plan="$run_dir/analytics-plan.json"
advance_output="$target_snapshot"
additional_fragment=""
if [ -n "$additional_fragment_source" ]; then
    additional_fragment="$run_dir/catalog-addition.json"
    install -m 0640 "$additional_fragment_source" "$additional_fragment"
    advance_output="$run_dir/active-raw-live.json"
fi

store_id="$(jq -er '.store_id' "$current_snapshot")"
"$catalog_bin" export \
    --inventory "$live_inventory" \
    --inventory-id "$inventory_id" \
    --store-id "$store_id" \
    --output "$replacement_fragment" \
    | tee "$run_dir/catalog-export.txt"
"$catalog_bin" advance \
    --baseline "$current_snapshot" \
    --previous-fragment "$previous_fragment" \
    --replacement-fragment "$replacement_fragment" \
    --output "$advance_output" \
    | tee "$run_dir/catalog-advance.txt"
if [ -n "$additional_fragment" ]; then
    "$catalog_bin" extend \
        --baseline "$advance_output" \
        --addition "$additional_fragment" \
        --output "$target_snapshot" \
        | tee "$run_dir/catalog-extension.txt"
fi
"$catalog_bin" verify --snapshot "$target_snapshot" \
    | tee "$run_dir/catalog-verification.txt"

snapshot_id="$(jq -er '.snapshot_id' "$target_snapshot")"
snapshot_hex="${snapshot_id#sha256:}"
if ! [[ "$snapshot_hex" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Target snapshot has an invalid identity: $snapshot_id" >&2
    exit 1
fi

publish_args=(
    publish
    --snapshot "$target_snapshot"
    --s3-bucket "$s3_bucket"
    --s3-region "$s3_region"
    --s3-endpoint-url "$s3_endpoint_url"
)
if [ "${PENSIEVE_ANALYTICS_S3_FORCE_PATH_STYLE:-1}" = "1" ]; then
    publish_args+=(--s3-force-path-style)
fi
"$catalog_bin" "${publish_args[@]}" | tee "$run_dir/catalog-publication.txt"

plan_args=(--catalog "$target_snapshot")
if [ "$identity_enabled" = "1" ]; then
    plan_args+=(--query-version slice-b1-v1)
fi
"$plan_bin" "${plan_args[@]}" >"$plan.new"
mv "$plan.new" "$plan"
run_kind="$(jq -er '.run_kind' "$plan")"
planned_snapshot_id="$(jq -er '.snapshot_id' "$plan")"
if [ "$planned_snapshot_id" != "$snapshot_id" ]; then
    echo "Planner returned $planned_snapshot_id for target $snapshot_id" >&2
    exit 1
fi

promote_generation() {
    generation="$generations_root/$snapshot_hex"
    if [ -e "$generation" ]; then
        cmp "$target_snapshot" "$generation/active-raw.json"
        cmp "$replacement_fragment" "$generation/production-live.json"
        if [ -n "$additional_fragment" ]; then
            cmp "$additional_fragment" "$generation/catalog-addition.json"
        fi
        if [ "$identity_enabled" = "1" ] && [ -s "$run_dir/identity-evidence.json" ]; then
            cmp "$run_dir/identity-evidence.json" "$generation/identity-evidence.json"
        fi
    else
        partial_generation="$generations_root/.$snapshot_hex.partial.$$"
        mkdir -m 0750 "$partial_generation"
        install -m 0640 "$target_snapshot" "$partial_generation/active-raw.json"
        install -m 0640 "$replacement_fragment" "$partial_generation/production-live.json"
        if [ -n "$additional_fragment" ]; then
            install -m 0640 "$additional_fragment" "$partial_generation/catalog-addition.json"
        fi
        install -m 0640 "$plan" "$partial_generation/analytics-plan.json"
        if [ -s "$run_dir/apply.json" ]; then
            install -m 0640 "$run_dir/apply.json" "$partial_generation/apply.json"
        fi
        if [ "$identity_enabled" = "1" ]; then
            install -m 0640 "$run_dir/identity-evidence.json" \
                "$partial_generation/identity-evidence.json"
        fi
        printf '%s\n' "$snapshot_id" >"$partial_generation/APPLIED"
        sync -f "$partial_generation/active-raw.json"
        sync -f "$partial_generation/production-live.json"
        sync -f "$partial_generation/APPLIED"
        sync -f "$partial_generation"
        mv "$partial_generation" "$generation"
        sync -f "$generations_root"
        partial_generation=""
    fi
    if [ ! -s "$generation/APPLIED" ]; then
        printf '%s\n' "$snapshot_id" >"$generation/APPLIED"
    fi
    partial_link="$state_root/.current.$$"
    ln -s "generations/$snapshot_hex" "$partial_link"
    mv -Tf "$partial_link" "$current_link"
    sync -f "$state_root"
    partial_link=""
}

prune_backups() {
    mapfile -t backups < <(
        find "$backups_root" -maxdepth 1 -type f \
            -name 'slice-a-before-*.duckdb' -printf '%T@ %p\n' \
            | sort -nr | cut -d' ' -f2-
    )
    for ((index = retain_backups; index < ${#backups[@]}; index++)); do
        backup="${backups[$index]}"
        backup_name="$(basename "$backup")"
        if [[ "$backup_name" =~ ^slice-a-before-[0-9a-f]{64}\.duckdb$ ]] \
            && [ "$backup" = "$backups_root/$backup_name" ]; then
            rm -f -- "$backup"
            echo "Pruned old analytics backup: $backup"
        fi
    done
}

prune_deltas() {
    mapfile -t deltas < <(
        find "$deltas_root" -mindepth 1 -maxdepth 1 -type d \
            -printf '%T@ %p\n' | sort -nr | cut -d' ' -f2-
    )
    for ((index = retain_deltas; index < ${#deltas[@]}; index++)); do
        delta="${deltas[$index]}"
        delta_name="$(basename "$delta")"
        if [[ "$delta_name" =~ ^[0-9a-f]{64}$ ]] \
            && [ "$delta" = "$deltas_root/$delta_name" ] \
            && [ -s "$generations_root/$delta_name/APPLIED" ]; then
            rm -rf --one-file-system -- "$delta"
            echo "Pruned old verified analytics delta cache: $delta"
        fi
    done
}

case "$run_kind" in
    no_change)
        promote_generation
        jq -n \
            --arg snapshot_id "$snapshot_id" \
            --arg started_at "$started_at" \
            --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
            '{status:"no_change", snapshot_id:$snapshot_id,
              started_at:$started_at, completed_at:$completed_at}' \
            >"$status_file.new"
        mv "$status_file.new" "$status_file"
        ;;
    incremental)
        delta_root="$deltas_root/$snapshot_hex"
        stage_evidence="$run_dir/staging"
        "$stage_script" "$plan" "$delta_root" "$stage_evidence"

        as_of="$(date +%s)"
        printf '%s\n' "$as_of" >"$run_dir/as-of"
        code_version="$(git -C "$repo_root" rev-parse HEAD)"
        backup="$backups_root/slice-a-before-$snapshot_hex.duckdb"
        if [ -e "$backup" ]; then
            test -f "$backup"
            test "$(stat -Lc '%s' "$backup")" -gt 0
        else
            partial_backup="$backup.partial.$$"
            cp --reflink=auto --sparse=always --preserve=mode,timestamps \
                -- "$work_database" "$partial_backup"
            chmod 0440 "$partial_backup"
            mv "$partial_backup" "$backup"
            sync -f "$backup"
            sync -f "$backups_root"
            partial_backup=""
        fi

        incremental_args=(
            --catalog "$target_snapshot"
            --plan "$plan"
            --work-database "$work_database"
            --delta-object-root "$delta_root"
            --as-of "$as_of"
            --code-version "$code_version"
        )
        if [ "$identity_enabled" = "1" ]; then
            identity_work_root="$identity_root/$snapshot_hex"
            incremental_args+=(
                --identity-baseline-evidence "$identity_baseline_evidence"
                --identity-evidence "$run_dir/identity-evidence.json"
                --identity-work-root "$identity_work_root"
            )
        fi
        "$incremental_bin" "${incremental_args[@]}" >"$run_dir/apply.json.new"
        mv "$run_dir/apply.json.new" "$run_dir/apply.json"
        jq -e \
            --arg snapshot_id "$snapshot_id" \
            '.snapshot_id == $snapshot_id and .dry_run == false and
             (.publication.status == "published" or
              .publication.status == "already_current")' \
            "$run_dir/apply.json" >/dev/null

        promote_generation
        prune_backups
        prune_deltas
        jq -n \
            --arg snapshot_id "$snapshot_id" \
            --arg started_at "$started_at" \
            --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
            --argjson as_of "$as_of" \
            '{status:"published", snapshot_id:$snapshot_id,
              as_of_epoch:$as_of, started_at:$started_at,
              completed_at:$completed_at}' \
            >"$status_file.new"
        mv "$status_file.new" "$status_file"
        ;;
    *)
        echo "Analytics refresh requires operator action for plan kind: $run_kind" >&2
        exit 3
        ;;
esac

ingest_active_after="$(systemctl is-active pensieve-ingest 2>/dev/null || true)"
ingest_restarts_after="$(systemctl show pensieve-ingest -p NRestarts --value 2>/dev/null || true)"
if [ "$require_ingest_active" = "1" ] \
    && { [ "$ingest_active_after" != "active" ] \
        || [ "$ingest_restarts_after" != "$ingest_restarts_before" ]; }; then
    echo "pensieve-ingest changed during analytics refresh: before=$ingest_active_before/$ingest_restarts_before after=$ingest_active_after/$ingest_restarts_after" >&2
    exit 4
fi
jq \
    --arg ingest_active "$ingest_active_after" \
    --argjson ingest_restarts "${ingest_restarts_after:-0}" \
    '. + {ingest_active:$ingest_active, ingest_restarts:$ingest_restarts}' \
    "$status_file" >"$status_file.new"
mv "$status_file.new" "$status_file"
evidence_files=("$replacement_fragment" "$target_snapshot" "$plan" "$status_file")
if [ -n "$additional_fragment" ]; then
    evidence_files+=("$additional_fragment")
fi
if [ "$identity_enabled" = "1" ] && [ -s "$run_dir/identity-evidence.json" ]; then
    evidence_files+=("$run_dir/identity-evidence.json")
fi
sha256sum "${evidence_files[@]}" >"$run_dir/SHA256SUMS"
sync -f "$run_dir/SHA256SUMS"
printf 'passed\n' >"$run_dir/SUCCESS"
sync -f "$run_dir/SUCCESS"
echo "Analytics refresh complete: snapshot=$snapshot_id status=$(jq -r '.status' "$status_file")"
