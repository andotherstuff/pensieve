# Pensieve Migration Plan — Lakehouse Re-Architecture

*Status: active / executing*
*Created: 2026-06-14*
*Last updated: 2026-07-28*
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
                                                 ├─► pensieve-serve (24 ClickHouse-backed analytics routes)
                                                 └─► pensieve-preview (per-event pages) ← being dropped
```

- **Archive:** notepack segments, sealed at 256 MB, gzip, framed length-prefixed.
  Source of truth. notepack is a custom binary format from a personal fork
  (`github.com/erskingardner/notepack`) — durability risk over a 10-year horizon.
- **Analytics:** ClickHouse, ~2.6 TiB on NVMe and growing. Already pre-aggregates via
  materialized views (DAU/WAU/MAU, first-seen, zaps, kinds, relay distribution).
- **Serving:** `pensieve-serve` (30 GET routes: 24 ClickHouse-backed analytics,
  3 relay-operational SQLite routes, authenticated ping, and 2 public service
  routes) + `pensieve-preview` (random-access-by-id event pages).
- **Dedup:** RocksDB, ~50 GB at 1.4 B events. It currently filters the global
  ingest stream and participates in archive state transitions. It is retained
  during migration, but the target archive must remain correct if this
  rebuildable efficiency index is lost.
- **Discovery:** NIP-65 graph crawl plus the first NIP-66 catalog consumer.
- **Sync:** negentropy (NIP-77) against the configured base set plus dynamic
  NIP-66 catalog targets; windowed REQ fallback for non-NIP-77 relays is still
  pending.

## 2. Target architecture

```
COLLECTORS (swappable)                         ║ DISCOVERY
  firehose (live REQ)                          ║   NIP-66 catalog (kind 30166/10166) ← primary
  negentropy reconcile (dynamic targets)       ║   NIP-65 graph crawl ← secondary
  windowed REQ backfill (non-NIP-77 fallback)  ║   seeds ← bootstrap
  bulk import (jsonl/proto)                     ║   (never nostr.band — it is dead)
        │
        ▼
  hot-path RocksDB dedup (retained initially; rebuildable)
        │
        ▼
  durable live batch buffer / WAL (implementation chosen in P2)
        │  close on size / age / shutdown: shared V1 writer → validate → publish
        ▼
  Parquet batch files on Hetzner Object Storage   ← target source of truth, immutable
        ├─► RESEARCH: DuckDB directly over Parquet (+ optional published dataset)
        └─► METRICS:  DuckDB batch cron ─► Postgres rollups ─► Grafana + analytics API
