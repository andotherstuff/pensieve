//! Fixed-memory Slice 9 benchmark for exact predefined publisher windows.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BoundedExecutionError, BoundedFixedActivity, FIXED_ACTIVITY_RECORD_BYTES, Result,
    publish_canonical_json,
};

/// Stable benchmark contract.
pub const PUBLISHER_BENCHMARK_VERSION: &str = "publisher-benchmark-v1";
/// Compact daily publisher fact bytes used for deterministic size projection.
pub const PUBLISHER_DAILY_FACT_BYTES: u64 = 52;
/// Compact daily publisher-kind fact bytes used for deterministic size projection.
pub const PUBLISHER_DAILY_KIND_FACT_BYTES: u64 = 54;
/// Compact predefined-window publisher row bytes used for size projection.
pub const PUBLISHER_MATERIALIZED_TOP_BYTES: u64 = 62;

const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-publisher-benchmark-v1";
const KIND_DOMAIN: usize = u16::MAX as usize + 1;

/// Exact windows and bounded top-K limits benchmarked by Slice 9.
#[derive(Clone, Debug)]
pub struct PublisherBenchmarkConfig {
    /// Exact rolling windows in days.
    pub windows_days: Vec<u32>,
    /// Representative kind filters whose exact rows are retained in evidence.
    pub sampled_kinds: Vec<u16>,
    /// Maximum publishers retained per window/filter.
    pub top_limit: usize,
}

/// One exact publisher result for a fixed as-of window.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublisherBenchmarkRow {
    /// Rolling window length.
    pub days: u32,
    /// Optional exact kind filter.
    pub kind: Option<u16>,
    /// Raw 32-byte pubkey as lowercase hex.
    pub pubkey: String,
    /// Exact events in the half-open lower / inclusive upper boundary.
    pub event_count: u64,
    /// Exact distinct kinds; one for kind-filtered rows.
    pub kinds_count: u64,
    /// Earliest included event timestamp.
    pub first_event: u32,
    /// Latest included event timestamp.
    pub last_event: u32,
}

/// Canonical deterministic results and measured scan cost for one benchmark.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublisherBenchmarkEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Completion state.
    pub status: String,
    /// Stable benchmark semantics.
    pub benchmark_version: String,
    /// Frozen activity snapshot.
    pub snapshot_id: String,
    /// Fixed analytics boundary.
    pub as_of_epoch: u64,
    /// SHA-256 of validated fixed-activity evidence.
    pub activity_evidence_sha256: String,
    /// SHA-256 of the exact source activity artifact.
    pub activity_artifact_sha256: String,
    /// Exact activity records scanned.
    pub source_records: u64,
    /// Exact predefined windows.
    pub windows_days: Vec<u32>,
    /// Representative retained kind filters.
    pub sampled_kinds: Vec<u16>,
    /// Exact top limit.
    pub top_limit: usize,
    /// Logical `(day,pubkey)` fact rows across the source.
    pub publisher_daily_rows: u64,
    /// Logical `(day,pubkey,kind)` fact rows across the source.
    pub publisher_daily_kind_rows: u64,
    /// Fixed-width compact bytes for all daily publisher facts.
    pub publisher_daily_compact_bytes: u64,
    /// Fixed-width compact bytes for all daily publisher-kind facts.
    pub publisher_daily_kind_compact_bytes: u64,
    /// Exact publisher cardinality for each window without a kind filter.
    pub all_kind_publishers_by_window: Vec<u64>,
    /// Exact sum across all 65,536 per-kind publisher cardinalities per window.
    pub per_kind_publishers_by_window: Vec<u64>,
    /// Exact rows in a fully materialized predefined-window top-K relation.
    pub materialized_top_rows: u64,
    /// Deterministic compact byte projection for that top-K relation.
    pub materialized_top_compact_bytes: u64,
    /// Exact all-kind and representative-kind top rows.
    pub representative_top_rows: Vec<PublisherBenchmarkRow>,
    /// SHA-256 of representative top rows.
    pub representative_rows_sha256: String,
    /// Maximum distinct kinds retained for one publisher/day.
    pub max_day_kinds_buffered: usize,
    /// Maximum distinct kinds retained for one publisher across windows.
    pub max_publisher_kinds_buffered: usize,
    /// Maximum total heap rows retained for representative results.
    pub max_representative_heap_rows: usize,
    /// Measured single-pass scan wall time.
    pub scan_elapsed_millis: u64,
    /// Measured source records processed per second.
    pub scan_records_per_second: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActivityRecord {
    pubkey: [u8; 32],
    created_at: u32,
    kind: u16,
}

