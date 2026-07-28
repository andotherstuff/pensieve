# Nostr archive catalog signaling

*Status: design draft; no event kind allocated*
*Created: 2026-07-28*
*Last updated: 2026-07-28*

This document proposes a small, generic Nostr signaling layer for publishing the current catalog of an event archive.

The central design is:

> A provider publishes one addressable Archive Head event for each independently consumable dataset. The event points to one immutable, content-addressed catalog snapshot. The catalog, not a collection of relay events, defines the active Parquet file set.

## 1. Layering

Archive interoperability has two distinct layers:

| layer | responsibility |
|---|---|
| archive file | Defines the canonical rows and physical Parquet compatibility contract |
| archive catalog | Names the immutable files that make up one selected dataset snapshot |

The Archive Head belongs to the catalog layer. It does not change the canonical Parquet schema.

## 2. Goals

The signaling layer should:

- let a provider publish the current catalog for a logical archive dataset;
- let clients discover the catalog through ordinary Nostr relay queries;
- bind a signed provider identity to exact catalog bytes;
- support HTTPS, S3, Blossom, torrents, or other retrieval transports;
- support exact mirrors without making a location part of artifact identity;
- make catalog replacement atomic from a consumer's point of view;
- permit independently operated providers to publish compatible archives;
- support catalog sharding without one Nostr event per shard or Parquet file;
- preserve immutable historical catalog snapshots for reproducibility; and
- avoid dependencies on unrelated application protocols.

## 3. Non-goals

The Archive Head does not:

- prove that a provider captured every event matching a claimed scope;
- prove global Nostr coverage or prevent provider omission;
- define the canonical Parquet event schema;
- carry the full catalog or a large list of files in Nostr tags;
- make relay retention authoritative for file-set membership;
- define set reconciliation or transfer the archived events through relays;
- define access control for private object stores;
- make individual NIP-94 file announcements authoritative catalog entries.

## 4. Dataset identity

The Archive Head MUST use an addressable event kind in the `30000`-`39999` range. No kind number is allocated by this draft.

The logical dataset coordinate is:

```text
<archive-head-kind>:<provider-pubkey>:<d-tag>
```

The provider pubkey identifies the publisher. The `d` tag identifies one independently consumable dataset published by that key.

Examples of distinct datasets from one provider might be:

- `global-raw` for canonical event files;
- `observations` for receive time and relay provenance;
- `optimized` for a compacted active-file layout;
- `public-subset` for an intentionally limited distribution; or
- `projections` for separately defined derived tables.

A provider SHOULD publish one Archive Head for each such dataset. It SHOULD NOT publish one head per Parquet file. An event MUST have one dataset-defining `d` tag; multiple datasets require multiple addressable events.

Different providers MAY use the same `d` value. Their coordinates remain distinct because their author pubkeys differ. Common `d` conventions can make compatible datasets easier to recognize, but the `d` value is not a globally unique provider identity.

## 5. Full-snapshot semantics

Every Archive Head points to a **full active catalog snapshot** for its dataset. It does not point to a delta.

When a consumer accepts a newer head:

- the new catalog replaces the previous active catalog;
- objects present in the new catalog are active;
- objects absent from the new catalog are no longer active;
- consumers MAY retain superseded objects locally for rollback or audit; and
- consumers MUST NOT infer that the old and new catalogs should be unioned.

This rule makes compaction, repair, and removal explicit and atomic. A consumer never has to reconstruct current membership from the survival of thousands of relay events.

An optional `prev` tag links the new snapshot to the logical identity of the catalog it supersedes. `prev` is a history and fork-detection link only. It does not change the full-replacement rule.

## 6. Proposed Archive Head event

The following event is schematic. `<archive-head-kind>` is deliberately unallocated.

```jsonc
{
  "kind": "<archive-head-kind>",
  "pubkey": "<provider-pubkey>",
  "created_at": 1785236400,
  "tags": [
    // REQUIRED stable dataset identifier within this provider
    ["d", "global-raw"],

    // REQUIRED catalog format and logical snapshot identity
    ["format", "pensieve.active-raw-catalog.v1"],
    ["snapshot", "sha256:<logical-catalog-payload-hash>"],

    // REQUIRED primary retrieval location and exact downloaded-byte identity
    ["url", "https://archive.example/catalog/<catalog-file-sha256>.json"],
    ["x", "<sha256-of-exact-catalog-file-bytes>"],

    // REQUIRED media type; size is strongly recommended
    ["m", "application/json"],
    ["size", "<catalog-file-bytes>"],

    // OPTIONAL underlying object schema and discoverability labels
    ["schema", "nostr.event_archive.v1"],
    ["t", "nostr-event-archive"],

    // OPTIONAL alternate locations serving the same x-identified bytes
    ["fallback", "https://mirror.example/<catalog-file-sha256>.json"],

    // OPTIONAL logical predecessor; this is not a delta or union instruction
    ["prev", "sha256:<previous-logical-snapshot-id>"]
  ],
  "content": "Canonical raw Nostr event archive"
}
```