```

**Eventually dropped:** per-event / low-latency serving (preview), ClickHouse,
notepack.
**Retained initially:** RocksDB hot-path dedup and the analytics/dashboard API
(serving *only* small rollups). RocksDB is an optimization, not part of the
canonical lake identity model.

Key design points:
- **Source of truth = Parquet on object storage.** Open, columnar, universally readable,
  and independently verifiable (recompute event ID + check Schnorr signature). Files
  are immutable operational batches, sealed by size, age, backfill work unit, or
  orderly shutdown; they are never rewritten in place. Mixed `created_at` values are
  expected. Every file is sorted internally and carries native row-group statistics.
  Later compaction improves physical query layout without changing the canonical rows.
  The accepted V1 schema, footer metadata, and lifecycle in §3 are covered by
  conformance tests.
- **Three independent archive workloads share one implementation:** (1) a
  resumable historical converter from sealed notepack segments, (2) the live
  sink fed by every collector, and (3) a later background optimizer that
  compacts and reclusters immutable Parquet inputs. They share the V1
  writer, row-validation rules, and publication rules, but not a single queue,
  checkpoint, or failure domain.
- **Correctness is keyed by event ID, not file placement.** A live file may mix
  old, current, and future `created_at` values. Raw files may overlap and may
  contain the same event in different files. Open-batch dedup keeps one ID at
  most once per file; hot-path dedup suppresses relay fan-in; queries and
  compaction provide logical lake dedup across files.
- **Object storage:** **Hetzner Object Storage** (S3-compatible, ~€5/TB-mo, 1 TB egress
  included; same provider as compute). NB: Hetzner **Storage Box ≠ S3** (SFTP/SMB/WebDAV
  only). Cloudflare R2 (zero egress) only if we publish a high-traffic public dataset.
- **Distinct/rolling metrics** (WAU/MAU/retention/total-unique) use **mergeable HLL
  sketches** (Apache DataSketches end-to-end — see P3), not stored integer counts, so
  "completed days + today" composes. ~2% error vs. today's `uniqExact` (tolerance to
  confirm — see §7).
- **Success metric changed: throughput → coverage** (fraction of the global event set
  captured). Measured continuously (§ Track A).

## 3. Archive format — accepted V1 specification

Every production-candidate file written by the live sink, historical converter,
repair/import tools, or compactor follows the
[Canonical Nostr Parquet Archive Format](parquet_archive_format.md). That
document is the accepted V1 schema, validation, footer-metadata,
file-lifecycle, data-lake, and compaction contract.

### 3.1 Exact V1 schema

A conforming file contains exactly these seven columns, in this order:

```text
message nostr_event_archive_v1 {
  required fixed_len_byte_array(32) id;
  required fixed_len_byte_array(32) pubkey;
  required int64 created_at (INTEGER(64, false));
  required int32 kind (INTEGER(16, false));

  required group tags (LIST) {
    repeated group list {
      required group element (LIST) {
        repeated group list {
          required binary element (STRING);
        }
      }
    }
  }

  required binary content (STRING);
  required fixed_len_byte_array(64) sig;
}
```

All fields and list elements are required and non-null. `id`, `pubkey`, and
`sig` are raw bytes. `created_at` and `kind` use unsigned logical annotations;
no staging, writer, reader, or compaction path may narrow `created_at` through
a signed 64-bit or floating-point value.

The outer `tags` list may be empty. Each inner tag must contain at least one
string, so one-element tags such as `["alt"]` are valid but `[]` as an inner
tag is not. Tag order, value order, duplicates, and empty strings are
preserved. `content` is exact UTF-8 and may be `""`; whitespace, newlines,
Unicode, and embedded JSON are not trimmed, normalized, or reserialized.

Additional top-level fields observed on a Nostr event are not stored in this
canonical row. They are not committed to by the event ID and may differ between
observations. Exact wire JSON, relay provenance, and receive time belong in a
separate observation dataset keyed by `id` if Pensieve later needs them.

### 3.2 Validation, identity, and ordering

Every row must pass complete NIP-01 event-ID recomputation and BIP-340 signature
verification. A file contains each event ID at most once. If more than one valid
signature was observed for the same ID, the lexicographically smallest raw
64-byte signature is retained.

Duplicate IDs may still occur across independently produced files, backfills,
imports, repairs, and pre-compaction source files. The logical lake is therefore
the union of validated rows keyed by `id`; query and compaction code must not
treat raw row count as unique-event count.

Rows in every file are sorted by unsigned `created_at`, then raw lexicographic
`id`. This ordering is local to a file. Neither a filename nor file membership
claims completeness for a `created_at` interval.

### 3.3 Parquet profile and metadata

V1 uses:

- the exact standard three-level `LIST` encoding shown above;
- Data Page V1;
- Zstandard column-chunk compression;
- only `PLAIN`, `RLE`, and `RLE_DICTIONARY` encodings; and
- exactly one required custom footer entry:
  `nostr.event_archive.version = "1"`.

Writers target approximately 128 MiB of uncompressed data per row group and
approximately 128 MiB to 1 GiB per sealed file. These are operational defaults,
not conformance requirements. A row group is a horizontal subset of file rows
with one column chunk per column. Every row group must have correct
`created_at` `min`, `max`, and `null_count = 0` statistics so DuckDB and other
engines can skip irrelevant groups even when a file spans many event dates.

V1 deliberately does not add footer keys for row count, event-time range,
kinds, filter/completeness claims, receive time, relay provenance, generator,
lineage, checksums, or commitments. Native Parquet metadata already describes
the schema, row count, row groups, encodings, compression, writer, and column
statistics. Distribution and lineage belong in an external object catalog or
manifest.

### 3.4 Shared writer and conformance gate

Live sealing, historical backfill, repair, import, and compaction must use one
shared V1 writer and canonical row-validation implementation with direct
control over the exact Parquet physical schema, logical annotations,
nested-list encoding, page version, encodings, compression, statistics, and
footer metadata. A separately implemented conformance validator must reopen
and inspect every output. The concrete writer library is a P0 implementation
decision, not a format decision.

DuckDB is a reader, query, and analytics engine in this architecture—not the
canonical file writer. Compaction may use DuckDB to scan and transform input,
but output must flow through the same shared V1 writer as live sealing and
backfill.

V1 acceptance and conformance status:

- [x] Accept the V1 archive-format specification (accepted 2026-07-26).
- [x] Select and prototype the canonical writer implementation with the required
      low-level Parquet controls.
- [x] Implement schema inspection plus full row validation as a reusable library
      and CLI.
- [x] Build golden valid fixtures covering empty tags, one-element tags,
      variable-length tags, empty tag values, empty and whitespace-only
      `content`, Unicode, multiline strings, `kind` boundaries, and
      `created_at` values on both sides of the signed-64 boundary.
- [x] Build invalid fixtures covering nulls, empty inner tags, wrong fixed-byte
      lengths, bad IDs, bad signatures, duplicate IDs, incorrect ordering,
      incorrect row-group statistics, missing/duplicate version metadata, and
      truncated footers.
- [x] Prove round trips through the canonical writer, the independent validator,
      DuckDB as a reader, and at least one second Parquet reader without logical
      value changes.

Implementation checkpoint (2026-07-23): `pensieve-parquet` now owns the typed
raw/notepack validation boundary, deterministic writer, strict validator
library/CLI, and reproducible valid/invalid corpus. `pensieve-lake` now owns the
separate work-unit journal, object inventory, target-sized campaign, and
immutable local/S3 publisher. `pensieve-ingest` can fan durably sealed notepack
work units to the same machinery as an optional live shadow without changing
notepack's authority over RocksDB archival state. The valid fixture decodes
identically through the Rust validator, DuckDB, and PyArrow. Real local segment
results are recorded in
[Parquet writer prototype benchmark](parquet_writer_benchmark.md). The archive
format was accepted as V1 on 2026-07-26 after this implementation and
interoperability validation. The acceptance rerun regenerated the fixture
corpus, passed 18 focused Rust conformance and recovery tests, and decoded the
valid fixture identically through DuckDB 1.5.5 and PyArrow 25.0.0.

A production-sized 256 MiB sealed notepack segment was also replayed through
the release campaign on 2026-07-26. It produced 389,695 validated rows in one
168.4 MiB Parquet object with three approximately 128 MiB-or-smaller
uncompressed row groups in 35.80 seconds, using 1,008 MiB maximum RSS. Details
and reproduction steps are in the benchmark document.

## 4. Migration philosophy

The system is live; relays don't retain forever, so **a gap in ingestion is permanent
data loss.** We split the work by risk:

- **Storage & analytics → build new in parallel, then cut over.** Source of truth and
  dashboards can't be safely mutated in place. Run the new stack in *shadow*
  (dual-write, shadow-read), prove it equals the old one, flip readers, keep the old
  stack as a live fallback, retire it only after a sustained grace period.
- **Collection → swap incrementally in place.** Every collector/discovery improvement is
  *additive* to the same deduped stream. Nothing to cut over; new collectors just raise
  coverage. Low risk.

**Five rules that hold across every phase:**
1. **Ingest never stops.** New paths are added beside old ones; nothing is removed until
   its replacement is proven in production.
2. **Nothing is deleted on cutover.** Superseded data is archived cold and kept for a
   defined retention (§7) as belt-and-suspenders.
3. **The crash-safety contract changes last.** notepack remains the archive
   commit authority and sole owner of `mark_archived` until Parquet is fully
   proven. Changing *when* an event is considered durable is the single
   riskiest change—it happens once, deliberately, in P5.
4. **Every phase is independently shippable, reversible, and leaves the system working.**
5. **Historical position is never inferred from `created_at`.** Migration boundaries
   are sealed notepack segment identities and checksums; live files are operational
   batches, not event-time partitions.

### Shape

```text
P0  FOUNDATION FOR TRACK B
    accepted V1 → writer + independent validator → object-storage round trip

