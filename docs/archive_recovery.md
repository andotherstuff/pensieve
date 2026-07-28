# Archive recovery operations

This runbook covers two explicit repair paths. Neither path edits an original
notepack source or an existing Parquet object.

## Terminally truncated notepack source

Use this only when the normal campaign reports `TruncatedFrame`. Arbitrary
decoding failures and oversized frames are not terminal-truncation cases.

1. Retain the exact source object in a recovery directory and record its size
   and SHA-256.
2. Create an immutable salvage bundle:

   ```bash
   pensieve-notepack-salvage \
     segment-000001151.notepack \
     segment-000001151-salvage
   ```

   The destination is atomically created and contains:

   - `salvaged.notepack`: every structurally complete frame, byte for byte;
   - `truncated-tail.bin`: the incomplete frame prefix and available payload
     bytes after decompression; and
   - `report.json`: canonical content-addressed source, frame-accounting, and
     checksum evidence.

   A complete input is rejected. Complete frames that fail event validation
   remain in `salvaged.notepack`; the normal campaign will quarantine them.
3. Run `salvaged.notepack` through `pensieve-parquet-campaign` with a separate
   repair inventory and the canonical object-store prefix. Export its
   active-raw fragment with `pensieve-lake-catalog export`.
4. Bind the failed original source, salvage evidence, and published repair:

   ```bash
   pensieve-source-manifest build-exception-ledger \
     --manifest historical-source-manifest.json \
     --salvage-report segment-000001151-salvage/report.json \
     --repair-fragment segment-000001151-repair-fragment.json \
     --output historical-source-exceptions.json
   ```

   This is fail-closed: source name/size, original checksum, complete-frame
   count, reject count, salvaged checksum, repair work-unit identity, and
   active repair coverage must reconcile. The ledger is no-clobber evidence.
5. Supply both artifacts to the final completion audit:

   ```bash
   pensieve-source-manifest audit \
     --manifest historical-source-manifest.json \
     --inventory campaign.sqlite \
     --exceptions historical-source-exceptions.json \
     --repair-fragment segment-000001151-repair-fragment.json \
     --output historical-completion-audit.json
   ```

The audit counts the source as resolved, not normally published. It keeps the
failed original inventory row and verifies the repair fragment on every run.

## Exact-ID relay recovery

`recover-events` accepts a preserved target ID file and an operator-supplied
relay list. It validates every existing output row before resuming, accepts
only target IDs with valid IDs and signatures, fsyncs appended recovery data,
and atomically replaces the current missing-ID snapshot.

For recurring segment-7703 rounds, create a new retained round directory. Use
the previous round's missing-ID snapshot as this round's target file and an
expanded relay list:

```bash
recover-events \
  round-N/target-event-ids.hex \
  round-N/recovered-events.jsonl \
  round-N/missing-event-ids.hex \
  --relay-file round-N/relays.txt \
  --journal round-N/recovery-journal.jsonl \
  --batch-sizes 20,1 \
  --concurrency 4 \
  --request-timeout-secs 12
```

After each round:

1. preserve target, recovered, missing, relay-list, and journal checksums;
2. prove recovered and missing IDs are unique/disjoint and their union is the
   round target;
3. validate recovered JSONL through the normal ingest path with an isolated
   dedupe database;
4. publish only newly recovered events as a new immutable repair work unit;
5. index that delta into the still-running ClickHouse shadow; and
6. merge the repair fragment into a new unified active-raw snapshot.

An event body cannot be reconstructed from its ID. A round that recovers zero
events is a valid result; its remaining IDs stay explicit and can be retried
later against a materially different relay set.
