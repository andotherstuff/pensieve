# Segment 7703 recovery record

*Status: initial repair and first recurring recovery delta published; recurring exact-ID recovery remains open*

This is the durable audit record for the first production live-shadow segment,
`segment-000007703.notepack.gz`.

## Incident

Segment 7703 was sealed as a valid but empty 20-byte gzip even though the
ingester had accepted 21,972 events into that segment. Its Parquet shadow and
ClickHouse indexing therefore also observed zero rows.

The cause was a seal/compression race: the next open segment could reuse the
path while asynchronous compression still referred to it. The segment state
machine now reserves the next open path before the old path is handed to the
sealer, and the frame-bounded notepack decoder fix is also deployed. Subsequent
live segments have sealed, converted, published, and indexed normally.

This incident does not change the historical/live boundary. The persisted live
replay floor is 7703, so the historical campaign boundary remains `H = 7702`.
Segment 7703 is repaired as a separate immutable work unit instead of editing
the empty source or any already-published object.

## Preserved evidence

The production RocksDB index preserved the exact 21,972 event IDs, but its
values intentionally contain no event bodies. Before any further work, the
relevant SST and the extracted ID set were copied aside:

- production SST:
  `/data/rocksdb-recovery/segment-7703/029060.sst`;
- exact target IDs:
  `/data/rocksdb-recovery/segment-7703/event-ids.hex`;
- target ID count: 21,972; and
- target ID file SHA-256:
  `12e159c9fd6ad764efa634398bbe3864643a79a617bf1e92d9f2ef511ba5b43e`.

The same ID file and checksum are retained on the recovery VM under
`/var/lib/pensieve-recovery/segment-7703/`. Production RocksDB is not edited and
no deduplication keys are deleted.

## Recovery and validation

A resumable recovery job asked multiple public Nostr relays for the exact event
IDs in progressively smaller batches. Every returned event had to:

1. parse as a Nostr event;
2. recompute to an ID in the preserved target set;
3. pass BIP-340 signature verification; and
4. be unique within the recovery output.

The relay passes used 100-ID batches, then 20-ID batches, then an exact retry of
every still-missing ID. The completed output was audited by sorting its IDs
against the target file. The audit proved that the recovered and missing sets
are unique and disjoint and that their union is exactly the 21,972 targets.

The recovered JSONL was processed by the normal `backfill-jsonl` validation
pipeline with an isolated temporary RocksDB index: 4,833 events entered and
4,833 valid events were written, with zero invalid events and zero duplicates.
The production RocksDB, live segment directory, and ClickHouse were not inputs
to that conversion.

That finite backfill exposed a second shutdown issue: gzip work ran in a
detached thread, so the process could exit with a complete sealed `.notepack`
source and an incomplete `.notepack.gz.open`. The incomplete temporary gzip was
removed and the complete plain source was used for repair publication. The
writer now tracks and joins every compression job before a finite process exits
or downstream consumers finish.

## Final result

- Recovered valid unique events: **4,833**
- Unrecovered target IDs: **17,139**
- Recovered JSONL SHA-256:
  `e46b20124e06795ab1615294c52696ae77a1e93ca10eecb19d39c4aeae3558d8`
- Missing-ID file SHA-256:
  `05cbb834edca0057fc83afad63699770c4917edace9db5a810306e8c1205e65c`
- Recovery journal SHA-256:
  `cc0bce01d824091b74a076308e7fef087468c4b405338047df0fe08fc4e5fd0b`
- Complete repair notepack bytes: **15,724,645**
- Complete repair notepack SHA-256:
  `e5a95b499fd8bbd8bf731548a6b8fbb3d699a2450dadeab95959ed7cb978d549`
- Repair work-unit ID:
  `notepack-sha256-e5a95b499fd8bbd8bf731548a6b8fbb3d699a2450dadeab95959ed7cb978d549`
- Active Parquet object:
  `nostr/v1/raw/notepack-sha256-e5a95b499fd8bbd8bf731548a6b8fbb3d699a2450dadeab95959ed7cb978d549/part-00000-c395769c662e725b9e8b99402683404ab72be322f5e2b417e782ec8d7fbbcc36.parquet`
