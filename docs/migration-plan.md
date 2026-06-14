# Pensieve Migration Plan — Lakehouse Re-Architecture

*Status: active / executing*
*Created: 2026-06-14*
*Owner: Jeff*

This is the canonical execution plan for migrating Pensieve from its current stack
(**notepack segments + ClickHouse + per-event preview server**) to a single-node
**open lakehouse**, **without interrupting live ingestion**.

It is a companion to the target-architecture design notes. Where the design doc says
*what* we're building, this doc says *how we get there safely on a running system*.

---

## 1. Current state (what exists today)

```
relay collectors ─► RocksDB dedup ─► notepack .gz segments (source of truth)
                                          └─► ClickHouse (events_local + ~7 MVs)
                                                 ├─► pensieve-serve (32 analytics endpoints)
                                                 └─► pensieve-preview (per-event pages) ← being dropped
```

- **Archive:** notepack segments, sealed at 256 MB, gzip, framed length-prefixed.
  Source of truth. notepack is a custom binary format from a personal fork
  (`github.com/erskingardner/notepack`) — durability risk over a 10-year horizon.
- **Analytics:** ClickHouse, ~2.6 TiB on NVMe and growing. Already pre-aggregates via
  materialized views (DAU/WAU/MAU, first-seen, zaps, kinds, relay distribution).
- **Serving:** `pensieve-serve` (real, 32 endpoints — *not* a placeholder) +
  `pensieve-preview` (random-access-by-id event pages).
- **Dedup:** RocksDB, ~50 GB at 1.4 B events. Works well — **kept**.
- **Discovery:** pure NIP-65 graph crawl from 10 hardcoded seeds.
- **Sync:** negentropy (NIP-77) against a hardcoded relay set; no fallback for
  non-supporting relays; Primal unsupported, Damus flaky.

## 2. Target architecture

```
COLLECTORS (swappable)                         ║ DISCOVERY
  firehose (live REQ)                          ║   NIP-66 catalog (kind 30166/10166) ← primary
  negentropy reconcile (dynamic targets)       ║   NIP-65 graph crawl ← secondary
  windowed REQ backfill (non-NIP-77 fallback)  ║   seeds ← bootstrap
  bulk import (jsonl/proto)                     ║   (never nostr.band — it is dead)
        │
        ▼
  RocksDB dedup (kept)
        │
        ▼
  hot SQLite (today + yesterday, rolling)
        │  midnight seal: DuckDB COPY → verify → upload → drop sealed day
        ▼
  Parquet daily files on Hetzner Object Storage   ← source of truth, immutable, self-verifying
        ├─► RESEARCH: DuckDB directly over Parquet (+ optional published dataset)
        └─► METRICS:  DuckDB batch cron ─► Postgres rollups ─► Grafana + analytics API
```

**Dropped:** per-event / low-latency serving (preview), ClickHouse, notepack.
**Kept:** RocksDB dedup; the analytics/dashboard API (serves *only* small rollups).

Key design points:
- **Source of truth = Parquet on object storage.** Open, columnar, universally readable,
  self-verifying (recompute event id + check schnorr sig). Daily files,
  **partitioned by ingest date, immutable, never rewritten.** `created_at` is preserved
  in-row (research queries on `created_at` scan more files — accepted trade-off for an
  append-only archive).
- **Object storage:** **Hetzner Object Storage** (S3-compatible, ~€5/TB-mo, 1 TB egress
  included; same provider as compute). NB: Hetzner **Storage Box ≠ S3** (SFTP/SMB/WebDAV
  only). Cloudflare R2 (zero egress) only if we publish a high-traffic public dataset.
- **Distinct/rolling metrics** (WAU/MAU/retention/total-unique) use **mergeable HLL
  sketches**, not stored integer counts, so "completed days + today" composes.
  ~2% error vs. today's `uniqExact` (tolerance to confirm — see §6).
- **Success metric changed: throughput → coverage** (fraction of the global event set
  captured). Measured continuously (§ Track A).

## 3. Migration philosophy

The system is live; relays don't retain forever, so **a gap in ingestion is permanent
data loss.** We split the work by risk:

- **Storage & analytics → build new in parallel, then cut over.** Source of truth and
  dashboards can't be safely mutated in place. Run the new stack in *shadow*
  (dual-write, shadow-read), prove it equals the old one, flip readers, keep the old
  stack as a live fallback, retire it only after a sustained grace period.
- **Collection → swap incrementally in place.** Every collector/discovery improvement is
  *additive* to the same deduped stream. Nothing to cut over; new collectors just raise
  coverage. Low risk.

