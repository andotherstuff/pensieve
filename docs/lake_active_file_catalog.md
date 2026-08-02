# Active-file lake catalog

*Status: implemented P2 foundation; active raw objects only*

This catalog gives a reader one explicit, immutable view of the Parquet files
that make up Pensieve's logical raw lake. It unifies the independently updated
SQLite inventories used by the historical campaign, the production live
shadow, and repair/import jobs without making those host-local databases a
distributed database.

The catalog is external operational metadata. It does not change the canonical
Parquet V1 schema or add footer metadata.

## Model

There are three layers:

1. Every writer keeps its existing SQLite publication journal and inventory.
   Only work units in `published` or `source_committed` state and Parquet
   objects in `active_raw` state are visible.
2. `pensieve-lake-catalog export` takes one consistent, read-only SQLite
   snapshot and writes a portable **fragment**.
3. `pensieve-lake-catalog merge` validates and deterministically unions one or
   more fragments into an immutable **active-file snapshot**.

A fragment includes published zero-output work units as well as work units with
objects. This makes a successfully processed empty input visible as coverage
instead of making it indistinguishable from an input that was never processed.
Failed, writing, uploading, quarantined, and otherwise incomplete work never
enters a fragment.

The V1 catalog covers active raw objects only. It sets
`deduplicated_by_event_id` to `false`, because independent raw files may contain
the same Nostr event ID. A reader must union/group by `id` and apply the
canonical signature-variant rule. `physical_rows` is useful for auditing I/O;
it is not a unique-event count.

## Identity and deterministic bytes

The format identifier is `pensieve.active-raw-catalog.v1`.

Each fragment records:

- a non-secret `store_id` shared by every participating inventory;
- a unique operator-assigned `inventory_id`;
- every published work unit's source name, checksum, byte size, conversion
  settings, writer identity, and input/output/reject counts;
- every active raw object's key, owning work unit and part, checksum, byte size,
  physical rows, writer identity, and unsigned event-time range; and
- checked aggregate totals.

Unsigned `created_at` minima and maxima are canonical decimal strings so the
full `u64` domain survives JSON consumers that cannot safely represent large
integers.

Records are strictly sorted. A fragment ID is SHA-256 over its canonical
payload. A snapshot ID is SHA-256 over its canonical payload, including the
sorted source fragment identities. There is deliberately no generation
timestamp: identical inventory states produce identical logical IDs and
identical JSON bytes. Readers require the tool's pretty-printed UTF-8 encoding
with one trailing newline; reformatting otherwise valid JSON is rejected. This
ensures one snapshot ID cannot be published with multiple byte representations.

Fragments do not need a cross-host transaction or matching wall-clock instant.
The merged snapshot is an exact selected object set, not a claim that collection
was complete at a timestamp. A coordinated P2d parity checkpoint still records
its own ingest barrier and verifies the selected union through that barrier.

Merge rejects:

- fragments for different object stores;
- one `inventory_id` with different fragment identities;
- one work-unit ID with different metadata or a different object set;
- one object key with different metadata;
- one work-unit part mapped to different object keys;
- missing work-unit references, invalid checksums or unsigned ranges;
- unsorted/duplicate records, incorrect totals, or a content-ID mismatch.

An identical fragment, work unit, or object may be supplied more than once and
is idempotently collapsed.

Because work IDs are derived from source content, byte-identical source files
in different inventories can share one work ID while retaining different
filenames. Merge accepts that case only when every conversion field and active
object key agrees, and keeps the lexicographically smallest filename as the V1
snapshot's deterministic representative. Per-filename historical completeness
remains evidence in the frozen-manifest completion audit and its publication
receipts rather than being inferred from this deduplicated active-object view.

`store_id` must identify the full object-store namespace, not only a bucket
name that might exist at multiple providers. Pensieve uses
`s3+https://hel1.your-objectstorage.com/pensieve-parquet`; it contains no
credentials.

## Operation

Build the release binary on each machine that owns an inventory. Exporting is
read-only and can run while its writer is active:

```bash
# Production live shadow and repair work
just lake-catalog export \
  --inventory /archive/segments/.parquet-shadow/inventory.sqlite \
  --inventory-id production-live \
  --store-id s3+https://hel1.your-objectstorage.com/pensieve-parquet \
  --output /var/lib/pensieve-parquet/catalog/production-live.json

# Historical conversion VM
just lake-catalog export \
  --inventory /var/lib/pensieve-parquet/state/campaign.sqlite \
  --inventory-id historical-campaign \
  --store-id s3+https://hel1.your-objectstorage.com/pensieve-parquet \
  --output /var/lib/pensieve-parquet/catalog/historical-campaign.json
```

Copy the two fragments to one administrative host, then merge and verify them:

```bash
just lake-catalog merge \
  --fragment historical-campaign.json \
  --fragment production-live.json \
  --output active-raw.json

just lake-catalog verify --snapshot active-raw.json
```

The merge replaces its local output atomically. Publishing uses the same
conditional, checksum-confirming immutable S3 machinery as Parquet objects:

```bash
just lake-catalog publish \
  --snapshot active-raw.json \
  --s3-bucket "$PENSIEVE_PARQUET_S3_BUCKET" \
  --s3-region "$AWS_REGION" \
  --s3-endpoint-url "$AWS_ENDPOINT_URL_S3" \
  --s3-force-path-style
```

The object key is
`nostr/v1/catalog/active-raw/<snapshot-id-hex>.json`. Publishing an identical
snapshot is safe; different bytes can never replace that key. Credentials use
the AWS SDK's normal environment/provider chain and never enter the catalog.

There is intentionally no mutable `latest` pointer in V1. A reader or scheduled
job selects an explicit snapshot ID, which makes a run reproducible and
rollback a configuration change rather than an object rewrite. Pointer
publication, retention policy, and scheduled refresh can be added with the
analytics control plane.

## Repairs and later compaction

A repair/import goes through the ordinary campaign state machine and becomes a
separate published work unit. If it uses an inventory already represented by a
fragment, the next export automatically includes it. This is how the segment
7703 recovery enters the unified snapshot; no existing object or fragment is
edited.

Compaction is not represented by V1. Before a compactor can activate
replacements, the inventory and catalog need a later format/state extension
that atomically selects compacted outputs while excluding every superseded
input. Until then, all snapshots are raw and readers deduplicate by event ID.

## Validation checkpoint

On 2026-07-27, a smoke test exported consistent backups of the active historical
and production-live inventories, merged them in both roles, and re-read the
result through the validator. The test snapshot contained 1,031 published work
units, 1,531 active objects, 246,933,708 physical rows, and 97,407,239,269
object bytes. It was a transient validation artifact, not a stable `latest`
snapshot; both inventories continued advancing.

The first operational snapshot was then assembled from fresh consistent
historical-campaign and production-live inventory backups plus the segment 7703
repair inventory. It contains:

- 1,082 published work units;
- 1,597 active raw Parquet objects;
- 255,696,331 physical rows; and
- 102,813,011,702 object bytes.

Its snapshot ID is
`sha256:9168328eeca44916aac9c9fdfd2a0fee941da515d728337c17af6085b15993a6`.
The exact 1,566,785-byte JSON was independently verified and conditionally
published at
`nostr/v1/catalog/active-raw/9168328eeca44916aac9c9fdfd2a0fee941da515d728337c17af6085b15993a6.json`;
its file SHA-256 is
`994854e3dbc65eaaea7099b5ee798888e264f73d9bd3f7b6aa0c44dc22421faf`.
This is an immutable checkpoint, not a mutable completeness claim: the
historical and live inventories continue to advance.
