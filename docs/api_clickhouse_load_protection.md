# ClickHouse API load protection

This change affects the API client only, not ingestion or ClickHouse server defaults.

- Two executing database calls; sixteen additional callers may wait up to two seconds.
  Admission failures return HTTP 503 `analytics_busy`.
- Each query sends `max_threads=4`, `max_execution_time=20`,
  `timeout_overflow_mode=throw`, and disconnect cancellation. Server execution
  deadlines are cooperative, not hard wall-clock guarantees.
- A detached database task retains its permit until completion even if its HTTP
  caller disconnects. Detached cache refreshes likewise retain their per-key lock
  and can populate the cache after an HTTP timeout. No unbounded task queue is used
  for database admission.
- Per-entry freshness is no longer capped by a global five-minute TTL. Aggregate
  responses may be served stale for one additional endpoint TTL while refreshing;
  real-time watermarks never use this stale allowance.
- Kind activity now means the most recent N calendar periods, including the current
  period, rather than the last N nonempty periods across all history. Future events
  are excluded. Existing group and limit caps remain.
- A configured ingestion watermark serves latest-event requests on ClickHouse too.
  A missing/stale/invalid configured watermark fails closed, without a raw-scan fallback.
- Earliest-event queries share a one-hour cache with overview requests. The first
  cold lookup still needs a bounded database query; no genesis timestamp is fabricated.
- ClickHouse engagement temporarily returns HTTP 503 `metric_unavailable` pending
  an efficient aggregate. This does not enable or select Postgres.

Before production: pass `just precommit`, build the exact commit, and canary locally
on the server with the production configuration but a localhost-only alternate port.
Check query settings, concurrency, cancellation, warm-cache latency, watermark
freshness, and deliberate overload responses. Back up the API service override and
restart only the API on promotion. Preserve the previous binary for rollback.
Verify ingestion sealing/indexing/Parquet publication and restart count afterwards.

Deployment status: not yet deployed.