struct ActivityReader {
    reader: BufReader<File>,
    previous: Option<[u8; FIXED_ACTIVITY_RECORD_BYTES]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RankRow {
    pubkey: [u8; 32],
    event_count: u64,
    kinds_count: u64,
    first_event: u32,
    last_event: u32,
}

impl Ord for RankRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event_count
            .cmp(&other.event_count)
            .then_with(|| other.pubkey.cmp(&self.pubkey))
    }
}

impl PartialOrd for RankRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Default)]
struct WindowAccumulator {
    count: u64,
    first: u32,
    last: u32,
    kinds: BTreeSet<u16>,
}

#[derive(Clone, Copy, Debug, Default)]
struct KindWindowAccumulator {
    count: u64,
    first: u32,
    last: u32,
}

/// Run one exact bounded publisher benchmark and preserve canonical evidence.
pub fn benchmark_publishers(
    evidence_path: impl AsRef<Path>,
    activity: &BoundedFixedActivity,
    config: PublisherBenchmarkConfig,
) -> Result<PublisherBenchmarkEvidence> {
    validate_config(activity, &config)?;
    let started = Instant::now();
    let mut reader = ActivityReader::open(Path::new(&activity.evidence.activity_artifact.path))?;
    let window_starts = config
        .windows_days
        .iter()
        .map(|days| {
            activity
                .evidence
                .as_of_epoch
                .checked_sub(u64::from(*days) * 86_400)
                .ok_or_else(|| {
                    BoundedExecutionError::Invalid("publisher window underflowed".to_owned()).into()
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let sampled = config
        .sampled_kinds
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut heaps = BTreeMap::new();
    for (window_index, _) in config.windows_days.iter().enumerate() {
        heaps.insert((window_index, None), BinaryHeap::new());
        for kind in &sampled {
            heaps.insert((window_index, Some(*kind)), BinaryHeap::new());
        }
    }
    let mut per_kind_cardinality = vec![vec![0_u64; KIND_DOMAIN]; config.windows_days.len()];
    let mut all_kind_cardinality = vec![0_u64; config.windows_days.len()];
    let mut publisher_daily_rows = 0_u64;
    let mut publisher_daily_kind_rows = 0_u64;
    let mut max_day_kinds_buffered = 0_usize;
    let mut max_publisher_kinds_buffered = 0_usize;
    let mut current_pubkey = None;
    let mut all_windows = vec![WindowAccumulator::default(); config.windows_days.len()];
    let mut kind_windows: BTreeMap<u16, Vec<KindWindowAccumulator>> = BTreeMap::new();
    let mut current_day = None;
    let mut day_kinds = BTreeSet::new();
    let mut source_records = 0_u64;
    while let Some(record) = reader.next()? {
        source_records = checked_add(source_records, 1, "publisher source records")?;
        if current_pubkey.is_some_and(|pubkey| pubkey != record.pubkey) {
            finish_day(
                &mut current_day,
                &mut day_kinds,
                &mut publisher_daily_rows,
                &mut publisher_daily_kind_rows,
                &mut max_day_kinds_buffered,
            )?;
            finish_publisher(
                current_pubkey.expect("publisher exists"),
                &all_windows,
                &kind_windows,
                &sampled,
                &mut all_kind_cardinality,
                &mut per_kind_cardinality,
                &mut heaps,
                config.top_limit,
            )?;
            all_windows.fill(WindowAccumulator::default());
            kind_windows.clear();
        }
        current_pubkey = Some(record.pubkey);
        if u64::from(record.created_at) > activity.evidence.as_of_epoch {
            continue;
        }
        let day = u64::from(record.created_at) / 86_400 * 86_400;
        if current_day.is_some_and(|current| current != day) {
            finish_day(
                &mut current_day,
                &mut day_kinds,
                &mut publisher_daily_rows,
                &mut publisher_daily_kind_rows,
                &mut max_day_kinds_buffered,
            )?;
        }
        current_day = Some(day);
        day_kinds.insert(record.kind);
        let per_kind = kind_windows
            .entry(record.kind)
            .or_insert_with(|| vec![KindWindowAccumulator::default(); config.windows_days.len()]);
        for (index, start) in window_starts.iter().enumerate() {
            if u64::from(record.created_at) < *start {
                continue;
            }
            observe_window(&mut all_windows[index], record)?;
            observe_kind_window(&mut per_kind[index], record)?;
        }
        max_publisher_kinds_buffered = max_publisher_kinds_buffered.max(kind_windows.len());
    }
    if let Some(pubkey) = current_pubkey {
        finish_day(
            &mut current_day,
            &mut day_kinds,
            &mut publisher_daily_rows,
            &mut publisher_daily_kind_rows,
            &mut max_day_kinds_buffered,
        )?;
        finish_publisher(
            pubkey,
            &all_windows,
            &kind_windows,
            &sampled,
            &mut all_kind_cardinality,
            &mut per_kind_cardinality,
            &mut heaps,
            config.top_limit,
        )?;
    }
    if source_records != activity.evidence.activity_artifact.row_count {
        return Err(BoundedExecutionError::Invalid(
            "publisher benchmark did not scan the exact activity artifact".to_owned(),
        )
        .into());
    }
    let per_kind_publishers_by_window = per_kind_cardinality
        .iter()
        .map(|counts| checked_sum(counts.iter().copied(), "publisher per-kind cardinality"))
        .collect::<Result<Vec<_>>>()?;
    let materialized_top_rows = all_kind_cardinality
        .iter()
        .zip(&per_kind_cardinality)
        .try_fold(0_u64, |total, (all, kinds)| {
            let total = checked_add(
                total,
                (*all).min(config.top_limit as u64),
                "publisher top rows",
            )?;
            kinds.iter().try_fold(total, |sum, count| {
                checked_add(
                    sum,
                    (*count).min(config.top_limit as u64),
                    "publisher kind top rows",
                )
            })
        })?;
    let representative_top_rows = finalize_heaps(&config, heaps);
    let representative_rows_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&representative_top_rows).map_err(BoundedExecutionError::from)?,
    ));
    let elapsed_millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let scan_records_per_second = source_records
        .saturating_mul(1_000)
        .checked_div(elapsed_millis)
        .unwrap_or(source_records);
    let max_representative_heap_rows = config
        .windows_days
        .len()
        .checked_mul(sampled.len() + 1)
        .and_then(|heaps| heaps.checked_mul(config.top_limit))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("publisher heap bound overflowed".to_owned())
        })?;
    let evidence = PublisherBenchmarkEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        benchmark_version: PUBLISHER_BENCHMARK_VERSION.to_owned(),
        snapshot_id: activity.evidence.snapshot_id.clone(),
        as_of_epoch: activity.evidence.as_of_epoch,
        activity_evidence_sha256: activity.evidence_sha256.clone(),
        activity_artifact_sha256: activity.evidence.activity_artifact.sha256.clone(),
        source_records,
        windows_days: config.windows_days,
        sampled_kinds: config.sampled_kinds,
        top_limit: config.top_limit,
        publisher_daily_rows,
        publisher_daily_kind_rows,
        publisher_daily_compact_bytes: publisher_daily_rows
            .checked_mul(PUBLISHER_DAILY_FACT_BYTES)
            .ok_or_else(|| {
                BoundedExecutionError::Invalid("publisher daily bytes overflowed".to_owned())
            })?,
        publisher_daily_kind_compact_bytes: publisher_daily_kind_rows
            .checked_mul(PUBLISHER_DAILY_KIND_FACT_BYTES)
            .ok_or_else(|| {
                BoundedExecutionError::Invalid("publisher kind bytes overflowed".to_owned())
            })?,
        all_kind_publishers_by_window: all_kind_cardinality,
        per_kind_publishers_by_window,
        materialized_top_rows,
        materialized_top_compact_bytes: materialized_top_rows
            .checked_mul(PUBLISHER_MATERIALIZED_TOP_BYTES)
            .ok_or_else(|| {
                BoundedExecutionError::Invalid("publisher materialized bytes overflowed".to_owned())
            })?,
        representative_top_rows,
        representative_rows_sha256,
        max_day_kinds_buffered,
        max_publisher_kinds_buffered,
        max_representative_heap_rows,
        scan_elapsed_millis: elapsed_millis,
        scan_records_per_second,
    };
    validate_evidence(&evidence)?;
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    Ok(evidence)
}

