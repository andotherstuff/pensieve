# Canonical Nostr Parquet Archive Format

*Status: draft design*
*Last updated: 2026-07-23*

This document defines the canonical Parquet representation of Nostr events and
the operational model for using those files as a durable, queryable data lake.
It is the working archive-format design for Pensieve's lakehouse
migration.

The format is intentionally narrow:

- A row contains one complete, valid Nostr event.
- A sealed Parquet file is an immutable batch of such rows.
- A data lake is the union of any number of conforming files.
- File membership is determined by operational batching, not by event
  `created_at`.
- Query-oriented clustering and compaction happen later without changing the
  logical event format.

This document does **not** define archive discovery, manifests, completeness
claims, relay provenance, observation history, deletion policy, or distribution
metadata. Those concerns may be layered on top without changing V1 files.

---

## 1. Meaning of "canonical"

V1 is canonical at the **logical data** level:

- column names, types, and nullability are fixed;
- strings and nested tags have exact preservation rules;
- every row must pass the same NIP-01 validation;
- rows have a deterministic order within each file; and
- duplicate event IDs have deterministic merge semantics.

V1 is not a byte-for-byte canonical serialization of the complete Parquet
object. Two writers may choose different page boundaries, row-group boundaries,
dictionary thresholds, compression levels, or library metadata and therefore
produce different file bytes for the same logical rows.

A whole-file hash identifies one physical artifact. Logical equivalence is
determined from the validated event rows.

## 2. Canonical V1 schema

A conforming V1 file contains exactly these seven columns, in this order:

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

All seven columns and all values are required and non-null. A canonical V1
archive has no extension columns. Derived projections and provider-specific
observations belong in separate tables keyed by `id`.

### 2.1 Column semantics

| column | Parquet representation | requirements |
|---|---|---|
| `id` | `FIXED_LEN_BYTE_ARRAY(32)` | Raw 32 bytes obtained by hex-decoding the Nostr event ID |
| `pubkey` | `FIXED_LEN_BYTE_ARRAY(32)` | Raw 32 bytes obtained by hex-decoding the author public key |
| `created_at` | physical `INT64`, logical `INTEGER(64, false)` | Exact unsigned Nostr timestamp in seconds; never a Parquet `TIMESTAMP` |
| `kind` | physical `INT32`, logical `INTEGER(16, false)` | Exact unsigned event kind in `0..65535` |
| `tags` | required `LIST` of required `LIST` of required `STRING` | Outer order, inner order, duplicates, and empty strings preserved |
| `content` | `BYTE_ARRAY` annotated as `STRING` | Exact UTF-8 string value |
| `sig` | `FIXED_LEN_BYTE_ARRAY(64)` | Raw 64 bytes obtained by hex-decoding the BIP-340 signature |

`id`, `pubkey`, and `sig` are byte sequences, not integers. Implementations
must not reverse them or apply endianness conversion.

`created_at` uses the unsigned 64-bit logical annotation because the Nostr data
model and the Rust `nostr` implementation represent timestamps as `u64`. The
physical Parquet type remains `INT64`; the logical annotation supplies unsigned
interpretation and ordering without narrowing the signed event field. It must
not use Parquet's temporal `TIMESTAMP` annotation, whose available units are
milliseconds, microseconds, and nanoseconds rather than Nostr's seconds.
Readers, staging stores, and compaction jobs must likewise avoid narrowing the
value through a signed 64-bit or floating-point intermediate.

### 2.2 Tag representation

The logical tag type is:

```text
List<non-null List<non-null String>>
```

The outer `tags` list may be empty:

```json
[]
```

Every inner tag contains one or more strings. Tags of different lengths are
valid and coexist in the same column:

```json
[
  ["alt"],
  ["d", ""],
  ["p", "f7234bd4..."],
  ["e", "5c83da77...", "wss://relay.example.com", "root"]
]
```