TRACK A — COLLECTION  (incremental, additive, no cutover)
    P1  coverage → NIP-66 catalog → dynamic negentropy → windowed REQ
                               │
                               └──────── all collectors feed one archive sink

TRACK B — STORAGE & ANALYTICS  (parallel build → verify → cutover)
    P2a shared archive foundation
    P2b live shadow Parquet starts; notepack still authoritative
    P2c sealed notepack → Parquet historical conversion
    P2d verify the historical + live union and keep it current
    P3  optional compaction + DuckDB → Postgres rollups (shadow)
    P4  flip readers; ClickHouse remains current as fallback
    P5  make Parquet the durability authority; keep notepack shadow-writing
    P6  retire notepack, then ClickHouse, after separate fallback windows
```

P0 gates P2, not work already underway in P1. Track A and Track B run in
parallel. Live REQ, negentropy, windowed REQ, and bulk imports are merely
different producers for the same P2 live sink; there is no collector-specific
Parquet format or writer.

---

## 5. Phases

### P0 — Foundation & breathing room  *(low risk)*

Maintain enough old-stack headroom while laying the rails required before P2
can start. P1 collection work already underway does not wait for P0.

- [x] Disk relief — breathing room already achieved (a few weeks of runway).
- [ ] (If not already) ClickHouse tiered storage: cold parts → HDD, to keep NVMe headroom
      while the old stack still runs.
- [x] Prove real Hetzner Object Storage compatibility in a temporary bucket:
      conditionally upload a golden V1 file and production-sized real-segment
      output, `HEAD`-verify size/SHA-256 metadata, download them, and read them
      through the independent validator, DuckDB, and PyArrow. Completed
      2026-07-26; exact results are in the benchmark document.
- [ ] Provision the durable production bucket, prefix, and scoped credentials.
      Before the historical campaign, compare the current single-request upload
      with a bounded-concurrency resumable multipart path from the production
      host.
- [x] Complete the archive-format acceptance and local conformance gate.
- [ ] Skeleton of the **old-vs-new verification harness** (later phases plug in).
- [ ] Confirm backups: notepack archive, `relay-stats.db`, RocksDB dedup index.

**Gate:** NVMe headroom OK · V1 design accepted · golden file round-trips the bucket and
all selected readers · backups confirmed.
**Rollback:** trivial — nothing meaningful removed.

### P1 — Collection & coverage  *(Track A; additive; priority)*

- [x] **Initial coverage instrumentation.**
  - *Reference-coverage:* sample event IDs referenced in `e` and `q` tags of
    events we have, check RocksDB membership → `have / referenced` %. Address
    references in `a` tags are deliberately not part of the current
    event-ID-based metric.
  - *Catalog visibility:* gauges for known catalog relays, relays advertising
    NIP-77, and known monitors. `connected / reconciled / known` is a desired
    follow-on metric, not the meaning of the gauges implemented today.
- [x] **NIP-66 catalog consumer (first slice).** Subscribe to kind **10166**
      (monitor announcements) and kind **30166** (relay discovery) from
      configured monitor relays (default bootstrap:
      `wss://relay.nostr.watch`). Persist the catalog data needed for relay and
      NIP-77 discovery. These are ordinary Nostr events, so they flow through
      normal ingest and archive too.
- [ ] **Harden NIP-66 trust and aggregation.** Retain source-monitor identity and
      combine claims from multiple monitors using an explicit quorum /
      web-of-trust policy; do not treat one monitor as authoritative.
- [x] **Dynamic negentropy targeting.** Add targets from catalog `N` tags
      advertising NIP-77 to the configured base relay set. Retirement of the
      static base list and any relay-specific exclusions are follow-on
      operational decisions.