- Parquet object: **4,833 rows**, **11,406,329 bytes**, SHA-256
  `c395769c662e725b9e8b99402683404ab72be322f5e2b417e782ec8d7fbbcc36`
- Parquet `created_at` range: `1762343413` through `1785165001`
- Unified snapshot ID:
  `sha256:9168328eeca44916aac9c9fdfd2a0fee941da515d728337c17af6085b15993a6`
- Unified snapshot object:
  `nostr/v1/catalog/active-raw/9168328eeca44916aac9c9fdfd2a0fee941da515d728337c17af6085b15993a6.json`

The independent Parquet validator reopened the repair object and confirmed
4,833 rows in one row group. Re-running publication was idempotent. An exact-ID
ClickHouse audit found zero of the target events before repair indexing and
4,833 rows / 4,833 unique IDs afterward.

The full recovery evidence archive is retained on production at
`/data/rocksdb-recovery/segment-7703/recovered-artifacts`:

- recovery archive:
  `segment-7703-recovery-artifacts.tar.gz`, SHA-256
  `d595f59954fc2d75e782aa6462f3768d3e5771f8e87fe5ccc26bcf6022185733`;
- catalog archive:
  `catalog/final-catalog.tar.gz`, SHA-256
  `fc5c186898ce79d3acf81ba826c4de6afc415996ca92d82d44240d4079592733`;
  and
- the catalog archive expands to the three source fragments and the exact
  unified snapshot JSON.

The 17,139 IDs not found in the initial relay rounds remain an explicit known
gap. They are currently unrecovered, not classified as permanently
unavailable. The retained missing-ID set can be used for recurring recovery
against materially broader relay sets with the first-class `recover-events`
tool described in [`archive_recovery.md`](archive_recovery.md). An event body
cannot be reconstructed from an ID alone, so every still-missing event remains
in the audit instead of being fabricated or silently counted as recovered.

## Recurring recovery round 2

On 2026-08-02, the first materially broader exact-ID retry queried 15 public
relays. The retained sets reconcile exactly: the 17,139 round targets equal
17,135 still-missing IDs plus 4 recovered IDs, with unique inputs, no overlap,
and no unaccounted ID.

All four returned events passed the normal JSONL ID/signature validation path
with zero invalid or duplicate events. They were written to a new isolated
notepack work unit and published as a separate immutable Parquet object; no
prior source, repair, inventory, or object was changed.

- Recovered events: **4**
- Still-missing target IDs: **17,135**
- Missing-ID SHA-256:
  `d4fbaad04a7cc993d857ba4f30ab10531085fe2e56fca6e1e5ed95b6c91cc348`
- Recovered JSONL SHA-256:
  `0a547605dc8ae92050a454b2393db55e0364997ef66452a6b3a6914d8fc6e13e`
- Recovery journal SHA-256:
  `1065c4b8b176c78a56e9c9f223e5186e98b3db6ff0c05f3e8494063ce60d554c`
- Relay-list SHA-256:
  `f72854d8cf2bad00ca73a9a474f875bd32e156d53f2aff0a0e3974a87ead4603`
- Repair work-unit ID:
  `notepack-sha256-335ce966c7638ea20ad305aaef6fd43ddc10dd3b8519510637474fb25e430cfd`
- Active Parquet object:
  `nostr/v1/raw/notepack-sha256-335ce966c7638ea20ad305aaef6fd43ddc10dd3b8519510637474fb25e430cfd/part-00000-2d67ac13ff199a3c59fb82de0a594753612051fdc226199e63746da828fa22ef.parquet`
- Parquet object: **4 rows**, **3,562 bytes**, SHA-256
  `2d67ac13ff199a3c59fb82de0a594753612051fdc226199e63746da828fa22ef`
- Superseding unified snapshot ID:
  `sha256:143aaf19143b79851f36ed7c507abd0e4174a0030a66f89e7be19e414fea6688`

The object was independently downloaded and compared to the round-2 notepack:
all four IDs and all seven canonical event fields matched exactly. The
superseding snapshot was also re-downloaded byte-for-byte and revalidated.