Parquet can enforce non-null lists and strings, but it cannot express a
non-empty-list constraint. A producer and a validator must therefore reject an
empty inner tag:

```json
[
  []
]
```

Writers must preserve outer order, inner positional order, duplicate tags,
duplicate values, and empty strings. They must not sort, deduplicate, normalize,
or reinterpret tags.

### 2.3 String preservation

`tags` elements and `content` contain valid UTF-8 and use Parquet's `STRING`
logical annotation.

Writers must preserve the decoded Nostr string value exactly:

- no Unicode normalization;
- no newline normalization;
- no trimming;
- no conversion of empty strings to null; and
- no parsing and reserialization of JSON stored inside `content`.

The spelling of JSON escapes in a received wire object is not part of the string
value and is not preserved. NIP-01 canonical serialization is reconstructed from
the stored values when validating the event ID.

### 2.4 Unknown top-level event fields

Nostr event objects may be observed with additional top-level JSON fields. Those
fields are not committed to by the event ID or signature and can vary between
observations of the same event. They are therefore not part of the canonical
event archive.

If exact wire observations are needed for forensic or research purposes, they
must be stored in a separate observation dataset with one-to-many rows per event
ID. An observation record may preserve the exact received JSON bytes, source,
and observation time without changing the canonical event row.

## 3. Row validation and identity

Every row must be independently self-verifying. For each row, a validator must:

1. Confirm the physical and logical schema, fixed byte lengths, and non-null
   values.
2. Confirm that every tag contains at least one string.
3. Lowercase-hex-encode `pubkey`.
4. Reconstruct the NIP-01 signing array:

   ```json
   [0, "<pubkey-hex>", created_at, kind, tags, content]
   ```

5. Serialize it using NIP-01's canonical compact UTF-8 JSON rules.
6. Compute SHA-256 and require the result to equal `id`.
7. Verify `sig` over `id` with `pubkey` according to BIP-340.

Invalid rows must not appear in a canonical file. V1 has no `raw_json` escape
hatch for events whose ID or signature does not validate.

### 3.1 Duplicate IDs and signature variants

An event ID commits to `pubkey`, `created_at`, `kind`, `tags`, and `content`, but
not to `sig`. More than one BIP-340 signature may validly authenticate the same
event ID.

Within a file:

- an `id` must occur at most once;
- invalid signature variants are discarded; and
- if multiple valid signatures were observed before sealing, the writer retains
  the lexicographically smallest raw 64-byte signature.

Across files, the same event ID may occur more than once after backfills, peer
imports, repairs, or the union of independently produced archives. Readers treat
the data lake as a set keyed by `id`. Compaction applies the same validation and
lexicographically-smallest-signature rule.

If two rows share an ID but disagree on any ID-committed field, at least one row
is corrupt or invalid and must not be accepted without successful independent
validation.

## 4. Row ordering

Rows within every file must be sorted by:

1. `created_at` ascending using unsigned comparison; then
2. `id` ascending using raw unsigned lexicographic byte order.

This is an in-file ordering requirement only. Files in a data lake have no
required chronological relationship to one another.

A live file may legitimately contain events whose `created_at` values span many
years. A newly received event may carry an old, current, or future timestamp.
File membership must never imply that the file is complete for a `created_at`
range.

## 5. Parquet compatibility profile

V1 deliberately uses a conservative Parquet feature set:

- the standard three-level `LIST` representation shown in the schema;
- Data Page V1;
- Zstandard compression for column chunks; and
- `PLAIN`, `RLE`, and `RLE_DICTIONARY` encodings only.

Compression level, page size, row-group boundaries, and dictionary thresholds
are non-normative because they do not change logical contents.

Writers should:

- dictionary-encode `kind`;
- consider dictionary encoding `pubkey` when it reduces size;
- avoid dictionary encoding `id` and `sig`;
- target approximately 128 MiB of uncompressed data per row group; and
- target sealed files in the approximate 128 MiB to 1 GiB range.