- [ ] **Windowed REQ backfill collector.** Paginate by `since`/`until` time slices walking
      backward — the completeness fallback for relays that don't support NIP-77.
- Each collector behind a **feature flag**.

**Gate:** coverage metrics rise measurably; no ingest-stability regression.
**Rollback:** flip the flag; prior behavior untouched.

### P2 — Build the Parquet archive in parallel  *(Track B; the delicate track)*

P2 is four deliberately separate pieces. Historical conversion is a finite
migration campaign. Live capture is a permanent ingest path. Publication is
shared infrastructure. Compaction is not required to finish P2.

#### P2a — Shared archive foundation

- [x] Implement one **V1 writer and canonical row-validation library** used by
      the converter, live sink, imports, repairs, and future compactor. The
      callers supply rows and work-unit identity; the library owns semantic
      validation, one-ID-per-file enforcement, deterministic signature
      selection, unsigned `(created_at, id)` sorting, and exact physical
      encoding. The independent conformance validator, not the writer itself,
      performs the final reopen-and-inspect gate.
- [x] Implement an idempotent **publication state machine** with states such as
      `open`, `writing`, `uploaded`, `validated`, `published`, and
      `source_committed`. It writes to unique object keys, confirms remote
      durability, and can safely resume or repeat every transition after a
      crash. Publication state is separate from RocksDB seen/dedup state.
- [x] Maintain an external **object inventory** recording object key, byte size,
      checksum, publication state, writer version, and operational job/work-unit
      identity. Once compaction exists, the inventory must distinguish:
  - active raw objects;
  - active compacted objects;
  - superseded objects retained for rollback; and
  - incomplete, orphaned, or quarantined objects that queries must ignore.

      Activating a replacement set is an atomic inventory operation. These are
      operational facts, not canonical V1 footer fields. Filenames and
      directories make no event-time completeness claim.
- [ ] Keep the three deduplication responsibilities explicit:
  1. **Open batch:** required—one event ID appears at most once in a file.
  2. **Hot ingest:** retain the existing global RocksDB index initially to
     suppress duplicate relay deliveries. Treat it as a rebuildable efficiency
     index: losing it may produce extra cross-file duplicates, but must not lose
     canonical events.
  3. **Logical lake:** readers and compaction union active files by `id` and
     apply the deterministic signature rule. Physical row counts are never
     unique-event counts.

      Historical conversion uses migration-local checkpoints and work-unit
      deduplication; it must not pass old events through the production RocksDB
      filter, where those IDs are already marked seen.

#### P2b — Start the live shadow writer first

- [ ] Add one **live Parquet sink** after event validation and the current
      hot-path dedup, shared by firehose REQ, negentropy, windowed REQ, and bulk
      imports. There is no separate negentropy archive writer. During P2 the
      same accepted stream continues into notepack, which remains authoritative
      and remains the sole owner of the RocksDB `mark_archived` transition.
  - [x] The live daemon's firehose and negentropy paths now share one downstream
        sealed-notepack shadow worker. Windowed REQ and bulk-import integration
        remain pending with those producer paths.
- [ ] Give the live sink a small durable, appendable **batch buffer / WAL** so
      relay I/O never depends on appending to a Parquet file. SQLite, a simple
      WAL, or a temporary notepack spool are implementation candidates; the
      choice is not part of V1. It must preserve the seven fields losslessly,
      including unsigned `created_at` values above `i64::MAX`, and expose
      recoverable batch state. A rolling live-query window is optional and must
      not be coupled to canonical archive correctness.
- [x] Close live batches on target size/row count, maximum age, or orderly
      shutdown—not on an event's `created_at`. For each frozen batch:
  1. validate and deduplicate by `id`;
  2. apply the deterministic signature-variant rule;
  3. sort by unsigned `(created_at, id)`;
  4. write one or more temporary files through the shared V1 writer;
  5. reopen each output with the independent validator;
  6. upload under unique object keys and confirm remote durability; and
  7. publish the objects in the inventory before releasing the batch.

      A midnight rollover may close an operational batch, but has no canonical
      meaning. Old, current, and future-dated events may coexist in a live file.
- [ ] Establish the **historical/live boundary without a gap**:
  1. activate the live shadow sink;
  2. after it is active, force-seal the current notepack segment; and
  3. record the ordered inventory and checksums of every sealed notepack input
     through that final segment, whose identity is high-water mark `H`.

      Historical conversion includes every sealed segment through `H`; live
      Parquet includes events observed after live activation. Events accepted
      between activation and sealing `H` intentionally occur in both paths.
      Logical dedup handles that overlap. The boundary is never a timestamp.
  - [x] The live shadow accepts an inclusive segment-number replay floor and
        persists the resulting replay policy in its SQLite inventory. On first
        production activation, the operator records `max(existing segment) + 1`
        while ingestion is stopped. Restarts replay that segment and every later
        sealed segment, but cannot silently widen the boundary to the historical
        archive or change the policy.
- [ ] Before P5, prove the chosen unsealed-buffer durability contract across the
      intended failure domain. Zero RPO requires synchronous/acknowledged
      durability outside the live process or host; asynchronous replication
      with minutes of RPO is not zero loss. During P2, notepack still protects
      canonical durability while this is tested.

