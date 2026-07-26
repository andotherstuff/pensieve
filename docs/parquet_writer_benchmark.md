# Parquet writer benchmarks

*Status: engineering capacity checkpoint, not a fleet-wide production result*
*Measured: 2026-07-23 and 2026-07-26*

This records release-mode conversions of real Pensieve notepack segments
through the typed decoder, canonical V1 writer, atomic local publication, and
strict reopen validator.

## Initial prototype input and environment

- Input: `data/segments/segment-000000000.notepack.gz`
- Input size: 4,963,489 bytes compressed; 6,653,778 bytes after gzip decoding
- Frames: 7,010
- Host: Apple M4 Max, arm64
- OS: macOS 26.5.2
- Writer: `parquet` / Arrow Rust 59.1 with Zstandard compression

The input contained 7,009 canonically valid events and one invalid historical
record. The rejected record had a valid signature over its claimed ID, but its
ID did not match its ID-committed fields. Strict mode stopped without publishing
an output. The measured run used explicit `--rejects`, which preserved that
frame verbatim in a separate framed notepack segment.

## Result

| measurement | result |
|---|---:|
| input frames | 7,010 |
| canonical output rows | 7,009 |
| in-file duplicate IDs | 0 |
| quarantined frames | 1 |
| Parquet row groups | 1 |
| Parquet bytes | 4,312,638 |
| reject segment bytes, gzip | 693 |
| converter-reported elapsed time | 0.248 s |
| converter-reported rate | 28,235 events/s |
| compressed-input throughput | 19.1 MiB/s |
| decoded-input throughput | approximately 25.6 MiB/s |
| whole-process wall time from `/usr/bin/time` | 0.62 s |
| maximum resident set size | 56,492,032 bytes, approximately 53.9 MiB |

The resulting Parquet file passed the strict Rust validator with 7,009 rows,
one row group, and a `created_at` range of `1,702,892..1,767,542,150`.

## Operational campaign checkpoint

The same input was later run through `pensieve-parquet-campaign` with a
1,048,576-byte represented-data target. It produced 11 validated active-raw
Parquet objects containing all 7,009 canonical rows plus one quarantined reject
object. The SQLite journal recorded the work unit as `published`; an immediate
rerun returned the same object set with `resumed=true`.

This tests deterministic multi-file boundaries, checksummed immutable local
publication, and atomic inventory activation on real data. The S3-compatible
publisher compiles and uses conditional object creation plus size/SHA-256 HEAD
verification, but a production-bucket round trip is still a separate P0 gate.

## Representative sealed-segment benchmark

The 2026-07-26 run used a byte-for-byte copy of production
`segment-000003951.notepack`. Its local and production SHA-256 values both
matched:

```text
cd72da9e4d47bfbca1df3892a344d1f873abd8978b56f5b7b61ecbb3b99d1ec3
```

The input is 268,435,767 bytes, approximately the current 256 MiB notepack seal
size. The release campaign used its default 512 MiB represented-data file
target on the same Apple M4 Max host. Because raw work units are independently
publishable and are not coalesced on the first pass, this source produced one
Parquet object rather than being combined with a second segment.

| measurement | result |
|---|---:|
| input bytes | 268,435,767 (256.0 MiB) |
| input frames | 389,697 |
| canonical output rows | 389,695 |
| quarantined frames | 2 |
| Parquet objects | 1 |
| Parquet bytes | 176,544,824 (168.4 MiB) |
| Parquet/input byte ratio | 65.8% |
| Parquet row groups | 3 |
| row-group uncompressed bytes | 118,528,475; 116,710,090; 30,526,112 |
| whole-process wall time | 35.80 s |
| converter CPU time | 33.93 s user; 0.77 s system |
| input rate | 10,885 frames/s |
| input-byte throughput | 7.15 MiB/s |
| maximum resident set size | 1,057,406,976 bytes (1,008 MiB) |
| idempotent resume | 0.48 s; 10,321,920-byte maximum RSS |

The strict validator reopened the object and verified 389,695 rows, three row
groups, and a `created_at` range of `1,680,455,097..1,770,544,133`. PyArrow
reported row-group uncompressed sizes of 113.0, 111.3, and 29.1 MiB, keeping
each group near or below the format's approximately 128 MiB operational target.

The first run of this production-sized input exposed that one whole-file Arrow
record batch could become one oversized row group despite the configured byte
limit. The writer now creates and flushes row groups from deterministic
represented-size partitions. On the same input this reduced maximum RSS by 24%
from 1,391,542,272 bytes and changed one 276,960,271-byte uncompressed row group
into the three groups above. Final wall time remained comparable to the initial
36.34-second run.

## Object-storage checkpoint

The production host and local development environment were checked for a
Hetzner Object Storage target on 2026-07-26. Neither had a configured endpoint,
bucket, or Hetzner account context. The only cloud object-store profile on the
production host belongs to the existing upstream input path and was
intentionally not used as a Parquet test destination.

Consequently, conditional upload, `HeadObject` size/SHA-256 verification,
idempotent retry, and independent-reader download remain implemented and
fault-tested, but a real Hetzner S3-compatible round trip is still an open P0
deployment gate.

## Interpretation

The representative run establishes a useful single-worker starting point, not
a final production concurrency setting:

- the current prototype owns decoded rows for the entire input work unit,
  sorts them, then builds Arrow arrays;
- one current-size sealed segment consumed approximately 1 GiB of RSS, so
  production worker concurrency must be capped and measured on the actual
  conversion host;
- the 512 MiB represented-data file target is an upper batching target, not a
  promise that raw files will reach that size when each source work unit is
  smaller; later compaction can combine these approximately 168 MiB physical
  objects; and
- peak memory will vary with event and tag shape, so campaigns should observe
  RSS, throughput, rejection rate, and object-size distributions.

If representative segments make this ownership model too expensive, the next
implementation step is bounded run generation plus a merge, or another
spillable external-sort strategy. The canonical writer, validator, and file
format do not need to change.

The invalid historical frame confirms that migration cannot assume every old
notepack record is canonical merely because it was archived. Strict failure
remains the default. Historical campaigns need explicit quarantine, reporting,
and work-unit accounting so rejected inputs are visible and recoverable.

## Reproduction

```bash
cargo build --release -p pensieve-parquet \
  --bin notepack-to-parquet \
  --bin pensieve-parquet-validate

/usr/bin/time -l target/release/notepack-to-parquet \
  data/segments/segment-000000000.notepack.gz \
  /tmp/segment.parquet \
  --rejects /tmp/segment.rejects.notepack.gz

target/release/pensieve-parquet-validate /tmp/segment.parquet
```

The representative campaign used:

```bash
/usr/bin/time -l target/release/pensieve-parquet-campaign \
  --state-db /tmp/parquet-benchmark/campaign.sqlite \
  --staging-dir /tmp/parquet-benchmark/staging \
  --lake-dir /tmp/parquet-benchmark/lake \
  --target-uncompressed-bytes 536870912 \
  /tmp/parquet-benchmark/segment-000003951.notepack
```
