//! Durable primitives for memory-bounded analytics execution.
//!
//! This module owns immutable run identity and checkpoint publication. Product
//! lanes may only reuse a completed artifact after its canonical checkpoint,
//! exact inputs, byte size, and SHA-256 have all been revalidated.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Schema version for bounded-run checkpoint evidence.
pub const BOUNDED_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Implementation identity for the bounded analytics runner.
pub const BOUNDED_RUNNER_VERSION: &str = "pensieve-analytics-bounded-v1";

/// Errors raised while publishing or validating bounded analytics evidence.
#[derive(Debug, thiserror::Error)]
pub enum BoundedExecutionError {
    /// A filesystem operation failed.
    #[error("bounded analytics I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Checkpoint JSON could not be encoded or decoded.
    #[error("bounded analytics checkpoint JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Immutable evidence or its identity failed validation.
    #[error("invalid bounded analytics evidence: {0}")]
    Invalid(String),
}

/// Result type for bounded analytics infrastructure.
pub type BoundedExecutionResult<T> = std::result::Result<T, BoundedExecutionError>;

/// Identity shared by every immutable run in one product build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunIdentity {
    /// Content ID of the frozen canonical catalog snapshot.
    pub snapshot_id: String,
    /// Fixed Unix timestamp used by time-windowed products.
    pub as_of: u64,
    /// Stable product lane name.
    pub product: String,
    /// Semantic version of the product computation.
    pub product_version: String,
    /// Stable description of the sorted key and record representation.
    pub key_space: String,
}

/// Exact identity and accounting for one input object or immutable run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputIdentity {
    /// Stable object key or content-derived run identity.
    pub identity: String,
    /// Exact input byte size.
    pub byte_size: u64,
    /// Exact logical input row count.
    pub row_count: u64,
    /// Lowercase hexadecimal SHA-256 of the input bytes.
    pub sha256: String,
}

/// Hard byte and row ceilings used to form bounded input batches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchLimits {
    /// Maximum catalog bytes assigned to one normal batch.
    pub max_bytes: u64,
    /// Maximum catalog rows assigned to one normal batch.
    pub max_rows: u64,
}

/// One deterministic, ordered input batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputBatch {
    /// Zero-based batch position in the frozen input order.
    pub index: usize,
    /// Exact ordered inputs assigned to the batch.
    pub inputs: Vec<InputIdentity>,
    /// Sum of catalog byte sizes in the batch.
    pub byte_size: u64,
    /// Sum of catalog row counts in the batch.
    pub row_count: u64,
    /// Whether one indivisible input exceeds at least one configured ceiling.
    pub oversized_single_input: bool,
}

/// Fixed-width record and key representation used by streaming merges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedRecordLayout {
    /// Total encoded bytes in one record.
    pub record_bytes: usize,
    /// Leading bytes that form the record's sorted key.
    pub key_bytes: usize,
}

/// Accounting and deterministic memory-bound evidence from one merge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MergeStats {
    /// Records consumed across all input runs.
    pub input_records: u64,
    /// Canonical records written after adjacent duplicate suppression.
    pub output_records: u64,
    /// Byte-identical records suppressed at equal keys.
    pub duplicate_records: u64,
    /// Maximum number of records simultaneously held by the merge heap.
    pub peak_buffered_records: usize,
    /// Maximum encoded record bytes simultaneously held by the merge heap.
    pub peak_buffered_bytes: usize,
}

/// One immutable run available to the levelled compaction planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunReference {
    /// Content-derived immutable run identity.
    pub identity: String,
    /// Completed artifact path.
    pub path: PathBuf,
    /// Current compaction level, where new batch runs are level zero.
    pub level: u32,
    /// Exact completed artifact byte size.
    pub byte_size: u64,
    /// Exact logical record count.
    pub row_count: u64,
}

/// Deterministic limits for levelled compaction planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionConfig {
    /// Maximum inputs consumed by one streaming merge.
    pub fan_in: usize,
    /// Maximum uncompacted runs retained at any one level.
    pub max_runs_per_level: usize,
}

/// One dependency-ordered merge in a levelled compaction plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionStep {
    /// Zero-based execution order.
    pub index: usize,
    /// Content-derived identity of the planned output.
    pub output_identity: String,
    /// Level assigned to the planned output.
    pub output_level: u32,
    /// Sorted identities consumed by the merge.
    pub input_identities: Vec<String>,
    /// Conservative upper bound for output bytes before duplicate suppression.
    pub max_output_bytes: u64,
    /// Conservative upper bound for output rows before duplicate suppression.
    pub max_output_rows: u64,
}

/// Disk requirements that must coexist before a bounded job starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskBudget {
    /// Maximum bytes written by the current output.
    pub output_bytes: u64,
    /// Maximum additional temporary merge or sort bytes.
    pub temporary_bytes: u64,
    /// Existing input/evidence bytes retained until publication succeeds.
    /// These are reported but already reflected in live filesystem usage.
    pub retained_bytes: u64,
    /// Free-space reserve that the job must leave untouched.
    pub reserve_bytes: u64,
}

/// Successful filesystem capacity preflight evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskPreflight {
    /// Bytes available to the current user at preflight time.
    pub available_bytes: u64,
    /// Existing protected bytes that must not be reclaimed to make the job fit.
    pub retained_bytes: u64,
    /// Sum of new output, temporary, and reserve requirements.
    pub required_bytes: u64,
    /// Available bytes remaining beyond the declared requirement.
    pub headroom_bytes: u64,
}