Implementation checkpoint (2026-07-23): an open notepack segment now uses a
`.notepack.open` name. Sealing fsyncs it, atomically renames it to the
discoverable `.notepack` name, fsyncs the directory, and only then lets the
existing writer mark its event IDs archived. The optional Parquet worker
receives the post-compression sealed path; on restart it scans final
`.notepack[.gz]` names and ignores `.open` files, so a missed in-memory
notification is replayable. The same work-unit inventory makes replay
idempotent. A configurable maximum-age timer force-seals the notepack batch;
size and orderly shutdown use the existing seal path. Any Parquet failure is
recorded and logged but cannot fail or advance the authoritative notepack /
RocksDB path.

Recovery testing on 2026-07-27 also found that finite backfill processes could
exit while a detached gzip job was still running. `SegmentWriter` now retains
and joins every compression thread after the final seal and during drop, before
final statistics are read or downstream publication/indexing queues are
closed.

#### P2c — Convert sealed notepack history

- [x] Add a dedicated **sealed-notepack campaign binary**. It reads sealed notepack
      segments through high-water mark `H` and emits target-sized active-raw V1
      objects through the shared writer, validator, publication state machine,
      and inventory. ClickHouse is not an input.
  - [x] Local prototype: read one framed plain/gzip notepack segment directly,
        validate without JSON, deduplicate and sort, write through the shared
        V1 writer, and publish a completed local file atomically.
  - [x] Strictly fail by default on invalid records; with an explicit reject
        path, preserve invalid frames verbatim for quarantine and report counts.
  - [x] Add target-sized multi-file output, work-unit checkpoints, remote
        publication, inventory activation, and resumable recovery.
- [x] Make conversion resumable and idempotent. A work unit is one sealed
      segment or an explicitly bounded group of segments, identified by stable
      path/segment identity plus source checksum. Record migration-local input,
      output, validation, and publication state so a retry cannot silently skip
      an input or activate an output twice.
- [x] Preserve every valid event and timestamp. Do not create author-controlled
      calendar partitions and do not require global historical deduplication on
      this pass. One-ID-per-file still applies; duplicates across source
      segments or the historical/live overlap are valid raw-lake inputs and are
      resolved logically or by later compaction.
- [x] Recompute event IDs and verify signatures while converting. Invalid or
      unreadable source records enter an explicit quarantine/report; they must
      not be silently discarded.

The campaign identifies each source as `notepack-sha256-<digest>`, stages
deterministic `part-NNNNN.parquet` files, publishes content-addressed object
keys, stores counts, ranges, checksums,
writer version, and state in SQLite, and atomically changes the complete object
set from uploaded to active raw. Local publication uses no-clobber atomic
renames. S3-compatible publication uses conditional `PutObject`; retries and
race recovery require a `HeadObject` size and SHA-256 metadata match. Local
fault injection, real-segment replay, and a temporary Hetzner bucket round trip
are green. The durable production bucket is active and the sequential
historical campaign is running on a dedicated in-network VM with bounded spool
space and post-publication cleanup. Single-request uploads are sufficient for
the present campaign; multipart upload and worker concurrency are deferred
optimizations rather than migration prerequisites. Failed work remains in the
journal and its source remains available for an idempotent retry. After the
initial startup inventory completes, a catch-up pass must process the remaining
sources from a frozen, content-addressed manifest whose inclusive segment
boundary is exactly `H`. The worker must never keep extending that finite
historical campaign merely because newer source objects appear remotely.

Implementation checkpoint (2026-07-28): the campaign has a canonical source
manifest built from one `rclone lsjson` capture. It records the selected source
name and exact byte size for every segment through `H = 7702`, prefers gzip
when both representations exist, excludes an unsafe plain high-water file, and
is created with no-clobber semantics. Every later pass verifies the configured
boundary and consumes that same manifest. A deterministic completion audit
joins the frozen source universe to the complete campaign inventory and active
raw catalog view, then reports missing, failed, in-progress, unexpected, and
event-accounting defects.

#### P2d — Converge and verify

- [x] Build a deterministic **unified active-raw snapshot catalog** across the
      historical campaign, production live shadow, and repair/import
      inventories. Each writer exports a read-only portable fragment; a strict
      merge produces one content-addressed snapshot containing published work
      coverage and active raw object keys/checksums. Incomplete and quarantined
      state is excluded. Conflicts fail closed, and snapshots explicitly state
      that raw rows are not event-ID-deduplicated. See
      `docs/lake_active_file_catalog.md`.
- [ ] Complete the frozen historical-source manifest through `H`: every entry
      must have one published inventory work unit, valid input/output/reject/
      duplicate accounting, and active objects that pass strict checksum and
      V1 validation. Repair or explicitly quarantine every reported exception.
- [ ] Perform full seven-field source-to-Parquet comparison for every anomaly,
      repaired or salvaged input, historical/live boundary input, and a
      deterministic sample of ordinary historical work. A second exhaustive
      1.4-billion-event body comparison is not a migration prerequisite: the
      converter already verifies every source event ID/signature and the strict
      validator independently reopens every output row and verifies all seven
      logical fields.
- [ ] Periodically create a coordinated go-forward checkpoint by closing the
      current live batch and sealing the current notepack segment at the same
      ingest barrier. Compare complete ID-keyed seven-field values through that
      barrier. The intentional historical/live overlap at `H` must disappear
      under ID-keyed comparison.
- [ ] If a go-forward mismatch is found, replay the corresponding post-`H`
      notepack range through the shared repair/import path, publish the repair
      as active raw objects, and repeat parity verification. Do not hide a gap
      by editing an existing Parquet file.