**Four rules that hold across every phase:**
1. **Ingest never stops.** New paths are added beside old ones; nothing is removed until
   its replacement is proven in production.
2. **Nothing is deleted on cutover.** Superseded data is archived cold and kept for a
   defined retention (§6) as belt-and-suspenders.
3. **The crash-safety contract changes last.** notepack keeps driving dedup /
   `mark_archived` until Parquet is fully proven. Changing *when* an event is considered
   durable is the single riskiest change — it happens once, deliberately, in P5.
4. **Every phase is independently shippable, reversible, and leaves the system working.**

### Shape

```
P0  FOUNDATION (precedes everything)

TRACK A — COLLECTION  (incremental, additive, NO cutover)
    P1  coverage metrics → NIP-66 catalog → dynamic negentropy → windowed backfill

TRACK B — STORAGE & ANALYTICS  (parallel build → verify → cutover)
    P2  hot SQLite + Parquet seal + backfill   ║ notepack STILL source of truth
    P3  DuckDB → Postgres rollups (shadow)      ║ ClickHouse STILL serving
    P4  flip readers → new stack                ║ ClickHouse kept as fallback
    P5  retire notepack-write + ClickHouse, reclaim 2.6 TiB   ← contract change here

Tracks A and B run in parallel; they converge only at P5.
```

---

## 4. Phases

### P0 — Foundation & breathing room  *(low risk)*

Make aggressive collection safe *before* doing it, and lay the rails for Track B.

- [x] Disk relief — breathing room already achieved (a few weeks of runway).
- [ ] (If not already) ClickHouse tiered storage: cold parts → HDD, to keep NVMe headroom
      while the old stack still runs.
- [ ] Stand up the Hetzner Object Storage bucket; confirm DuckDB reads/writes `s3://`
      against it (round-trip a test Parquet file).
- [ ] Skeleton of the **old-vs-new verification harness** (later phases plug in).
- [ ] Confirm backups: notepack archive, `relay-stats.db`, RocksDB dedup index.

**Gate:** NVMe headroom OK · DuckDB round-trips the bucket · backups confirmed.
**Rollback:** trivial — nothing meaningful removed.

### P1 — Collection & coverage  *(Track A; additive; priority)*

- [ ] **Coverage instrumentation first.** Two metrics on existing Grafana, measured at a
      baseline before anything changes:
  - *Reference-coverage:* sample event ids referenced in `e`/`a`/`q` tags of events we
    have, check RocksDB membership → `have / referenced` %.
  - *Catalog-coverage:* `connected / reconciled / known` relays (known = from NIP-66).
- [ ] **NIP-66 catalog consumer.** Subscribe to kind **10166** (monitor announcements →
      discover monitors + frequency) and kind **30166** (relay discovery) from a trusted
      monitor set (nostr.watch primary). Persist a `relay_catalog`:
      URL (`d`), supported NIPs (`N`), network (`n`), RTT (`rtt-*`), requirements (`R`),
      accepted kinds (`k`), geohash (`g`), source monitors, last-seen.
      **Aggregate across multiple monitors** — NIP-66 risk mitigation says never trust a
      single monitor (misconfig/malice); use quorum / web-of-trust.
      These are ordinary Nostr events, so they flow through normal ingest + archive too.
- [ ] **Dynamic negentropy targeting.** Choose reconciliation targets from catalog `N`
      tags advertising NIP-77; retire the hardcoded list; drop Primal.
- [ ] **Windowed REQ backfill collector.** Paginate by `since`/`until` time slices walking
      backward — the completeness fallback for relays that don't support NIP-77.
- Each collector behind a **feature flag**.

**Gate:** coverage metrics rise measurably; no ingest-stability regression.
**Rollback:** flip the flag; prior behavior untouched.

### P2 — Parquet archive in parallel  *(Track B; the delicate track)*

- [ ] Add the **hot SQLite** store (today + yesterday rolling window). Writes are
      **non-blocking / batched** so they never slow the ingest hot path.
      *notepack still drives dedup and crash-safety.* Use a per-day table/file and drop it
      after seal — never a daily multi-million-row `DELETE` (bloats SQLite without VACUUM).
- [ ] **Midnight seal job (00:05):** for *yesterday*, `DuckDB COPY` from
      `sqlite_scan('hot.db', ...)` → `YYYY-MM-DD.parquet` (zstd, `ORDER BY created_at`,
      row-group 100k) → verify → upload to object storage → **only then** drop the day.
- [ ] **`backfill-parquet` binary:** read existing notepack segments → one Parquet file
      per ingestion day over all history (partition by approximate ingest date from
      segment mtime; `created_at` preserved in-row). Run once.