Those sizes are operational defaults, not conformance requirements. Low-volume
or time-bounded live batches may produce smaller files and later be compacted.

## 6. File metadata

### 6.1 Required custom metadata

V1 defines one required Parquet footer key/value pair:

```text
nostr.event_archive.version = "1"
```

Rules:

- the key must occur exactly once;
- the value is the ASCII string `"1"`;
- duplicate `nostr.event_archive.*` keys make the file invalid;
- V1 readers ignore unknown `nostr.event_archive.*` keys;
- readers ignore unrelated application and library metadata; and
- a file with the exact schema but without the marker may be imported
  heuristically, but it is not a conforming V1 archive.

The namespace `nostr.event_archive.*` is reserved for this specification.
Provider-specific keys should use a separate namespace and carry no canonical
meaning.

### 6.2 Native Parquet metadata

The format relies on native Parquet metadata for:

- schema;
- total row count;
- row groups;
- encodings and compression;
- writer/library identification through `created_by`; and
- column statistics.

V1 writers must record correct `min`, `max`, and `null_count` statistics for
`created_at` in every row group. Because `created_at` is required, `null_count`
must be zero. These statistics make mixed-date live files queryable: after
in-file sorting, engines can skip old or future row groups even when a file's
overall event-time span is broad.

Writers should record native statistics for `kind`, `id`, and `pubkey`. They
should avoid statistics for `content`, nested tag values, and `sig`, where the
metadata cost offers little pruning value. Writers may add a Parquet Bloom
filter to `id`.

### 6.3 Metadata deliberately excluded from V1

V1 does not define custom keys for:

- row count or event-time extrema, which are native or derivable;
- kinds present, which are derivable;
- filters, time partitions, or completeness claims;
- ingestion time, observation time, or relay provenance;
- version/deletion/ephemeral-event policy claims;
- generator or generation time;
- archive series, parents, successors, or mirrors;
- a self-referential whole-file hash; or
- Merkle roots or logical event-set commitments.

These values either duplicate the rows, describe an external collection policy,
or require a manifest/distribution protocol. Keeping them out prevents stale or
contradictory footer claims from changing the meaning of a canonical event
file.

## 7. Immutable file lifecycle

Parquet files are immutable once sealed. File boundaries are operational and may
be triggered by:

- target uncompressed or compressed size;
- target row count;
- maximum live-buffer duration;
- a backfill work-unit boundary; or
- an orderly process shutdown.

They are not triggered by, and make no completeness claim about, `created_at`.

A producer follows this lifecycle:

1. Receive or read candidate events.
2. Validate IDs and signatures.
3. Deduplicate the pending batch by `id`.
4. Accumulate until an operational boundary is reached.
5. Sort by `(created_at, id)`.
6. Write the canonical schema and row groups to a temporary object.
7. Finish the Parquet footer.
8. Re-open and validate the sealed file, including schema, footer marker,
   ordering, row count, row-group statistics, sampled or complete event
   verification, and duplicate IDs.
9. Durably persist the object.
10. Publish it atomically by rename or completed object-store upload.

An open or partially written Parquet object is never advertised as an archive
file. Parquet footer metadata is written at close, so a crash can leave an open
file unreadable.

If live ingestion must be durable before a batch is sealed, an appendable
staging log, queue, or transactional hot store must retain those events. Events
are marked durably archived only after the sealed Parquet object is fsynced or
confirmed durable in object storage.

## 8. Backfill and live ingestion

Historical conversion and live collection use the same schema and validation
rules.

Backfill inputs may naturally produce files with narrow or chronological
`created_at` ranges. Live inputs will often produce mixed ranges because of:

- relay catch-up;
- late discovery;
- peer archive imports;
- old events newly encountered through references;
- client clock skew;
- intentionally backdated events; and
- future-dated events.