- [ ] Run every §3 conformance check and fault-inject every publication-journal
      transition. Quarantined or incomplete objects must never enter an active
      query snapshot.

Implementation checkpoint (2026-07-27): fragments from the historical
campaign, production live shadow, and the segment 7703 repair inventory were
merged, independently verified, and conditionally published as immutable
snapshot
`sha256:9168328eeca44916aac9c9fdfd2a0fee941da515d728337c17af6085b15993a6`.
It selects 1,082 published work units, 1,597 active raw objects, 255,696,331
physical rows, and 102,813,011,702 object bytes. This is a reproducible
checkpoint while both ingestion paths continue, not the still-pending
historical/live parity proof.

**Gate:** the frozen manifest accounts for every sealed notepack input through
`H`; all active output passes the independent V1 validator; anomaly, repair,
boundary, and deterministic-sample content comparisons pass; N sustained
go-forward windows have complete ID-keyed parity; crash recovery converges at
every publication transition; and the proposed P5 unsealed-event RPO is
demonstrated.

**Rollback:** stop live Parquet sealing and historical conversion. Parquet is
shadow-only; authoritative notepack ingestion is unaffected.

### P3 — Optimize the lake and build analytics in parallel  *(Track B)*

- [x] Freeze the current analytics surface in the
      [endpoint migration ledger](analytics_endpoint_migration.md): 24
      ClickHouse-backed routes plus the separately scoped relay-operational and
      service routes. The ledger records request/response contracts, current
      query semantics, proposed DuckDB products, candidate Postgres serving
      shapes, parity classifications, and the initial implementation order.
- [x] Implement the first shadow analytics slice: active-snapshot consumption,
      exact cross-file ID deduplication, overview/daily/kind DuckDB products,
      reconciliations, versioned Postgres serving DDL, input/run provenance,
      streamed publication, and an atomic current-run pointer. See
      [Analytics Slice A](analytics_slice_a.md). This is not yet deployed,
      parity-approved, or connected to the API.
- [ ] Stand up **Postgres** rollup store.
- [ ] **DuckDB batch jobs** reproducing all 24 ClickHouse-backed analytics
      routes' metrics from Parquet.
      Distinct/rolling metrics use **Apache DataSketches HLL end-to-end**: DuckDB's
      built-in `approx_count_distinct` exposes no mergeable sketch state, and
      `postgresql-hll`'s binary format is **incompatible** with DataSketches — so it's
      the DataSketches DuckDB extension paired with `datasketches-postgresql`.
      (Simpler alternative: merge sketches entirely in the batch job and store plain
      integers per rollup window in Postgres; then only "today so far" needs a live
      merge.) "Today so far" comes from the selected live buffer and/or live
      sketches; it does not require SQLite specifically.
- [ ] Extend and consume the explicit **active-file snapshot** when compaction
      begins. The P2 foundation currently snapshots active raw objects only.
      The compaction extension must contain the active raw objects not covered
      by compaction plus active compacted replacements, and must never contain
      both a compacted output and the inputs it supersedes. Raw scans group by
      `id` unless the selected snapshot is certified deduplicated. A sum of
      physical Parquet rows is not an event count.
- [ ] Add the independent background **optimizer/compactor** only when file
      count or pruning performance warrants it. It:
  1. selects a bounded active input set from the inventory;
  2. reads and validates those V1 files;
  3. unions by `id` and applies the deterministic signature rule;
  4. writes new V1 files through the shared canonical writer, optionally
     clustering them into narrow `created_at` ranges and improved row groups;
  5. proves exact ID-keyed input/output equality; and
  6. atomically activates the outputs and supersedes the inputs.

      Clean event-date boundaries are query layout, not completeness claims.
      Late-arriving historical events land in later raw files and are folded
      into the relevant ranges by a future compaction pass. Immutable raw
      inputs remain available under the rollback/rebuild retention policy; the
      initial archive migration does not depend on compaction completing.
- [ ] **Verification harness:** diff each endpoint old (ClickHouse) vs new (Postgres)
      across parameter ranges. Compare exact metrics on the common representable,
      ID-deduplicated domain and maintain an expected-divergence ledger for known
      old-stack behavior (including narrowed timestamps and duplicate-sensitive
      materialized views). Distinct metrics remain within the agreed tolerance
      (§7).

**Gate:** all endpoints satisfy exact common-domain comparisons, documented
expected divergences, and approximate-metric tolerances for a sustained window
(≈1–2 weeks).
**Rollback:** new stack is shadow; readers still on ClickHouse.

### P4 — Cut over the readers  *(Track B)*

- [ ] Point `pensieve-serve` + Grafana at Postgres rollups.
- [ ] **Keep ClickHouse receiving the same ingest/index updates and ready as an
      instant reader fallback.** It may be read-only to ordinary consumers, but
      it cannot be frozen at cutover and still remain a current fallback.

**Gate:** stable through grace period (§7), no correctness issues.
**Rollback:** flip readers back to ClickHouse (still alive).

### P5 — Make Parquet the durability authority  *(contract change)*

- [ ] Switch the archive commit authority from notepack to Parquet.
      **Crash-safety contract changes here:** a remotely durable, independently
      validated active V1 object plus its recoverable publication-journal record
      becomes the condition for RocksDB `mark_archived`; the journal becomes
      `source_committed` only after that transition succeeds. A local live-buffer
      commit alone is insufficient.