- [ ] **Verification (automated):** per-day row counts vs notepack/ClickHouse; sample N
      events/day, recompute id + verify sig + confirm byte-identical reconstruction;
      kind/pubkey distribution spot-checks.

**Gate:** every historical day + N go-forward days verify Parquet == notepack exactly.
**Rollback:** stop the seal job — Parquet is shadow-only; notepack unaffected.

### P3 — New analytics in parallel  *(Track B)*

- [ ] Stand up **Postgres** rollup store.
- [ ] **DuckDB batch jobs** reproducing all 32 endpoints' metrics from Parquet.
      **HLL sketches** for distinct/rolling metrics. "Today so far" from hot SQLite +
      live sketches.
- [ ] **Verification harness:** diff each endpoint old (ClickHouse) vs new (Postgres)
      across parameter ranges — additive metrics **exact**, distinct metrics within the
      agreed tolerance (§6).

**Gate:** all endpoints match within tolerance for a sustained window (≈1–2 weeks).
**Rollback:** new stack is shadow; readers still on ClickHouse.

### P4 — Cut over the readers  *(Track B)*

- [ ] Point `pensieve-serve` + Grafana at Postgres rollups.
- [ ] **Keep ClickHouse running, read-only, as instant fallback.**

**Gate:** stable through grace period (§6), no correctness issues.
**Rollback:** flip readers back to ClickHouse (still alive).

### P5 — Retire & reclaim  *(only after sustained proof)*

- [ ] Stop writing notepack; Parquet becomes the sole go-forward source of truth.
      **Crash-safety contract changes here:** `mark_archived` now keys off the durable
      hot-SQLite write (`synchronous=FULL`) or the verified seal. Done last, deliberately.
- [ ] Archive existing notepack segments **cold** to object storage; do **not** delete for
      the retention window (§6).
- [ ] Decommission ClickHouse → reclaim ~2.6 TiB NVMe. Drop the preview server.

**Gate:** fully on the new stack for a sustained period before anything old is deleted.
**Rollback:** hardest here — mitigated by the long grace periods and cold-kept notepack.

---

## 5. Verification

The diff harness (seeded in P0) is what makes cutovers trustworthy:

- **Archive (P2):** counts + sample id/sig recompute + byte-identical reconstruction,
  per day, Parquet vs notepack/ClickHouse. The archive is *self-verifying* — event id is
  the hash of its own content — so this doubles as an ongoing integrity sweep.
- **Analytics (P3):** per-endpoint, per-parameter diff old vs new. Additive = exact;
  distinct/rolling = within tolerance. Automate as a repeatable test, run continuously
  through the shadow window.
- **Coverage (P1):** before/after each collector change; the metric must move.

## 6. Parameters to lock

| Parameter | Default | Notes |
|---|---|---|
| Distinct-metric tolerance | ~2% (HLL) | If exact required: store per-day pubkey sets (bigger, exact) |
| P3 shadow window | ≈2 weeks | Old vs new must match throughout |
| P4 fallback-kept window | ≈2–4 weeks | ClickHouse stays read-only |
| notepack cold retention | 6–12 months | After P5, before any deletion |
| Tracks A & B | parallel | A now; B's P2 once P0 lands |

## 7. Risk register

| Risk | Mitigation |
|---|---|
| Event loss during migration | Ingest never stops; new paths additive/dual-write; collectors flagged |
| Archive corruption/loss | notepack stays source of truth until Parquet proven; cold-kept; self-verifying |
| Wrong analytics after cutover | Parallel shadow + diff harness + tolerance; ClickHouse fallback live |
| Ingest hot-path regression | Hot-store writes non-blocking/batched; load-test before P2 gate |
| Crash-safety/durability mismatch | Defer contract change to P5; notepack drives dedup until then |
| Collection outpaces old-stack disk | P0 tiering/relief; monitor NVMe through Track A |
| Bad NIP-66 monitor (misconfig/malice) | Aggregate multiple monitors; quorum / web-of-trust |
| `created_at` query performance | Partition by ingest date, append-only; `created_at` in-row; documented trade-off |

## 8. Status tracker

- **P0 Foundation** — in progress (disk relief done; bucket/harness/backups pending)
- **P1 Collection & coverage** — not started (next up)
- **P2 Parquet archive** — not started
- **P3 New analytics** — not started
- **P4 Reader cutover** — not started
- **P5 Retire & reclaim** — not started

---

## Related
- Target-architecture design notes (the *what*)
- `docs/ingestion_pipeline.md` — current pipeline architecture
- Open questions: negentropy strategy, relay-discovery strategy (tracked separately)