fn observe_window(accumulator: &mut WindowAccumulator, record: ActivityRecord) -> Result<()> {
    accumulator.count = checked_add(accumulator.count, 1, "publisher event count")?;
    if accumulator.count == 1 {
        accumulator.first = record.created_at;
    }
    accumulator.last = record.created_at;
    accumulator.kinds.insert(record.kind);
    Ok(())
}

fn observe_kind_window(
    accumulator: &mut KindWindowAccumulator,
    record: ActivityRecord,
) -> Result<()> {
    accumulator.count = checked_add(accumulator.count, 1, "publisher kind event count")?;
    if accumulator.count == 1 {
        accumulator.first = record.created_at;
    }
    accumulator.last = record.created_at;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_publisher(
    pubkey: [u8; 32],
    all_windows: &[WindowAccumulator],
    kind_windows: &BTreeMap<u16, Vec<KindWindowAccumulator>>,
    sampled: &BTreeSet<u16>,
    all_cardinality: &mut [u64],
    kind_cardinality: &mut [Vec<u64>],
    heaps: &mut BTreeMap<(usize, Option<u16>), BinaryHeap<Reverse<RankRow>>>,
    limit: usize,
) -> Result<()> {
    for (index, accumulator) in all_windows.iter().enumerate() {
        if accumulator.count == 0 {
            continue;
        }
        all_cardinality[index] =
            checked_add(all_cardinality[index], 1, "publisher all-kind cardinality")?;
        push_top(
            heaps.get_mut(&(index, None)).expect("all-kind heap"),
            RankRow {
                pubkey,
                event_count: accumulator.count,
                kinds_count: accumulator.kinds.len() as u64,
                first_event: accumulator.first,
                last_event: accumulator.last,
            },
            limit,
        );
    }
    for (kind, windows) in kind_windows {
        for (index, accumulator) in windows.iter().enumerate() {
            if accumulator.count == 0 {
                continue;
            }
            kind_cardinality[index][usize::from(*kind)] = checked_add(
                kind_cardinality[index][usize::from(*kind)],
                1,
                "publisher kind cardinality",
            )?;
            if sampled.contains(kind) {
                push_top(
                    heaps
                        .get_mut(&(index, Some(*kind)))
                        .expect("sampled-kind heap"),
                    RankRow {
                        pubkey,
                        event_count: accumulator.count,
                        kinds_count: 1,
                        first_event: accumulator.first,
                        last_event: accumulator.last,
                    },
                    limit,
                );
            }
        }
    }
    Ok(())
}

fn push_top(heap: &mut BinaryHeap<Reverse<RankRow>>, row: RankRow, limit: usize) {
    if heap.len() < limit {
        heap.push(Reverse(row));
    } else if heap.peek().is_some_and(|worst| row > worst.0) {
        heap.pop();
        heap.push(Reverse(row));
    }
}

fn finish_day(
    day: &mut Option<u64>,
    kinds: &mut BTreeSet<u16>,
    daily_rows: &mut u64,
    daily_kind_rows: &mut u64,
    max_kinds: &mut usize,
) -> Result<()> {
    if day.take().is_some() {
        *daily_rows = checked_add(*daily_rows, 1, "publisher daily rows")?;
        *daily_kind_rows = checked_add(
            *daily_kind_rows,
            kinds.len() as u64,
            "publisher daily-kind rows",
        )?;
        *max_kinds = (*max_kinds).max(kinds.len());
        kinds.clear();
    }
    Ok(())
}

fn finalize_heaps(
    config: &PublisherBenchmarkConfig,
    heaps: BTreeMap<(usize, Option<u16>), BinaryHeap<Reverse<RankRow>>>,
) -> Vec<PublisherBenchmarkRow> {
    let mut output = Vec::new();
    for ((window_index, kind), heap) in heaps {
        let mut rows = heap.into_iter().map(|row| row.0).collect::<Vec<_>>();
        rows.sort_by(|left, right| right.cmp(left));
        for row in rows {
            output.push(PublisherBenchmarkRow {
                days: config.windows_days[window_index],
                kind,
                pubkey: hex::encode(row.pubkey),
                event_count: row.event_count,
                kinds_count: row.kinds_count,
                first_event: row.first_event,
                last_event: row.last_event,
            });
        }
    }
    output.sort_by(|left, right| {
        left.days
            .cmp(&right.days)
            .then(left.kind.cmp(&right.kind))
            .then(right.event_count.cmp(&left.event_count))
            .then(left.pubkey.cmp(&right.pubkey))
    });
    output
}

fn validate_config(
    activity: &BoundedFixedActivity,
    config: &PublisherBenchmarkConfig,
) -> Result<()> {
    activity.validate_for_publication(
        &activity.evidence.snapshot_id,
        activity.evidence.as_of_epoch,
    )?;
    if config.windows_days.is_empty()
        || config
            .windows_days
            .windows(2)
            .any(|days| days[0] >= days[1])
        || config.windows_days.contains(&0)
        || config
            .sampled_kinds
            .windows(2)
            .any(|kinds| kinds[0] >= kinds[1])
        || config.top_limit == 0
        || config.top_limit > 1_000
    {
        return Err(BoundedExecutionError::Invalid(
            "publisher benchmark windows, kinds, or top limit are invalid".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn validate_evidence(evidence: &PublisherBenchmarkEvidence) -> Result<()> {
    let rows_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&evidence.representative_top_rows)
            .map_err(BoundedExecutionError::from)?,
    ));
    if evidence.schema_version != EVIDENCE_SCHEMA_VERSION
        || evidence.runner_version != RUNNER_VERSION
        || evidence.status != "completed"
        || evidence.benchmark_version != PUBLISHER_BENCHMARK_VERSION
        || evidence.windows_days.len() != evidence.all_kind_publishers_by_window.len()
        || evidence.windows_days.len() != evidence.per_kind_publishers_by_window.len()
        || evidence.representative_rows_sha256 != rows_sha256
        || evidence
            .publisher_daily_rows
            .checked_mul(PUBLISHER_DAILY_FACT_BYTES)
            != Some(evidence.publisher_daily_compact_bytes)
        || evidence
            .publisher_daily_kind_rows
            .checked_mul(PUBLISHER_DAILY_KIND_FACT_BYTES)
            != Some(evidence.publisher_daily_kind_compact_bytes)
        || evidence
            .materialized_top_rows
            .checked_mul(PUBLISHER_MATERIALIZED_TOP_BYTES)
            != Some(evidence.materialized_top_compact_bytes)
        || evidence.representative_top_rows.iter().any(|row| {
            row.event_count == 0
                || row.kinds_count == 0
                || row.first_event > row.last_event
                || row.pubkey.len() != 64
        })
    {
        return Err(BoundedExecutionError::Invalid(
            "publisher benchmark evidence does not reconcile".to_owned(),
        )
        .into());
    }
    Ok(())
}

impl ActivityReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<ActivityRecord>> {
        let mut bytes = [0_u8; FIXED_ACTIVITY_RECORD_BYTES];
        if !read_exact_or_eof(&mut self.reader, &mut bytes)? {
            return Ok(None);
        }
        if self.previous.is_some_and(|previous| previous >= bytes) {
            return Err(BoundedExecutionError::Invalid(
                "publisher source activity is not strictly ordered".to_owned(),
            )
            .into());
        }
        self.previous = Some(bytes);
        Ok(Some(ActivityRecord {
            pubkey: bytes[..32].try_into().expect("fixed pubkey"),
            created_at: u32::from_be_bytes(bytes[32..36].try_into().expect("fixed timestamp")),
            kind: u16::from_be_bytes(bytes[36..38].try_into().expect("fixed kind")),
        }))
    }
}

fn read_exact_or_eof(reader: &mut impl Read, bytes: &mut [u8]) -> Result<bool> {
    let mut offset = 0;
    while offset < bytes.len() {
        match reader.read(&mut bytes[offset..])? {
            0 if offset == 0 => return Ok(false),
            0 => {
                return Err(BoundedExecutionError::Invalid(
                    "publisher source activity is truncated".to_owned(),
                )
                .into());
            }
            read => offset += read,
        }
    }
    Ok(true)
}

fn checked_sum(values: impl IntoIterator<Item = u64>, label: &str) -> Result<u64> {
    values
        .into_iter()
        .try_fold(0_u64, |sum, value| checked_add(sum, value, label))
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed")).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_heap_keeps_exact_count_and_pubkey_order() {
        let mut heap = BinaryHeap::new();
        for (pubkey, count) in [(3, 5), (2, 5), (1, 4), (4, 6)] {
            push_top(
                &mut heap,
                RankRow {
                    pubkey: [pubkey; 32],
                    event_count: count,
                    kinds_count: 1,
                    first_event: 1,
                    last_event: 1,
                },
                2,
            );
        }
        let mut rows = heap.into_iter().map(|row| row.0).collect::<Vec<_>>();
        rows.sort_by(|left, right| right.cmp(left));
        assert_eq!(rows[0].pubkey, [4; 32]);
        assert_eq!(rows[1].pubkey, [2; 32]);
    }

    #[test]
    fn activity_reader_rejects_truncation() {
        let bytes = [0_u8; FIXED_ACTIVITY_RECORD_BYTES - 1];
        assert!(
            read_exact_or_eof(
                &mut bytes.as_slice(),
                &mut [0_u8; FIXED_ACTIVITY_RECORD_BYTES]
            )
            .is_err()
        );
    }
}