/// Facts required before a superseded run may be considered for cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupEligibility {
    /// A successor checkpoint and every artifact checksum have verified.
    pub successor_verified: bool,
    /// The successor generation has been atomically published.
    pub successor_published: bool,
    /// The explicit evidence-retention policy permits cleanup now.
    pub retention_permits_cleanup: bool,
    /// The candidate is still referenced by a current run or protected evidence.
    pub candidate_is_protected: bool,
}

/// Exact identity and accounting for one completed immutable artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactIdentity {
    /// Artifact path recorded by the runner.
    pub path: String,
    /// Exact completed byte size.
    pub byte_size: u64,
    /// Exact logical output row count.
    pub row_count: u64,
    /// Minimum encoded key, when the run is non-empty.
    pub min_key: Option<String>,
    /// Maximum encoded key, when the run is non-empty.
    pub max_key: Option<String>,
    /// Lowercase hexadecimal SHA-256 of the completed bytes.
    pub sha256: String,
}

/// Canonical immutable evidence for a completed bounded run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCheckpoint {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Frozen snapshot, time, product, and key-space identity.
    pub run: RunIdentity,
    /// Ordered exact identities consumed by this run.
    pub inputs: Vec<InputIdentity>,
    /// Completed output identity and accounting.
    pub artifact: ArtifactIdentity,
}

/// Load a completed checkpoint only when run and input identities match exactly.
pub fn load_reusable_checkpoint(
    checkpoint_path: impl AsRef<Path>,
    completed_path: impl AsRef<Path>,
    expected_run: &RunIdentity,
    expected_inputs: &[InputIdentity],
) -> BoundedExecutionResult<Option<RunCheckpoint>> {
    let checkpoint_path = checkpoint_path.as_ref();
    if !checkpoint_path.exists() {
        return Ok(None);
    }
    let checkpoint = read_run_checkpoint(checkpoint_path)?;
    if checkpoint.run != *expected_run || checkpoint.inputs != expected_inputs {
        return Err(BoundedExecutionError::Invalid(
            "completed checkpoint belongs to a different run or input sequence".to_owned(),
        ));
    }
    validate_run_checkpoint(checkpoint_path, completed_path, &checkpoint).map(Some)
}

/// Produce a deterministic dependency-ordered levelled compaction plan.
///
/// Planning never mutates or deletes input runs. Intermediate identities are
/// derived from the ordered input identities and destination level, so a retry
/// produces the same merge tree regardless of filesystem enumeration order.
pub fn plan_levelled_compaction(
    runs: &[RunReference],
    config: CompactionConfig,
) -> BoundedExecutionResult<Vec<CompactionStep>> {
    if config.fan_in < 2 || config.max_runs_per_level == 0 {
        return Err(BoundedExecutionError::Invalid(
            "compaction fan-in must be at least two and per-level limit must be non-zero"
                .to_owned(),
        ));
    }
    let mut levels: BTreeMap<u32, Vec<RunReference>> = BTreeMap::new();
    let mut identities = BTreeSet::new();
    for run in runs {
        if run.identity.trim().is_empty() {
            return Err(BoundedExecutionError::Invalid(
                "compaction run identity must not be empty".to_owned(),
            ));
        }
        if !identities.insert(run.identity.clone()) {
            return Err(BoundedExecutionError::Invalid(format!(
                "compaction run identity {:?} is duplicated",
                run.identity
            )));
        }
        levels.entry(run.level).or_default().push(run.clone());
    }
    for level in levels.values_mut() {
        level.sort_by(|left, right| left.identity.cmp(&right.identity));
    }

    let mut steps = Vec::new();
    while let Some(level) = levels
        .iter()
        .find_map(|(level, runs)| (runs.len() > config.max_runs_per_level).then_some(*level))
    {
        let available = levels
            .get_mut(&level)
            .expect("selected compaction level exists");
        let merge_count = available.len().min(config.fan_in);
        let inputs: Vec<_> = available.drain(..merge_count).collect();
        let output_level = level.checked_add(1).ok_or_else(|| {
            BoundedExecutionError::Invalid("compaction level overflowed u32".to_owned())
        })?;
        let max_output_bytes = checked_sum(
            inputs.iter().map(|run| run.byte_size),
            "compaction output bytes",
        )?;
        let max_output_rows = checked_sum(
            inputs.iter().map(|run| run.row_count),
            "compaction output rows",
        )?;
        let input_identities: Vec<_> = inputs.iter().map(|run| run.identity.clone()).collect();
        let output_identity = compaction_identity(output_level, &input_identities);
        steps.push(CompactionStep {
            index: steps.len(),
            output_identity: output_identity.clone(),
            output_level,
            input_identities,
            max_output_bytes,
            max_output_rows,
        });
        let next = levels.entry(output_level).or_default();
        next.push(RunReference {
            identity: output_identity,
            path: PathBuf::new(),
            level: output_level,
            byte_size: max_output_bytes,
            row_count: max_output_rows,
        });
        next.sort_by(|left, right| left.identity.cmp(&right.identity));
    }
    Ok(steps)
}

/// Check live filesystem capacity against a conservative coexistence budget.
pub fn preflight_disk(
    path: impl AsRef<Path>,
    budget: DiskBudget,
) -> BoundedExecutionResult<DiskPreflight> {
    let stat = rustix::fs::statvfs(path.as_ref()).map_err(std::io::Error::from)?;
    let available_bytes = stat.f_bavail.checked_mul(stat.f_frsize).ok_or_else(|| {
        BoundedExecutionError::Invalid("available disk byte accounting overflowed u64".to_owned())
    })?;
    evaluate_disk_budget(available_bytes, budget)
}