The event borrows `url`, `x`, `m`, `size`, and `fallback` vocabulary from [NIP-94 File Metadata](https://github.com/nostr-protocol/nips/blob/master/94.md). This draft does not require a separate kind `1063` event. A provider MAY also publish a NIP-94 event for the catalog or its individual Parquet files, but those events are supplementary distribution metadata.

### 6.1 Required tags

| tag | cardinality | meaning |
|---|---:|---|
| `d` | exactly one | Stable dataset identity within the provider pubkey |
| `format` | exactly one | Catalog parsing and validation contract |
| `snapshot` | exactly one | Format-defined logical identity of the catalog payload |
| `url` | exactly one | Primary URL for the exact catalog file |
| `x` | exactly one | Lowercase SHA-256 of the exact downloaded catalog bytes |
| `m` | exactly one | Lowercase MIME type of the catalog file |

`size` SHOULD be included so consumers can reject unexpectedly large downloads before parsing. A consumer MUST still count the received bytes and verify `x`.

### 6.2 Logical identity versus file identity

`snapshot` and `x` are intentionally distinct:

- `snapshot` identifies the catalog's canonical logical payload according to its declared `format`; and
- `x` identifies the exact bytes downloaded from `url` or `fallback`.

Pensieve's current catalog illustrates the distinction. Its `snapshot_id` is SHA-256 over a canonical payload, while the published JSON file also includes that ID and uses a required pretty-printed byte encoding. The complete JSON file therefore has a different SHA-256.

A consumer MUST verify both identities when the declared catalog format defines a logical content ID. Matching `x` alone proves exact transport bytes but does not replace format-specific validation.

### 6.3 Optional tags

`schema` is a discovery hint for the object format represented by the catalog. The catalog remains authoritative and MUST be checked before assuming that every object uses the declared schema.

Zero or more `fallback` tags MAY advertise other URLs that serve the exact bytes identified by `x`. A consumer MUST apply the same hash and size verification to every location.

`prev` SHOULD contain the prior catalog's logical `snapshot` identity. A consumer that has retained the previous accepted head can use it to detect unexpected history rewrites or forks.

`content` MAY contain a human-readable dataset description. It has no machine-readable catalog semantics.

## 7. Why there is not one event per file

One authoritative event per Parquet object would make relay state part of the archive's correctness. It would also require consumers to infer the active set from many independently delivered events and make atomic replacement impossible.

In particular:

- missing relay events would be indistinguishable from inactive files;
- compaction would require retiring many source events while activating many replacement events;
- a consumer could observe a mixture of old and new membership;
- large archives would generate excessive relay events and subscriptions; and
- reproducible historical snapshots would require preserving relay query results rather than a single content-addressed catalog.

Individual NIP-94 file announcements MAY help generic file-sharing clients locate or mirror an object. They MUST NOT define whether that object belongs to the active dataset. Only the catalog selected by the Archive Head does that.

## 8. Catalog hierarchy and scale

The first implementation may publish one catalog containing every active object. If that catalog becomes too large, the hierarchy should change without changing the Nostr event model:

```text
Archive Head
    └── immutable root catalog
          ├── immutable catalog shard A
          ├── immutable catalog shard B
          ├── immutable catalog shard C
          └── ...
                └── immutable Parquet objects
```

The Archive Head continues to reference only the root catalog. The root catalog's format defines how it references and hashes its shards. Unchanged shards can be reused in later snapshots, while changing the signed root pointer still atomically selects one complete active view.

Catalog shards SHOULD remain ordinary immutable objects rather than independent Archive Head events. A separate head is appropriate only when a subcatalog is an independently consumable dataset with its own identity and lifecycle.

## 9. Publisher procedure

A publisher updating an archive should:

1. Seal and validate every new Parquet object.
2. Durably publish each object under immutable, checksum-confirmed storage.
3. Construct the complete next catalog snapshot.
4. Validate catalog structure, object references, totals, and logical identity.
5. Publish the immutable catalog or root catalog.
6. Re-download or remotely inspect it and confirm byte size and SHA-256.
7. Construct an Archive Head with `x` matching those exact bytes.
8. Include `prev` when advancing an existing uninterrupted dataset history.
9. Sign and publish the addressable event to the provider's selected relays.

The head MUST be the final publication step. A publisher MUST NOT advertise a catalog whose bytes or referenced active objects have not completed their durability and validation gates.

## 10. Consumer procedure

A consumer should:

1. Query selected relays for Archive Head events by kind and, when known, provider pubkey and `d`.
2. Apply the standard addressable-event replacement rules.
3. Verify the Nostr event ID and signature.
4. Validate required tags, supported `format`, hash syntax, URL policy, and configured size limits.
5. Download from `url`, falling back only to explicitly accepted locations.
6. Require the exact received byte count to match `size`, when present.
7. Compute SHA-256 and require it to match `x` before parsing.
8. Parse and fully validate the declared catalog format, including `snapshot`.
9. Validate referenced object metadata and any hierarchical catalog shards.
10. Atomically select the new catalog as the active local snapshot.
11. Fetch Parquet objects on demand and verify their byte size, SHA-256, schema, and canonical row validity before use.

A consumer MUST treat all URLs and catalog contents as untrusted input. It should enforce outbound-network policy, redirect limits, response-size limits, parser limits, and local resource bounds.

## 11. Mirrors and transport independence

The immutable SHA-256 in `x` separates artifact identity from location. Providers may distribute identical bytes through:

- ordinary HTTPS;
- public or authenticated S3-compatible storage;
- Blossom servers;
- torrents or magnet links advertised separately; or
- offline replication.

[NIP-B7](https://github.com/nostr-protocol/nips/blob/master/B7.md) and Blossom's hash-addressed retrieval model can provide additional locations when a URL contains the catalog hash and the provider publishes a Blossom server list. Every transport remains subject to exact hash verification.

The catalog itself should carry the locations and hashes of its Parquet objects or of subordinate catalog shards. The Archive Head only needs enough location information to bootstrap retrieval of the root.

## 12. Coverage, trust, and reconciliation

The Archive Head proves that a provider signed a pointer to exact catalog bytes. It does not prove:

- that the catalog contains all events matching some filter;
- that the provider observed every relay or historical source;
- that an event omitted by the provider does not exist; or
- that two different providers should have identical file layouts.

Canonical event rows remain independently verifiable. Coverage and provenance claims remain publisher assertions that consumers may compare, audit, or ignore.

The SHA-256 of a physical Parquet file is not a logical event-set identity: two conforming writers can encode the same rows into different Parquet bytes. Cross-provider event-set reconciliation should therefore operate over event IDs rather than physical object hashes.

[NIP-77 Negentropy](https://github.com/nostr-protocol/nips/blob/master/77.md) already defines range-based event-ID set reconciliation. A future archive protocol may advertise a Negentropy-capable endpoint or another explicit logical-set reconciliation service. A Merkle root or fingerprint may detect equality, but a root alone does not specify how missing events are localized or transferred.

## 13. History, replay, and equivocation

Addressable events provide a current head, not an append-only transparency log. A provider can publish conflicting heads to different relay sets. The protocol cannot prevent such equivocation.

Consumers can make it more detectable by:

- retaining the last accepted event and catalog;
- requiring a known `prev` link for ordinary forward updates;
- warning when a new head does not descend from the retained snapshot;
- comparing heads observed from multiple relays; and
- optionally relying on independent witnesses or checkpoint archives.

An absent or unexpected `prev` is not necessarily malicious: it may represent a new dataset, key recovery, or an intentional history reset. Such a reset should require explicit operator or user acceptance rather than silent union.

Provider-key rotation is not defined in this draft. A later revision should define how an old key authorizes a successor without letting an unrelated key silently take over the dataset coordinate.

## 14. Pensieve mapping

Pensieve already implements the object and catalog side needed by this design:

- canonical Parquet objects follow `nostr.event_archive.version = "1"`;
- published objects are immutable and byte-identified by SHA-256;
- `pensieve.active-raw-catalog.v1` snapshots select the exact active raw object set;
- snapshots contain object keys, hashes, sizes, row counts, event-time ranges, work-unit coverage, and checked totals; and
- the complete catalog JSON is itself published immutably under a content-derived object key.

The initial Pensieve Archive Head should therefore be one event with a stable `d` such as `global-raw`:

```text
provider pubkey: Pensieve archive publisher key
d:               global-raw
format:          pensieve.active-raw-catalog.v1
snapshot:        existing catalog snapshot_id
x:               SHA-256 of the complete published JSON file
url:             immutable active-raw catalog object URL
schema:          nostr.event_archive.v1
```

The historical campaign, production live sink, and repair inventories remain catalog fragments and internal sources. They do not each require a Nostr event. The merged active-file snapshot is the one network-visible catalog.

## 15. Open decisions

Before publishing this protocol to production relays:

1. Allocate or adopt an addressable event kind.
2. Decide whether the first proposal standardizes only the Archive Head or also a provider-neutral catalog JSON format.
3. Decide whether `format`, `snapshot`, `schema`, and `prev` should retain these names or use existing single-letter/indexable tag conventions.
4. Define provider-key rotation.
5. Define whether trusted witnesses or immutable checkpoint events are needed for stronger equivocation detection.
6. Set practical catalog size and sharding thresholds.
7. Define relay recommendations and a discovery convention for finding previously unknown archive providers.

The first implementation should remain conservative: generate and validate the event from an already published catalog, test it on non-production relays, and avoid assigning provisional public semantics to an unallocated kind.