- [ ] Continue writing notepack in shadow after this switch. Before P5, a locally
      fsynced/sealed notepack segment is the authoritative durability boundary;
      remote notepack synchronization is a separate asynchronous operation.
      After P5, notepack is a continuously updated rollback copy, not the commit
      authority.
- [ ] Prove restart recovery around every ordering of buffer commit, V1 close,
      upload, remote validation, inventory activation, RocksDB transition, and
      notepack shadow write. Losing or rebuilding RocksDB may cause duplicate
      collection, but the publication journal and active lake must still prevent
      event loss.

**Precondition:** the P2 publication state machine, remote-object validation,
restart recovery, active inventory, and selected unsealed-buffer RPO are live
and demonstrated; P3/P4 have completed their shadow and reader-fallback gates.

**Gate:** Parquet remains authoritative with complete live parity while
notepack and ClickHouse both continue as current fallbacks for the P5 soak
window.

**Rollback:** restore notepack as commit authority; no historical replay is
needed because shadow writing never stopped.

### P6 — Retire old storage  *(only after separate sustained proof)*

- [ ] Remove any remaining runtime dependency on ClickHouse before retirement.
      In particular, replace negentropy cold-start seeding from ClickHouse with
      the live buffer/recent-ID index, object inventory, or a bounded Parquet
      scan.
- [ ] After the P5 durability soak, stop writing notepack. Keep the complete
      notepack archive cold and immutable on object storage for the retention
      window (§7).
- [ ] After the P4 reader-fallback window and after all non-reader dependencies
      are removed, stop ClickHouse ingestion and decommission ClickHouse to
      reclaim ~2.6 TiB NVMe.
- [ ] Drop the preview server when its separate product retirement is confirmed.
- [ ] After archive stability is established, measure duplicate amplification
      and decide whether RocksDB should remain global, become a bounded/TTL hot
      index, or be replaced. This cleanup is not a migration prerequisite.

**Gate:** fully on the new stack for a sustained period before anything old is deleted.
**Rollback:** hardest here — mitigated by the long grace periods and cold-kept notepack.

---

## 6. Verification

The diff harness (seeded in P0) is what makes cutovers trustworthy:

- **File conformance (P0/P2):** inspect the exact physical/logical schema, nullability,
  nested-list shape, footer marker, compression/encodings, ordering, uniqueness, and
  row-group statistics. Reopen every sealed local and remotely published object.
- **Archive parity (P2):** compare complete ID sets and all seven logical values
  per live/conversion work unit against authoritative notepack, then compare
  the active historical-plus-live union across high-water mark `H`. Recompute
  every event ID and verify every signature during historical conversion;
  continue full or explicitly sampled cryptographic sweeps after cutover. Use
  ClickHouse only as a secondary reconciliation source on its representable
  domain, never as the historical conversion authority. Audit legacy RocksDB
  `Pending` state against notepack before P5 so stale dedup state cannot hide an
  archive hole.
- **Publication recovery (P2):** inject crashes before and after write, close, upload,
  remote validation, journal publication, RocksDB `mark_archived`, source commit, and
  live-buffer cleanup. Every restart must converge without losing an event or advertising
  a partial file. Exercise the RocksDB transitions in the P5-mode test harness without
  enabling them in the P2 production shadow path.
- **Lake snapshots (P3):** verify that the active object inventory resolves to the
  expected event set, excludes incomplete/quarantined objects and superseded
  inputs, and gives every compaction replacement exact ID-keyed input/output
  parity before activation.
- **Analytics (P3):** per-endpoint, per-parameter diff old vs new. Require exact
  results on the shared ID-deduplicated domain; record and explain differences
  caused by old ClickHouse timestamp narrowing or duplicate-sensitive
  materialized views. Distinct/rolling metrics remain within tolerance.
  Automate as a repeatable test and run it continuously through the shadow
  window.
- **Coverage (P1):** before/after each collector change; the metric must move.

## 7. Parameters to lock

| Parameter | Default | Notes |
|---|---|---|
| Distinct-metric tolerance | ~2% (HLL) | If exact required: store per-day pubkey sets (bigger, exact) |
| P3 shadow window | ≈2 weeks | Old vs new must match throughout |
| P4 ClickHouse fallback window | ≈2–4 weeks | ClickHouse continues receiving updates |
| P5 Parquet-authority soak | TBD before P5 | notepack continues shadow-writing throughout |
| notepack cold retention | 6–12 months | Starts after P6 stops notepack writes; no deletion before expiry |
| Tracks A & B | parallel | A now; B's P2 once P0 lands |
| Sketch stack | DataSketches | One format end-to-end (DuckDB ⇄ Postgres); never mix with `postgresql-hll` |
| V1 row-group target | ≈128 MiB uncompressed | Operational default; not part of conformance |
| V1 sealed-file target | ≈128 MiB–1 GiB | Smaller age/shutdown files are valid and compacted later |
| Maximum live-batch age | TBD before P2 | Bounds pending memory, recovery work, and unsealed exposure |
| P2 go-forward parity windows (`N`) | TBD before P2 | Consecutive coordinated live/notepack checkpoints required for the P2 gate |
| Unsealed live-buffer RPO after P5 | 0 by default; any exception explicit | Async replication with minutes of RPO is not zero-loss |

## 8. Risk register