/// Return whether a candidate satisfies every conservative cleanup gate.
pub fn cleanup_is_eligible(eligibility: CleanupEligibility) -> bool {
    eligibility.successor_verified
        && eligibility.successor_published
        && eligibility.retention_permits_cleanup
        && !eligibility.candidate_is_protected
}

fn evaluate_disk_budget(
    available_bytes: u64,
    budget: DiskBudget,
) -> BoundedExecutionResult<DiskPreflight> {
    let required_bytes = checked_sum(
        [
            budget.output_bytes,
            budget.temporary_bytes,
            budget.reserve_bytes,
        ],
        "disk preflight requirement",
    )?;
    let headroom_bytes = available_bytes.checked_sub(required_bytes).ok_or_else(|| {
        BoundedExecutionError::Invalid(format!(
            "disk preflight requires {required_bytes} bytes but only {available_bytes} are available"
        ))
    })?;
    Ok(DiskPreflight {
        available_bytes,
        retained_bytes: budget.retained_bytes,
        required_bytes,
        headroom_bytes,
    })
}

fn checked_sum(values: impl IntoIterator<Item = u64>, label: &str) -> BoundedExecutionResult<u64> {
    values.into_iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")))
    })
}

fn compaction_identity(level: u32, inputs: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-bounded-compaction-v1\0");
    digest.update(level.to_be_bytes());
    for input in inputs {
        digest.update((input.len() as u64).to_be_bytes());
        digest.update(input.as_bytes());
    }
    format!("sha256:{}", hex::encode(digest.finalize()))
}

/// Partition frozen inputs by both byte and row ceilings.
///
/// Input order is preserved. Because catalog objects are indivisible, an
/// object larger than either ceiling is emitted alone and explicitly marked.
pub fn plan_input_batches(
    inputs: &[InputIdentity],
    limits: BatchLimits,
) -> BoundedExecutionResult<Vec<InputBatch>> {
    if limits.max_bytes == 0 || limits.max_rows == 0 {
        return Err(BoundedExecutionError::Invalid(
            "batch byte and row ceilings must both be non-zero".to_owned(),
        ));
    }
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    validate_inputs(inputs)?;
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0_u64;
    let mut current_rows = 0_u64;

    for input in inputs {
        let next_bytes = current_bytes.checked_add(input.byte_size).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch byte accounting overflowed u64".to_owned())
        })?;
        let next_rows = current_rows.checked_add(input.row_count).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch row accounting overflowed u64".to_owned())
        })?;
        if !current.is_empty() && (next_bytes > limits.max_bytes || next_rows > limits.max_rows) {
            push_batch(
                &mut batches,
                std::mem::take(&mut current),
                current_bytes,
                current_rows,
                limits,
            );
            current_bytes = 0;
            current_rows = 0;
        }
        current_bytes = current_bytes.checked_add(input.byte_size).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch byte accounting overflowed u64".to_owned())
        })?;
        current_rows = current_rows.checked_add(input.row_count).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch row accounting overflowed u64".to_owned())
        })?;
        current.push(input.clone());
    }
    if !current.is_empty() {
        push_batch(&mut batches, current, current_bytes, current_rows, limits);
    }
    Ok(batches)
}

