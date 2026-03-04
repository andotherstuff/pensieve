#!/usr/bin/env bash
# =============================================================================
# Phase 0 Baseline Capture (read-only)
# =============================================================================
# Run during normal traffic for 30-60 minutes.
# Save output to timestamped files for before/after comparison.
# =============================================================================
# Re-exec with bash if invoked as sh (arrays require bash)
[ -n "${BASH_VERSION:-}" ] || exec bash "$0" "$@"

set -u

if command -v rg >/dev/null 2>&1; then
  FILTER_CMD=(rg -i)
else
  FILTER_CMD=(grep -Ei)
fi

if command -v just >/dev/null 2>&1; then
  CH_QUERY_CMD=(just ch-query)
elif command -v clickhouse-client >/dev/null 2>&1; then
  CH_QUERY_CMD=(clickhouse-client --query)
else
  echo "Error: need either 'just' or 'clickhouse-client' in PATH." >&2
  exit 1
fi

TS="$(date +%Y-%m-%d_%H%M%S)"
OUT_DIR="$HOME/pensieve-baselines/$TS"
mkdir -p "$OUT_DIR"
echo "Writing baseline output to: $OUT_DIR"
# -----------------------------------------------------------------------------
# 1) Host CPU/process snapshot
# -----------------------------------------------------------------------------
{
  echo "=== uptime ==="
  uptime
  echo
  echo "=== top (single sample) ==="
  top -b -n 1 | "${FILTER_CMD[@]}" "cpu|mem|load"
  echo
  echo "=== top processes by CPU ==="
  ps -Ao pid,pcpu,pmem,comm | sort -k2 -nr | head -n 20
} > "$OUT_DIR/host_snapshot.txt"
# -----------------------------------------------------------------------------
# 2) ClickHouse heavy queries (last 1h)
# -----------------------------------------------------------------------------
"${CH_QUERY_CMD[@]}" "
SELECT
  normalized_query_hash,
  count() AS runs,
  round(sum(query_duration_ms)/1000, 2) AS total_sec,
  sum(read_rows) AS read_rows,
  sum(read_bytes) AS read_bytes,
  any(query) AS sample_query
FROM system.query_log
WHERE event_time >= now() - INTERVAL 1 HOUR
  AND type = 'QueryFinish'
  AND query_kind = 'Select'
GROUP BY normalized_query_hash
ORDER BY total_sec DESC
LIMIT 30
" > "$OUT_DIR/ch_top_queries_1h.txt"
# -----------------------------------------------------------------------------
# 3) Filter likely expensive patterns (events_local / uniqExact / arrayExists / FINAL)
# -----------------------------------------------------------------------------
"${CH_QUERY_CMD[@]}" "
SELECT
  round(sum(query_duration_ms)/1000, 2) AS total_sec,
  count() AS runs,
  any(query) AS sample_query
FROM system.query_log
WHERE event_time >= now() - INTERVAL 1 HOUR
  AND type = 'QueryFinish'
  AND (
    query ILIKE '%events_local%'
    OR query ILIKE '%uniqExact%'
    OR query ILIKE '%arrayExists%'
    OR query ILIKE '%FINAL%'
  )
GROUP BY normalizeQuery(query)
ORDER BY total_sec DESC
LIMIT 30
" > "$OUT_DIR/ch_hot_patterns_1h.txt"
# -----------------------------------------------------------------------------
# 4) Refreshable MV health
# -----------------------------------------------------------------------------
"${CH_QUERY_CMD[@]}" "
SELECT
  view,
  status,
  last_refresh_time,
  next_refresh_time
FROM system.view_refreshes
WHERE database = currentDatabase()
ORDER BY view
" > "$OUT_DIR/ch_mv_refresh_status.txt"
# -----------------------------------------------------------------------------
# 5) Target endpoint timings
# -----------------------------------------------------------------------------
{
  echo "=== endpoint timings ==="
  curl -s -o /dev/null -w "kinds_activity %{time_total}s\n" \
    "http://localhost:8080/api/v1/kinds/1/activity?group_by=day&limit=30"
  curl -s -o /dev/null -w "engagement %{time_total}s\n" \
    "http://localhost:8080/api/v1/stats/engagement?days=30"
  curl -s -o /dev/null -w "hourly_activity %{time_total}s\n" \
    "http://localhost:8080/api/v1/stats/activity/hourly?days=30"
} > "$OUT_DIR/api_endpoint_timings.txt"
# -----------------------------------------------------------------------------
# 6) Cache-related headers (with full response headers)
# -----------------------------------------------------------------------------
{
  echo "=== engagement headers ==="
  curl -s -D - -o /dev/null \
    "http://localhost:8080/api/v1/stats/engagement?days=30" \
    | "${FILTER_CMD[@]}" "HTTP/|cache-control|etag|age"
  echo
  echo "=== kind activity headers ==="
  curl -s -D - -o /dev/null \
    "http://localhost:8080/api/v1/kinds/1/activity?group_by=day&limit=30" \
    | "${FILTER_CMD[@]}" "HTTP/|cache-control|etag|age"
} > "$OUT_DIR/api_cache_headers.txt"
# -----------------------------------------------------------------------------
# 7) API cache hit/miss logs (last 60 min)
# -----------------------------------------------------------------------------
if command -v journalctl >/dev/null 2>&1; then
  journalctl -u pensieve-api --since "60 min ago" \
    | "${FILTER_CMD[@]}" "cache hit|cache miss|warn|error|timeout" \
    > "$OUT_DIR/api_cache_log_60m.txt"
else
  echo "journalctl not found on this host." > "$OUT_DIR/api_cache_log_60m.txt"
fi
echo "Done. Files:"
ls -1 "$OUT_DIR"