Mixed `created_at` values are normal. A producer buffers and sorts the current
batch before sealing it; it does not reopen old files or route each event into a
historical calendar partition.

Backfill and live writers may run concurrently. Their outputs are simply more
immutable files in the same data lake.

## 9. Data-lake semantics

The raw archive lake is a collection of immutable conforming files:

```text
canonical event set = union(all file rows) grouped by id
```

No filename or directory layout has canonical meaning. Operators may organize
objects by ingestion batch, upload date, writer, import source, or storage tier.
A manifest or catalog may later index those objects and their native statistics,
but it does not change the event format.

Within one Pensieve ingestion path, the global seen-ID index should prevent most
cross-file duplicates. The format nevertheless permits duplicates across files
because they are unavoidable when independently produced archives are combined.
Query and compaction layers must use `id` as the event-set key.

Generic analytical queries may scan the raw union directly. As the file count
grows, a manifest, table catalog, or local metadata index should avoid repeated
object listings and footer fetches. That catalog is an acceleration structure,
not the source of truth.

## 10. Query optimization

V1 provides three immediate query properties:

1. column projection avoids reading unused event fields;
2. in-file `(created_at, id)` sorting narrows row-group ranges; and
3. native statistics allow predicate and row-group pruning.

File-level event-time pruning may be weak for live batches containing a few old
or future events. That is acceptable. Row-group pruning still works after
in-file sorting, while later compaction improves file-level organization.

The canonical archive must not use author-controlled `created_at` as an
ingestion watermark or durability checkpoint. Queries for when an operator
received an event require a separate observation/provenance dataset.

## 11. Compaction and optimized layouts

Compaction is a derived, repeatable operation over immutable source files. It
may:

- combine small files;
- remove cross-file duplicate IDs;
- apply the deterministic signature-variant rule;
- sort globally or within larger groups by `(created_at, id)`;
- cluster events into narrower `created_at` ranges;
- choose improved row-group boundaries; and
- rebuild statistics and Bloom filters.

A compaction job must:

1. record its input objects in the external job state or manifest;
2. read and validate all input rows;
3. union by `id`;
4. write new temporary V1 files;
5. validate the complete logical output against the validated input event set;
6. durably publish the new files; and
7. update the external catalog or manifest atomically.

Compaction never modifies a sealed file in place. Input files are retained until
the replacement snapshot is published, verified, and past the configured
rollback/retention window.

An optimized lake may eventually use `created_at`-based directories or files for
research scans. Such layout is a query optimization, not a statement that the
network event set for a time window is complete. Late events may appear in
repair files and be incorporated by a later compaction.

The same canonical V1 schema is used for raw ingest files, backfill files,
repair files, and compacted files.

## 12. Conformance checklist

A file is a conforming V1 canonical event archive when:

- [ ] `nostr.event_archive.version` occurs exactly once with value `"1"`.
- [ ] The schema contains exactly the seven columns in the specified order.
- [ ] Physical types, logical annotations, and three-level `LIST` encoding
      match this document.
- [ ] All fields and list elements are non-null.
- [ ] Every tag has at least one string.
- [ ] Every event ID recomputes successfully.
- [ ] Every signature verifies.
- [ ] Every event ID occurs at most once in the file.
- [ ] Any observed valid signature variants were reduced deterministically.
- [ ] Rows are sorted by unsigned `created_at`, then raw `id`.
- [ ] Every row group has correct `created_at` statistics and zero nulls.
- [ ] The footer is complete and the sealed file can be reopened.

## 13. References

- [NIP-01: Basic protocol flow description](https://github.com/nostr-protocol/nips/blob/master/01.md)
- [Apache Parquet file format](https://parquet.apache.org/docs/file-format/)
- [Apache Parquet logical types](https://parquet.apache.org/docs/file-format/types/logicaltypes/)
- [Apache Parquet implementation status](https://parquet.apache.org/docs/file-format/implementationstatus/)