/// Merge sorted fixed-width runs with one buffered record per input.
///
/// Records use their leading `key_bytes` as the comparison key. Exact
/// duplicates are suppressed, while two different records with the same key
/// fail closed. The caller chooses no more than `fan_in` inputs and receives a
/// durable `.partial` output suitable for immutable checkpoint publication.
pub fn merge_fixed_runs(
    input_paths: &[PathBuf],
    partial_output: impl AsRef<Path>,
    layout: FixedRecordLayout,
    fan_in: usize,
) -> BoundedExecutionResult<MergeStats> {
    validate_merge_config(input_paths, partial_output.as_ref(), layout, fan_in)?;
    let mut readers = input_paths
        .iter()
        .map(|path| File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::with_capacity(readers.len());
    let mut stats = MergeStats::default();
    for (source, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = read_fixed_record(reader, layout.record_bytes)? {
            heap.push(HeapRecord { record, source });
            stats.input_records = checked_increment(stats.input_records, "input record count")?;
        }
    }
    record_peak(&mut stats, heap.len(), layout.record_bytes)?;

    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial_output.as_ref())?;
    let mut writer = BufWriter::new(output);
    let mut last_output: Option<Vec<u8>> = None;
    while let Some(item) = heap.pop() {
        let source = item.source;
        let source_key = item.record[..layout.key_bytes].to_vec();
        match last_output.as_deref() {
            Some(previous) if keys_equal(previous, &item.record, layout.key_bytes) => {
                if previous != item.record {
                    return Err(BoundedExecutionError::Invalid(
                        "equal merge keys have conflicting record bytes".to_owned(),
                    ));
                }
                stats.duplicate_records =
                    checked_increment(stats.duplicate_records, "duplicate record count")?;
            }
            _ => {
                writer.write_all(&item.record)?;
                stats.output_records =
                    checked_increment(stats.output_records, "output record count")?;
                last_output = Some(item.record);
            }
        }

        if let Some(next) = read_fixed_record(&mut readers[source], layout.record_bytes)? {
            if source_key.as_slice() > &next[..layout.key_bytes] {
                return Err(BoundedExecutionError::Invalid(format!(
                    "input run {} is not sorted by its encoded key",
                    input_paths[source].display()
                )));
            }
            heap.push(HeapRecord {
                record: next,
                source,
            });
            stats.input_records = checked_increment(stats.input_records, "input record count")?;
            record_peak(
                &mut stats,
                heap.len() + usize::from(last_output.is_some()),
                layout.record_bytes,
            )?;
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(stats)
}

fn push_batch(
    batches: &mut Vec<InputBatch>,
    inputs: Vec<InputIdentity>,
    byte_size: u64,
    row_count: u64,
    limits: BatchLimits,
) {
    batches.push(InputBatch {
        index: batches.len(),
        oversized_single_input: inputs.len() == 1
            && (byte_size > limits.max_bytes || row_count > limits.max_rows),
        inputs,
        byte_size,
        row_count,
    });
}

#[derive(Eq, PartialEq)]
struct HeapRecord {
    record: Vec<u8>,
    source: usize,
}

impl Ord for HeapRecord {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .record
            .cmp(&self.record)
            .then_with(|| other.source.cmp(&self.source))
    }
}

impl PartialOrd for HeapRecord {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn validate_merge_config(
    input_paths: &[PathBuf],
    partial_output: &Path,
    layout: FixedRecordLayout,
    fan_in: usize,
) -> BoundedExecutionResult<()> {
    require_partial_path(partial_output)?;
    if input_paths.is_empty() {
        return Err(BoundedExecutionError::Invalid(
            "a merge must have at least one input run".to_owned(),
        ));
    }
    if fan_in < 2 {
        return Err(BoundedExecutionError::Invalid(
            "merge fan-in must be at least two".to_owned(),
        ));
    }
    if input_paths.len() > fan_in {
        return Err(BoundedExecutionError::Invalid(format!(
            "merge has {} inputs but configured fan-in is {fan_in}",
            input_paths.len()
        )));
    }
    if layout.record_bytes == 0 || layout.key_bytes == 0 || layout.key_bytes > layout.record_bytes {
        return Err(BoundedExecutionError::Invalid(
            "record and key widths must be non-zero and key width must not exceed record width"
                .to_owned(),
        ));
    }
    Ok(())
}

fn read_fixed_record(
    reader: &mut impl Read,
    record_bytes: usize,
) -> BoundedExecutionResult<Option<Vec<u8>>> {
    let mut record = vec![0_u8; record_bytes];
    let mut offset = 0;
    while offset < record_bytes {
        let read = reader.read(&mut record[offset..])?;
        if read == 0 {
            if offset == 0 {
                return Ok(None);
            }
            return Err(BoundedExecutionError::Invalid(
                "fixed-width run ends with a truncated record".to_owned(),
            ));
        }
        offset += read;
    }
    Ok(Some(record))
}

fn keys_equal(left: &[u8], right: &[u8], key_bytes: usize) -> bool {
    left[..key_bytes] == right[..key_bytes]
}

fn checked_increment(value: u64, label: &str) -> BoundedExecutionResult<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")))
}

fn record_peak(
    stats: &mut MergeStats,
    records: usize,
    record_bytes: usize,
) -> BoundedExecutionResult<()> {
    stats.peak_buffered_records = stats.peak_buffered_records.max(records);
    stats.peak_buffered_bytes = stats
        .peak_buffered_records
        .checked_mul(record_bytes)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("merge buffer accounting overflowed usize".to_owned())
        })?;
    Ok(())
}

/// Atomically publish a completed `.partial` artifact and immutable checkpoint.
///
/// A retry after interruption may reuse an already-published artifact only if
/// its bytes match the partial candidate. An existing checkpoint is accepted
/// only when its canonical bytes and every requested identity match exactly.
#[allow(clippy::too_many_arguments)]
pub fn publish_run_checkpoint(
    partial_path: impl AsRef<Path>,
    completed_path: impl AsRef<Path>,
    checkpoint_path: impl AsRef<Path>,
    run: RunIdentity,
    inputs: Vec<InputIdentity>,
    row_count: u64,
    min_key: Option<String>,
    max_key: Option<String>,
) -> BoundedExecutionResult<RunCheckpoint> {
    let partial_path = partial_path.as_ref();
    let completed_path = completed_path.as_ref();
    let checkpoint_path = checkpoint_path.as_ref();
    require_partial_path(partial_path)?;
    if parent_directory(partial_path) != parent_directory(completed_path) {
        return Err(BoundedExecutionError::Invalid(
            "partial and completed artifacts must share one directory for atomic publication"
                .to_owned(),
        ));
    }
    validate_run_identity(&run)?;
    validate_inputs(&inputs)?;
    validate_key_range(row_count, min_key.as_deref(), max_key.as_deref())?;

    let partial_identity = file_identity(partial_path)?;
    publish_file_noclobber(partial_path, completed_path, &partial_identity)?;
    let artifact = ArtifactIdentity {
        path: completed_path.to_string_lossy().into_owned(),
        byte_size: partial_identity.0,
        row_count,
        min_key,
        max_key,
        sha256: partial_identity.1,
    };
    let checkpoint = RunCheckpoint {
        schema_version: BOUNDED_CHECKPOINT_SCHEMA_VERSION,
        runner_version: BOUNDED_RUNNER_VERSION.to_owned(),
        run,
        inputs,
        artifact,
    };
    validate_checkpoint_fields(&checkpoint)?;
    write_canonical_json_noclobber(checkpoint_path, &checkpoint)?;
    validate_run_checkpoint(checkpoint_path, completed_path, &checkpoint)
}