| Risk | Mitigation |
|---|---|
| Event loss during migration | Ingest never stops; new paths additive/dual-write; collectors flagged |
| Archive corruption/loss | notepack stays source of truth until Parquet proven; validate schema/footer/rows; track object checksums externally; retain cold rollback copies |
| Historical/live boundary gap | Start live shadow first, then force-seal and checksum notepack high-water `H`; verify the intentional ID overlap |
| Wrong analytics after cutover | Parallel shadow + diff harness + expected-divergence ledger; keep ClickHouse ingest current during fallback |
| Ingest hot-path regression | Durable live-buffer writes isolated/batched; load-test before P2 gate |
| Crash-safety/durability mismatch | Defer contract change to P5; notepack owns archive commit and `mark_archived` until then |
| Writer emits a readable but non-conforming file | One shared writer plus an independent validator; golden cross-engine fixtures; validate every sealed object |
| `created_at` narrows through signed storage | Preserve the full unsigned domain in every buffer, writer, reader, and compactor; boundary fixtures above `i64::MAX` |
| Partial upload or ambiguous publication restart | Unique object keys + durable publication journal + fault injection at every transition |
| Cross-file duplicates inflate queries | Active-file snapshots plus `id`-keyed query semantics and compaction |
| RocksDB loss or stale `Pending` entries | Treat dedup as rebuildable efficiency state; audit against notepack before P5; never make lake correctness depend on global seen state |
| Small-file growth | Target-sized continuous seals; cataloged immutable compaction when thresholds are crossed |
| Collection outpaces old-stack disk | P0 tiering/relief; monitor NVMe through Track A |
| Bad NIP-66 monitor (misconfig/malice) | Aggregate multiple monitors; quorum / web-of-trust |
| `created_at` query performance | In-file `(created_at, id)` sort and native row-group statistics first; immutable compaction/reclustering later |
| Post-P5 loss of an unsealed live batch | Default to a zero-RPO buffer across the chosen failure domain; never persist `Archived` before the V1 object is durable |

## 9. Status tracker

Updated 2026-07-28.

- **P0 Foundation** — in progress (disk relief, archive-format acceptance,
  durable production object storage, real Hetzner compatibility and
  production-network round trips done; full verification automation and final
  backup/retention policy pending)
- **P1 Collection & coverage** — in progress (initial `e`/`q`
  reference-coverage ✅, NIP-66 catalog first slice ✅, catalog visibility
  gauges ✅, dynamic negentropy target augmentation ✅; multi-monitor trust,
  richer connection/reconciliation coverage, and windowed REQ pending)
- **P2 Parquet archive** — in progress. Shared V1 writer/validator, resumable
  target-sized campaign, publication journal/inventory, immutable S3
  publication, sealed-notepack live shadow, repair publication, and the
  deterministic unified active-raw snapshot catalog are implemented. The live
  replay floor is segment 7703, establishing the historical boundary at
  `H = 7702`. The sequential historical campaign is active on its dedicated VM;
  transient upload failures remain safely retryable. The finite catch-up source
  universe is now frozen at `H` and has deterministic completion accounting.
  Segment 7703 was sealed as an empty gzip after a now-fixed seal/compression
  race. Its exact 21,972 event IDs were preserved from RocksDB; initial
  exact-ID relay recovery found 4,833 valid unique event bodies, with 17,139
  IDs currently unrecovered and eligible for recurring wider-relay attempts.
  The repair was validated, published as a separate immutable Parquet work
  unit, restored to ClickHouse, and included with the historical and live
  inventories in the first operational unified snapshot. Campaign
  completion/catch-up, explicit handling of damaged historical inputs,
  targeted historical content comparison, coordinated go-forward parity
  windows, publication fault coverage, and the post-P5 unsealed-buffer
  failure-domain proof remain.
- **P3 Optimization + new analytics** — implementation started. The current
  ClickHouse-backed API surface and endpoint-by-endpoint parity contract are
  inventoried. Slice A now has an executable DuckDB builder and transactional
  Postgres serving schema; real-snapshot shadow deployment, old/new comparison,
  later slices, endpoint cutover, and the optimizer remain.
- **P4 Reader cutover** — not started
- **P5 Parquet durability authority** — not started
- **P6 Retire old storage** — not started

Also done (operational, not a plan phase): `pensieve-deploy/` → `ops/` restructure with
secrets in `/etc/pensieve/pensieve.env`; production box cut over to the new layout.

---

## Related
- `docs/analytics_endpoint_migration.md` — draft endpoint contract, DuckDB
  product mapping, Postgres serving candidates, and parity ledger for P3
- `docs/parquet_archive_format.md` — accepted canonical V1 specification
- `docs/lake_active_file_catalog.md` — deterministic distributed-inventory
  export, merge, verification, and immutable snapshot publication
- `docs/segment_7703_recovery.md` — evidence, validation, repair publication,
  and known-gap audit for the empty live segment incident
- `docs/parquet_writer_benchmark.md` — first real-segment prototype measurement
- Target-architecture design notes (the *what*)
- `docs/ingestion_pipeline.md` — current pipeline architecture
- TSM `event-archives` proposal — informative input to V1, not the accepted or
  normative Pensieve schema: `gitworkshop.dev/manime@nostr4.social/tsm`,
  clone: `https://relay.ngit.dev/npub1manlnflyzyjhgh970t8mmngrdytcp3jrmaa66u846ggg7t20cgqqvyn9tn/tsm.git`
- Open questions: negentropy strategy, relay-discovery strategy (tracked separately)
