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

## Deployment: 2026-09-14

- Code commit: `7ac5121`; full `just precommit` passed.
- Linux binary SHA-256: `fed87958c79c5531343ac145a3e2a2b761fd0dee1a497c53912e5419070a54bb`.
- Release: `/home/pensieve/pensieve/releases/api-7ac5121/pensieve-serve`.
- One-CPU build completed in 58 seconds with 1.3 GiB peak memory.
- Localhost-only canary on port 18082 passed; stopped after promotion.
- Root-only rollback override:
  `/etc/pensieve/api-cutover-backups/20260914-load-7ac5121/90-postgres-cutover.conf`.
- Only `pensieve-api` restarted. Ingestion PID 1683 and NRestarts=0 unchanged.
- Live smoke checks: latest-event 200; kind-1 activity 200/143 ms cold;
  overview 200/583 ms cold and 1 ms cached; active-users 200/4 ms.
  Engagement returns intended 503. Canary additionally checked earliest-event,
  kinds listing, weekly/monthly kind activity, new users and throughput.
- Archive free 751 GiB; data free 483 GiB. Recent segment 21725 published
  33,387 Parquet rows from 33,387 events, zero rejected.
- Observed load was 0.98/8.13/58.04 after deployment. Load was already falling
  before deployment; this is not a causal benchmark or sustained-load proof.

The cancellation/concurrency invariant has unit-test coverage; a broader live
load/overload soak remains distinct from these endpoint smoke checks.