/// Read a checkpoint, requiring canonical JSON and valid evidence fields.
pub fn read_run_checkpoint(path: impl AsRef<Path>) -> BoundedExecutionResult<RunCheckpoint> {
    let bytes = fs::read(path)?;
    let checkpoint: RunCheckpoint = serde_json::from_slice(&bytes)?;
    validate_checkpoint_fields(&checkpoint)?;
    if bytes != canonical_json(&checkpoint)? {
        return Err(BoundedExecutionError::Invalid(
            "checkpoint JSON is not canonically encoded".to_owned(),
        ));
    }
    Ok(checkpoint)
}

/// Validate a checkpoint, its exact expected identity, and completed artifact.
pub fn validate_run_checkpoint(
    checkpoint_path: impl AsRef<Path>,
    completed_path: impl AsRef<Path>,
    expected: &RunCheckpoint,
) -> BoundedExecutionResult<RunCheckpoint> {
    let checkpoint = read_run_checkpoint(checkpoint_path)?;
    if checkpoint != *expected {
        return Err(BoundedExecutionError::Invalid(
            "checkpoint identity does not exactly match the requested run".to_owned(),
        ));
    }
    let completed_path = completed_path.as_ref();
    if checkpoint.artifact.path != completed_path.to_string_lossy() {
        return Err(BoundedExecutionError::Invalid(
            "checkpoint artifact path does not match the requested completed path".to_owned(),
        ));
    }
    let (byte_size, sha256) = file_identity(completed_path)?;
    if byte_size != checkpoint.artifact.byte_size || sha256 != checkpoint.artifact.sha256 {
        return Err(BoundedExecutionError::Invalid(format!(
            "completed artifact {} does not match checkpoint bytes and SHA-256",
            completed_path.display()
        )));
    }
    Ok(checkpoint)
}

fn validate_checkpoint_fields(checkpoint: &RunCheckpoint) -> BoundedExecutionResult<()> {
    if checkpoint.schema_version != BOUNDED_CHECKPOINT_SCHEMA_VERSION {
        return Err(BoundedExecutionError::Invalid(format!(
            "unsupported checkpoint schema version {}",
            checkpoint.schema_version
        )));
    }
    if checkpoint.runner_version != BOUNDED_RUNNER_VERSION {
        return Err(BoundedExecutionError::Invalid(format!(
            "unsupported runner version {:?}",
            checkpoint.runner_version
        )));
    }
    validate_run_identity(&checkpoint.run)?;
    validate_inputs(&checkpoint.inputs)?;
    validate_sha256("artifact SHA-256", &checkpoint.artifact.sha256)?;
    validate_key_range(
        checkpoint.artifact.row_count,
        checkpoint.artifact.min_key.as_deref(),
        checkpoint.artifact.max_key.as_deref(),
    )
}

fn validate_run_identity(run: &RunIdentity) -> BoundedExecutionResult<()> {
    validate_sha256_id("snapshot ID", &run.snapshot_id)?;
    for (label, value) in [
        ("product", run.product.as_str()),
        ("product version", run.product_version.as_str()),
        ("key space", run.key_space.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(BoundedExecutionError::Invalid(format!(
                "{label} must not be empty"
            )));
        }
    }
    Ok(())
}

fn validate_inputs(inputs: &[InputIdentity]) -> BoundedExecutionResult<()> {
    if inputs.is_empty() {
        return Err(BoundedExecutionError::Invalid(
            "a completed run must record at least one input identity".to_owned(),
        ));
    }
    for input in inputs {
        if input.identity.trim().is_empty() {
            return Err(BoundedExecutionError::Invalid(
                "input identity must not be empty".to_owned(),
            ));
        }
        validate_sha256("input SHA-256", &input.sha256)?;
    }
    Ok(())
}

fn validate_key_range(
    row_count: u64,
    min_key: Option<&str>,
    max_key: Option<&str>,
) -> BoundedExecutionResult<()> {
    match (row_count, min_key, max_key) {
        (0, None, None) => Ok(()),
        (0, _, _) => Err(BoundedExecutionError::Invalid(
            "an empty artifact must not record a key range".to_owned(),
        )),
        (_, Some(minimum), Some(maximum)) if minimum <= maximum => Ok(()),
        (_, Some(_), Some(_)) => Err(BoundedExecutionError::Invalid(
            "artifact minimum key exceeds maximum key".to_owned(),
        )),
        _ => Err(BoundedExecutionError::Invalid(
            "a non-empty artifact must record both minimum and maximum keys".to_owned(),
        )),
    }
}

fn require_partial_path(path: &Path) -> BoundedExecutionResult<()> {
    if path.extension().and_then(|value| value.to_str()) != Some("partial") {
        return Err(BoundedExecutionError::Invalid(format!(
            "temporary artifact {} must use a .partial suffix",
            path.display()
        )));
    }
    Ok(())
}

fn validate_sha256_id(label: &str, value: &str) -> BoundedExecutionResult<()> {
    let digest = value.strip_prefix("sha256:").ok_or_else(|| {
        BoundedExecutionError::Invalid(format!("{label} is not a SHA-256 content ID"))
    })?;
    validate_sha256(label, digest)
}

fn validate_sha256(label: &str, value: &str) -> BoundedExecutionResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BoundedExecutionError::Invalid(format!(
            "{label} is not lowercase hexadecimal SHA-256"
        )));
    }
    Ok(())
}

fn publish_file_noclobber(
    partial_path: &Path,
    completed_path: &Path,
    expected: &(u64, String),
) -> BoundedExecutionResult<()> {
    let partial = OpenOptions::new()
        .read(true)
        .write(true)
        .open(partial_path)?;
    partial.sync_all()?;
    let parent = parent_directory(completed_path);
    fs::create_dir_all(parent)?;
    match fs::hard_link(partial_path, completed_path) {
        Ok(()) => File::open(parent)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if file_identity(completed_path)? != *expected {
                return Err(BoundedExecutionError::Invalid(format!(
                    "refusing to replace immutable artifact {}",
                    completed_path.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(partial_path)?;
    File::open(parent_directory(partial_path))?.sync_all()?;
    Ok(())
}

fn write_canonical_json_noclobber(
    path: &Path,
    value: &impl Serialize,
) -> BoundedExecutionResult<()> {
    let bytes = canonical_json(value)?;
    if path.exists() {
        if fs::read(path)? == bytes {
            return Ok(());
        }
        return Err(BoundedExecutionError::Invalid(format!(
            "refusing to replace immutable checkpoint {}",
            path.display()
        )));
    }
    let parent = parent_directory(path);
    fs::create_dir_all(parent)?;
    let partial = partial_checkpoint_path(path)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
    {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(&partial)? != bytes {
                return Err(BoundedExecutionError::Invalid(format!(
                    "checkpoint partial {} has conflicting content",
                    partial.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    match fs::hard_link(&partial, path) {
        Ok(()) => File::open(parent)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(path)? != bytes {
                return Err(BoundedExecutionError::Invalid(format!(
                    "refusing to replace immutable checkpoint {}",
                    path.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(&partial)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn partial_checkpoint_path(path: &Path) -> BoundedExecutionResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        BoundedExecutionError::Invalid("checkpoint path must have a file name".to_owned())
    })?;
    Ok(path.with_file_name(format!("{}.partial", file_name.to_string_lossy())))
}

fn canonical_json(value: &impl Serialize) -> BoundedExecutionResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn file_identity(path: &Path) -> BoundedExecutionResult<(u64, String)> {
    let mut file = File::open(path)?;
    let byte_size = file.metadata()?.len();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok((byte_size, hex::encode(digest.finalize())))
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_identity() -> RunIdentity {
        RunIdentity {
            snapshot_id: format!("sha256:{}", "1".repeat(64)),
            as_of: 1_786_024_407,
            product: "canonical-events".to_owned(),
            product_version: "v1".to_owned(),
            key_space: "event-id-v1".to_owned(),
        }
    }

    fn input_identity() -> InputIdentity {
        InputIdentity {
            identity: "raw/object.parquet".to_owned(),
            byte_size: 100,
            row_count: 2,
            sha256: "2".repeat(64),
        }
    }

    fn numbered_input(number: u64, bytes: u64, rows: u64) -> InputIdentity {
        InputIdentity {
            identity: format!("object-{number:03}"),
            byte_size: bytes,
            row_count: rows,
            sha256: format!("{number:064x}"),
        }
    }

    fn write_records(path: &Path, records: &[[u8; 4]]) {
        let bytes: Vec<_> = records.iter().flatten().copied().collect();
        fs::write(path, bytes).expect("write records");
    }

    fn publish(root: &Path, bytes: &[u8]) -> BoundedExecutionResult<RunCheckpoint> {
        let partial = root.join("run.bin.partial");
        fs::write(&partial, bytes)?;
        publish_run_checkpoint(
            &partial,
            root.join("run.bin"),
            root.join("run.json"),
            run_identity(),
            vec![input_identity()],
            2,
            Some("00".to_owned()),
            Some("ff".to_owned()),
        )
    }

    #[test]
    fn publishes_and_revalidates_canonical_immutable_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = publish(root.path(), b"sorted run").expect("publish");
        assert!(!root.path().join("run.bin.partial").exists());
        let validated = validate_run_checkpoint(
            root.path().join("run.json"),
            root.path().join("run.bin"),
            &checkpoint,
        )
        .expect("validate");
        assert_eq!(validated, checkpoint);
        let bytes = fs::read(root.path().join("run.json")).expect("checkpoint bytes");
        assert_eq!(bytes, canonical_json(&checkpoint).expect("canonical JSON"));
    }

    #[test]
    fn partial_artifact_never_satisfies_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("run.bin.partial"), b"incomplete").expect("partial");
        assert!(read_run_checkpoint(root.path().join("run.json")).is_err());
        assert!(!root.path().join("run.bin").exists());
    }

    #[test]
    fn resumes_after_artifact_publication_before_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let completed = root.path().join("run.bin");
        fs::write(&completed, b"sorted run").expect("completed");
        let checkpoint = publish(root.path(), b"sorted run").expect("resume");
        assert_eq!(checkpoint.artifact.byte_size, 10);
        assert!(root.path().join("run.json").is_file());
    }

    #[test]
    fn exact_retry_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = publish(root.path(), b"sorted run").expect("first publish");
        let second = publish(root.path(), b"sorted run").expect("retry");
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_tampered_artifact_and_identity_reuse() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = publish(root.path(), b"sorted run").expect("publish");
        fs::write(root.path().join("run.bin"), b"tampered").expect("tamper");
        assert!(
            validate_run_checkpoint(
                root.path().join("run.json"),
                root.path().join("run.bin"),
                &checkpoint,
            )
            .is_err()
        );

        fs::write(root.path().join("run.bin.partial"), b"different").expect("partial");
        assert!(
            publish_run_checkpoint(
                root.path().join("run.bin.partial"),
                root.path().join("run.bin"),
                root.path().join("run.json"),
                run_identity(),
                vec![input_identity()],
                2,
                Some("00".to_owned()),
                Some("ff".to_owned()),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_noncanonical_or_mismatched_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = publish(root.path(), b"sorted run").expect("publish");
        let path = root.path().join("run.json");
        let compact = serde_json::to_vec(&checkpoint).expect("compact JSON");
        fs::write(&path, compact).expect("replace checkpoint in test");
        assert!(read_run_checkpoint(&path).is_err());
    }

    #[test]
    fn resumes_from_exact_checkpoint_partial_and_rejects_scope_drift() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = publish(root.path(), b"sorted run").expect("publish");
        let loaded = load_reusable_checkpoint(
            root.path().join("run.json"),
            root.path().join("run.bin"),
            &run_identity(),
            &[input_identity()],
        )
        .expect("load")
        .expect("checkpoint");
        assert_eq!(loaded, first);

        let mut different = run_identity();
        different.as_of += 1;
        assert!(
            load_reusable_checkpoint(
                root.path().join("run.json"),
                root.path().join("run.bin"),
                &different,
                &[input_identity()],
            )
            .is_err()
        );

        let checkpoint_partial = root.path().join("run.json.partial");
        let checkpoint_bytes = fs::read(root.path().join("run.json")).expect("checkpoint");
        fs::remove_file(root.path().join("run.json")).expect("simulate interrupted link");
        fs::write(&checkpoint_partial, checkpoint_bytes).expect("checkpoint partial");
        fs::write(root.path().join("run.bin.partial"), b"sorted run").expect("artifact partial");
        let resumed = publish(root.path(), b"sorted run").expect("resume checkpoint partial");
        assert_eq!(resumed, first);
        assert!(!checkpoint_partial.exists());
    }

    #[test]
    fn batches_preserve_order_and_enforce_both_ceilings() {
        let inputs = vec![
            numbered_input(1, 6, 2),
            numbered_input(2, 4, 3),
            numbered_input(3, 7, 1),
            numbered_input(4, 2, 8),
        ];
        let batches = plan_input_batches(
            &inputs,
            BatchLimits {
                max_bytes: 10,
                max_rows: 5,
            },
        )
        .expect("plan");
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].inputs, inputs[..2]);
        assert_eq!((batches[0].byte_size, batches[0].row_count), (10, 5));
        assert_eq!(batches[1].inputs, inputs[2..3]);
        assert!(!batches[1].oversized_single_input);
        assert_eq!(batches[2].inputs, inputs[3..]);
        assert!(batches[2].oversized_single_input);
        assert!(
            plan_input_batches(
                &[],
                BatchLimits {
                    max_bytes: 10,
                    max_rows: 5,
                },
            )
            .expect("empty plan")
            .is_empty()
        );
    }

    #[test]
    fn fixed_merge_is_invariant_to_input_order_and_fan_in() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = root.path().join("first.run");
        let second = root.path().join("second.run");
        let third = root.path().join("third.run");
        write_records(&first, &[*b"a001", *b"c003", *b"e005"]);
        write_records(&second, &[*b"b002", *b"c003", *b"f006"]);
        write_records(&third, &[*b"a001", *b"d004", *b"g007"]);
        let layout = FixedRecordLayout {
            record_bytes: 4,
            key_bytes: 1,
        };
        let output_a = root.path().join("a.partial");
        let stats_a = merge_fixed_runs(
            &[first.clone(), second.clone(), third.clone()],
            &output_a,
            layout,
            3,
        )
        .expect("merge A");
        let output_b = root.path().join("b.partial");
        let stats_b =
            merge_fixed_runs(&[third, first, second], &output_b, layout, 8).expect("merge B");
        assert_eq!(
            fs::read(output_a).expect("output A"),
            b"a001b002c003d004e005f006g007"
        );
        assert_eq!(
            fs::read(root.path().join("a.partial")).expect("output A"),
            fs::read(output_b).expect("output B")
        );
        assert_eq!(stats_a, stats_b);
        assert_eq!(stats_a.input_records, 9);
        assert_eq!(stats_a.output_records, 7);
        assert_eq!(stats_a.duplicate_records, 2);
        assert_eq!(stats_a.peak_buffered_records, 4);
        assert_eq!(stats_a.peak_buffered_bytes, 16);
    }

    #[test]
    fn fixed_merge_fails_closed_on_conflicts_unsorted_and_truncated_runs() {
        let root = tempfile::tempdir().expect("tempdir");
        let valid = root.path().join("valid.run");
        let conflict = root.path().join("conflict.run");
        write_records(&valid, &[*b"a001", *b"b002"]);
        write_records(&conflict, &[*b"a999"]);
        let layout = FixedRecordLayout {
            record_bytes: 4,
            key_bytes: 1,
        };
        assert!(
            merge_fixed_runs(
                &[valid.clone(), conflict],
                root.path().join("conflict.partial"),
                layout,
                2,
            )
            .is_err()
        );

        let unsorted = root.path().join("unsorted.run");
        write_records(&unsorted, &[*b"b002", *b"a001"]);
        assert!(
            merge_fixed_runs(
                &[valid.clone(), unsorted],
                root.path().join("unsorted.partial"),
                layout,
                2,
            )
            .is_err()
        );

        let truncated = root.path().join("truncated.run");
        fs::write(&truncated, b"a00").expect("truncated");
        assert!(
            merge_fixed_runs(
                &[valid, truncated],
                root.path().join("truncated.partial"),
                layout,
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn levelled_compaction_plan_is_deterministic_and_respects_fan_in() {
        let runs: Vec<_> = (0..11)
            .rev()
            .map(|number| RunReference {
                identity: format!("run-{number:02}"),
                path: PathBuf::from(format!("run-{number:02}.bin")),
                level: 0,
                byte_size: 10,
                row_count: 2,
            })
            .collect();
        let config = CompactionConfig {
            fan_in: 3,
            max_runs_per_level: 2,
        };
        let first = plan_levelled_compaction(&runs, config).expect("plan");
        let mut reordered = runs.clone();
        reordered.rotate_left(4);
        let second = plan_levelled_compaction(&reordered, config).expect("replan");
        assert_eq!(first, second);
        assert!(
            first
                .iter()
                .all(|step| (2..=3).contains(&step.input_identities.len()))
        );
        assert!(first.iter().any(|step| step.output_level > 1));
        assert_eq!(
            first.iter().map(|step| step.index).collect::<Vec<_>>(),
            (0..first.len()).collect::<Vec<_>>()
        );
        let mut duplicated = runs.clone();
        duplicated.push(runs[0].clone());
        assert!(plan_levelled_compaction(&duplicated, config).is_err());
    }

    #[test]
    fn disk_preflight_and_cleanup_policy_fail_closed() {
        let budget = DiskBudget {
            output_bytes: 20,
            temporary_bytes: 30,
            retained_bytes: 40,
            reserve_bytes: 10,
        };
        assert_eq!(
            evaluate_disk_budget(125, budget).expect("preflight"),
            DiskPreflight {
                available_bytes: 125,
                retained_bytes: 40,
                required_bytes: 60,
                headroom_bytes: 65,
            }
        );
        assert!(evaluate_disk_budget(59, budget).is_err());
        let root = tempfile::tempdir().expect("tempdir");
        let live = preflight_disk(
            root.path(),
            DiskBudget {
                output_bytes: 0,
                temporary_bytes: 0,
                retained_bytes: 0,
                reserve_bytes: 0,
            },
        )
        .expect("live filesystem preflight");
        assert_eq!(live.available_bytes, live.headroom_bytes);
        let eligible = CleanupEligibility {
            successor_verified: true,
            successor_published: true,
            retention_permits_cleanup: true,
            candidate_is_protected: false,
        };
        assert!(cleanup_is_eligible(eligible));
        for blocked in [
            CleanupEligibility {
                successor_verified: false,
                ..eligible
            },
            CleanupEligibility {
                successor_published: false,
                ..eligible
            },
            CleanupEligibility {
                retention_permits_cleanup: false,
                ..eligible
            },
            CleanupEligibility {
                candidate_is_protected: true,
                ..eligible
            },
        ] {
            assert!(!cleanup_is_eligible(blocked));
        }
    }

    #[test]
    fn merge_memory_plateaus_across_hundredfold_input_growth() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = FixedRecordLayout {
            record_bytes: 8,
            key_bytes: 8,
        };
        let mut observed = Vec::new();
        for cardinality in [100_u64, 1_000, 10_000] {
            let paths: Vec<_> = (0..4)
                .map(|shard| {
                    let path = root.path().join(format!("{cardinality}-{shard}.run"));
                    let mut file = File::create(&path).expect("run");
                    for value in (shard..cardinality).step_by(4) {
                        file.write_all(&value.to_be_bytes()).expect("record");
                    }
                    path
                })
                .collect();
            let output = root.path().join(format!("{cardinality}.partial"));
            let stats = merge_fixed_runs(&paths, output, layout, 4).expect("merge");
            assert_eq!(stats.output_records, cardinality);
            observed.push(stats.peak_buffered_bytes);
        }
        assert_eq!(observed, vec![40, 40, 40]);
    }

    #[test]
    fn merge_tree_boundaries_produce_byte_identical_output() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = FixedRecordLayout {
            record_bytes: 4,
            key_bytes: 4,
        };
        let paths: Vec<_> = (0..4)
            .map(|shard| {
                let path = root.path().join(format!("shard-{shard}.run"));
                let records: Vec<_> = (shard..40_u32).step_by(4).map(u32::to_be_bytes).collect();
                write_records(&path, &records);
                path
            })
            .collect();
        let direct = root.path().join("direct.partial");
        merge_fixed_runs(&paths, &direct, layout, 4).expect("direct merge");
        let left = root.path().join("left.partial");
        let right = root.path().join("right.partial");
        merge_fixed_runs(&paths[..2], &left, layout, 2).expect("left merge");
        merge_fixed_runs(&paths[2..], &right, layout, 2).expect("right merge");
        let tree = root.path().join("tree.partial");
        merge_fixed_runs(&[left, right], &tree, layout, 2).expect("tree merge");
        assert_eq!(
            fs::read(direct).expect("direct"),
            fs::read(tree).expect("tree")
        );
    }
}
