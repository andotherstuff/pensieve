//! Bounded flexible-window distinct-author sketches.
//!
//! The exact Slice 5 activity artifact is already sorted by pubkey and commits
//! to the deduplicated event domain. This lane transforms bounded chunks into
//! sorted `(UTC hour, kind, pubkey)` identities, externally merges them, then
//! streams one fixed HLL leaf per `(hour, kind)`. Complete-hour windows compose
//! without retaining input-cardinality state in memory.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ArtifactIdentity, BoundedExecutionError, BoundedFixedActivity, DiskBudget, DistinctSketch,
    DistinctSketchBuilder, DistinctSketchUnion, FixedRecordLayout, InputIdentity, MergeStats,
    Result, RunCheckpoint, RunIdentity, load_reusable_checkpoint, merge_fixed_runs, preflight_disk,
    publish_canonical_json, publish_run_checkpoint,
};

/// Hour key, kind, and raw pubkey bytes in each exact intermediate identity.
pub const FLEXIBLE_DISTINCT_IDENTITY_BYTES: usize = 4 + 2 + 32;

/// Semantic version of the flexible distinct product.
pub const FLEXIBLE_DISTINCT_VERSION: &str = "flexible-distinct-v1";

const RUNNER_VERSION: &str = "pensieve-analytics-flexible-distinct-v1";
const ACTIVITY_RECORD_BYTES: usize = 32 + 4 + 2 + 32;
const LEAF_KEY_BYTES: usize = 6;
const LEAF_LENGTH_BYTES: usize = 4;
const MAX_SERIALIZED_SKETCH_BYTES: usize = 1_048_576;
const SECONDS_PER_HOUR: u64 = 3_600;
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource settings for the flexible distinct lane.
#[derive(Clone, Debug)]
pub struct FlexibleDistinctConfig {
    /// Dedicated immutable run root.
    pub work_root: PathBuf,
    /// Exact activity records transformed by one in-memory sort.
    pub source_records_per_batch: u64,
    /// Maximum fixed runs opened by one external merge.
    pub merge_fan_in: usize,
    /// Free bytes left untouched on the work filesystem.
    pub disk_reserve_bytes: u64,
}

/// Immutable completion evidence for one flexible distinct build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlexibleDistinctEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Completion status.
    pub status: String,
    /// Frozen catalog identity inherited from Slice 5.
    pub snapshot_id: String,
    /// Slice 5 analytics boundary.
    pub as_of_epoch: u64,
    /// Exclusive boundary after the last complete UTC hour represented.
    pub complete_through_epoch: u64,
    /// SHA-256 of the validated Slice 5 activity evidence.
    pub activity_evidence_sha256: String,
    /// Exact Slice 5 activity artifact consumed.
    pub activity_artifact: ArtifactIdentity,
    /// Activity records considered, including the incomplete trailing hour.
    pub source_activity_rows: u64,
    /// Immutable source batches.
    pub batch_count: u64,
    /// Immutable fixed-run merges.
    pub merge_count: u64,
    /// Exact unique `(hour, kind, pubkey)` state.
    pub identity_artifact: ArtifactIdentity,
    /// Versioned HLL leaves sorted by `(hour, kind)`.
    pub leaf_artifact: ArtifactIdentity,
    /// Maximum encoded identity bytes retained by one batch sort.
    pub max_batch_buffered_bytes: u64,
    /// Maximum encoded merge bytes retained simultaneously.
    pub max_merge_buffered_bytes: usize,
    /// Largest serialized Pensieve HLL envelope observed.
    pub max_leaf_bytes: usize,
    /// Conservative immutable-run disk estimate.
    pub estimated_run_bytes: u64,
    /// Operator-selected disk reserve.
    pub disk_reserve_bytes: u64,
    /// Immutable batch checkpoint paths.
    pub batch_checkpoints: Vec<String>,
    /// Immutable merge checkpoint paths.
    pub merge_checkpoints: Vec<String>,
    /// Immutable leaf checkpoint path.
    pub leaf_checkpoint: String,
}

/// Completed and validated flexible distinct product.
#[derive(Clone, Debug)]
pub struct BoundedFlexibleDistinct {
    /// Canonical completion evidence.
    pub evidence: FlexibleDistinctEvidence,
    /// SHA-256 of canonical evidence JSON.
    pub evidence_sha256: String,
}

/// One complete-hour-aligned distinct-author query over immutable leaves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlexibleDistinctWindow {
    /// Inclusive UTC epoch-second boundary, aligned to an hour.
    pub since_epoch: u64,
    /// Exclusive UTC epoch-second boundary, aligned to an hour.
    pub until_epoch: u64,
    /// Optional event-kind restriction; `None` unions all kinds.
    pub kind: Option<u16>,
}

impl BoundedFlexibleDistinct {
    /// Revalidate all immutable input, identity, leaf, and evidence invariants.
    pub fn validate_for_publication(&self, snapshot_id: &str, as_of_epoch: u64) -> Result<()> {
        let evidence = &self.evidence;
        if evidence.schema_version != 1
            || evidence.runner_version != RUNNER_VERSION
            || evidence.status != "completed"
            || evidence.snapshot_id != snapshot_id
            || evidence.as_of_epoch != as_of_epoch
            || evidence.complete_through_epoch != floor_hour(as_of_epoch)
        {
            return invalid("flexible-distinct evidence is not a completed matching product");
        }
        validate_fixed_artifact(&evidence.activity_artifact, ACTIVITY_RECORD_BYTES)?;
        validate_fixed_artifact(
            &evidence.identity_artifact,
            FLEXIBLE_DISTINCT_IDENTITY_BYTES,
        )?;
        validate_leaf_artifact(
            &evidence.identity_artifact,
            &evidence.leaf_artifact,
            evidence.complete_through_epoch,
            evidence.max_leaf_bytes,
        )?;
        Ok(())
    }
}

/// Load and fully revalidate completed flexible distinct evidence.
pub fn load_bounded_flexible_distinct(path: impl AsRef<Path>) -> Result<BoundedFlexibleDistinct> {
    let path = path.as_ref();
    let evidence: FlexibleDistinctEvidence =
        serde_json::from_slice(&fs::read(path)?).map_err(|e| {
            BoundedExecutionError::Invalid(format!("decode flexible-distinct evidence: {e}"))
        })?;
    let completed = BoundedFlexibleDistinct {
        evidence_sha256: pensieve_lake::sha256_file(path)?,
        evidence,
    };
    completed.validate_for_publication(
        &completed.evidence.snapshot_id,
        completed.evidence.as_of_epoch,
    )?;
    Ok(completed)
}

/// Visit every validated leaf in canonical `(hour, kind)` order.
///
/// Callers must obtain `product` from [`load_bounded_flexible_distinct`] or a
/// successful builder. The visitor receives only one bounded sketch blob at a
/// time, so publication does not retain leaf-cardinality state in memory.
pub fn visit_flexible_distinct_leaves(
    product: &BoundedFlexibleDistinct,
    mut visitor: impl FnMut(u32, u16, &[u8]) -> Result<()>,
) -> Result<u64> {
    let mut reader = LeafReader::open(Path::new(&product.evidence.leaf_artifact.path))?;
    let mut rows = 0_u64;
    while let Some(leaf) = reader.next()? {
        visitor(leaf.hour, leaf.kind, &leaf.sketch)?;
        rows = checked_add(rows, 1, "visited flexible leaf rows")?;
    }
    if rows != product.evidence.leaf_artifact.row_count {
        return invalid("visited flexible leaf row count mismatch");
    }
    Ok(rows)
}

#[derive(Clone)]
struct CompletedRun {
    identity: String,
    path: PathBuf,
    checkpoint_path: PathBuf,
    checkpoint: RunCheckpoint,
}

struct MergeOutcome {
    final_run: CompletedRun,
    merge_count: u64,
    max_buffered_bytes: usize,
    checkpoints: Vec<String>,
}

/// Build complete-hour, per-kind sketches from validated Slice 5 activity.
pub fn build_bounded_flexible_distinct(
    evidence_path: impl AsRef<Path>,
    activity: &BoundedFixedActivity,
    config: FlexibleDistinctConfig,
) -> Result<BoundedFlexibleDistinct> {
    activity.validate_for_publication(
        &activity.evidence.snapshot_id,
        activity.evidence.as_of_epoch,
    )?;
    validate_config(&config)?;
    fs::create_dir_all(&config.work_root)?;
    let source = &activity.evidence.activity_artifact;
    let batch_count = source.row_count.div_ceil(config.source_records_per_batch);
    let estimated_run_bytes = estimate_run_bytes(
        source.row_count,
        usize::try_from(batch_count).map_err(|_| {
            BoundedExecutionError::Invalid("flexible batch count exceeds usize".to_owned())
        })?,
        config.merge_fan_in,
    )?;
    let retained = completed_run_bytes(&config.work_root)?;
    preflight_disk(
        &config.work_root,
        DiskBudget {
            output_bytes: estimated_run_bytes.saturating_sub(retained),
            temporary_bytes: 0,
            retained_bytes: retained,
            reserve_bytes: config.disk_reserve_bytes,
        },
    )?;

    let complete_through_epoch = floor_hour(activity.evidence.as_of_epoch);
    let batch_root = config.work_root.join("batches");
    fs::create_dir_all(&batch_root)?;
    let mut runs = Vec::new();
    let mut batch_checkpoints = Vec::new();
    let mut offset = 0_u64;
    let mut max_batch_buffered_bytes = 0_u64;
    let mut index = 0_u64;
    while offset < source.row_count {
        let rows = config
            .source_records_per_batch
            .min(source.row_count - offset);
        let run = build_batch(
            activity,
            complete_through_epoch,
            offset,
            rows,
            index,
            &batch_root,
        )?;
        max_batch_buffered_bytes = max_batch_buffered_bytes.max(
            rows.checked_mul(FLEXIBLE_DISTINCT_IDENTITY_BYTES as u64)
                .ok_or_else(|| {
                    BoundedExecutionError::Invalid(
                        "flexible batch memory evidence overflow".to_owned(),
                    )
                })?,
        );
        batch_checkpoints.push(run.checkpoint_path.to_string_lossy().into_owned());
        runs.push(run);
        offset = checked_add(offset, rows, "flexible source offset")?;
        index = checked_add(index, 1, "flexible batch index")?;
    }
    if runs.is_empty() {
        runs.push(build_empty(activity, &config.work_root)?);
    }

    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_all(runs, activity, config.merge_fan_in, &merge_root)?;
    let leaf_root = config.work_root.join("leaves");
    fs::create_dir_all(&leaf_root)?;
    let (leaf_run, max_leaf_bytes) = build_leaves(activity, &merged.final_run, &leaf_root)?;

    let evidence = FlexibleDistinctEvidence {
        schema_version: 1,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: activity.evidence.snapshot_id.clone(),
        as_of_epoch: activity.evidence.as_of_epoch,
        complete_through_epoch,
        activity_evidence_sha256: activity.evidence_sha256.clone(),
        activity_artifact: source.clone(),
        source_activity_rows: source.row_count,
        batch_count,
        merge_count: merged.merge_count,
        identity_artifact: merged.final_run.checkpoint.artifact,
        leaf_artifact: leaf_run.checkpoint.artifact,
        max_batch_buffered_bytes,
        max_merge_buffered_bytes: merged.max_buffered_bytes,
        max_leaf_bytes,
        estimated_run_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
        batch_checkpoints,
        merge_checkpoints: merged.checkpoints,
        leaf_checkpoint: leaf_run.checkpoint_path.to_string_lossy().into_owned(),
    };
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    let completed = BoundedFlexibleDistinct {
        evidence,
        evidence_sha256: pensieve_lake::sha256_file(evidence_path.as_ref())?,
    };
    completed.validate_for_publication(
        &completed.evidence.snapshot_id,
        completed.evidence.as_of_epoch,
    )?;
    Ok(completed)
}

/// Merge matching complete-hour leaves for one aligned half-open window.
pub fn estimate_flexible_distinct_window(
    product: &BoundedFlexibleDistinct,
    since_epoch: u64,
    until_epoch: u64,
    kind: Option<u16>,
) -> Result<u64> {
    let estimates = estimate_flexible_distinct_windows(
        product,
        &[FlexibleDistinctWindow {
            since_epoch,
            until_epoch,
            kind,
        }],
    )?;
    estimates.into_iter().next().ok_or_else(|| {
        BoundedExecutionError::Invalid("missing flexible estimate".to_owned()).into()
    })
}

/// Validate once and estimate several windows with one bounded leaf scan.
pub fn estimate_flexible_distinct_windows(
    product: &BoundedFlexibleDistinct,
    windows: &[FlexibleDistinctWindow],
) -> Result<Vec<u64>> {
    product
        .validate_for_publication(&product.evidence.snapshot_id, product.evidence.as_of_epoch)?;
    estimate_validated_windows(product, windows)
}

/// Load, fully validate, and estimate several windows without a redundant validation pass.
pub fn load_and_estimate_flexible_distinct_windows(
    evidence_path: impl AsRef<Path>,
    windows: &[FlexibleDistinctWindow],
) -> Result<(BoundedFlexibleDistinct, Vec<u64>)> {
    let product = load_bounded_flexible_distinct(evidence_path)?;
    let estimates = estimate_validated_windows(&product, windows)?;
    Ok((product, estimates))
}

fn estimate_validated_windows(
    product: &BoundedFlexibleDistinct,
    windows: &[FlexibleDistinctWindow],
) -> Result<Vec<u64>> {
    let mut bounded = Vec::with_capacity(windows.len());
    for window in windows {
        if !window.since_epoch.is_multiple_of(SECONDS_PER_HOUR)
            || !window.until_epoch.is_multiple_of(SECONDS_PER_HOUR)
            || window.since_epoch > window.until_epoch
            || window.until_epoch > product.evidence.complete_through_epoch
        {
            return invalid("flexible distinct window is not a valid complete-hour interval");
        }
        let start_hour = u32::try_from(window.since_epoch / SECONDS_PER_HOUR).map_err(|_| {
            BoundedExecutionError::Invalid("window start hour exceeds u32".to_owned())
        })?;
        let end_hour = u32::try_from(window.until_epoch / SECONDS_PER_HOUR).map_err(|_| {
            BoundedExecutionError::Invalid("window end hour exceeds u32".to_owned())
        })?;
        bounded.push((
            start_hour,
            end_hour,
            window.kind,
            DistinctSketchUnion::new(),
        ));
    }
    let mut reader = LeafReader::open(Path::new(&product.evidence.leaf_artifact.path))?;
    while let Some(leaf) = reader.next()? {
        for (start_hour, end_hour, kind, union) in &mut bounded {
            if leaf.hour >= *start_hour
                && leaf.hour < *end_hour
                && kind.is_none_or(|kind| kind == leaf.kind)
            {
                union.push_serialized(&leaf.sketch).map_err(sketch_error)?;
            }
        }
    }
    Ok(bounded
        .into_iter()
        .map(|(_, _, _, union)| union.finish().estimate())
        .collect())
}

fn build_batch(
    activity: &BoundedFixedActivity,
    complete_through_epoch: u64,
    offset: u64,
    rows: u64,
    index: u64,
    root: &Path,
) -> Result<CompletedRun> {
    let stem = format!("batch-{index:08}");
    let completed = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let identity = run_identity(activity, "batch");
    let source = &activity.evidence.activity_artifact;
    let inputs = vec![source_chunk_input(source, offset, rows)?];
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &inputs)?
    {
        return Ok(completed_run(stem, completed, checkpoint_path, checkpoint));
    }

    let partial = unique_partial(&completed)?;
    let mut source_file = BufReader::new(File::open(&source.path)?);
    source_file.seek(SeekFrom::Start(
        offset
            .checked_mul(ACTIVITY_RECORD_BYTES as u64)
            .ok_or_else(|| {
                BoundedExecutionError::Invalid("activity source offset overflow".to_owned())
            })?,
    ))?;
    let capacity = usize::try_from(rows).map_err(|_| {
        BoundedExecutionError::Invalid("flexible batch rows exceed usize".to_owned())
    })?;
    let mut identities = Vec::<[u8; FLEXIBLE_DISTINCT_IDENTITY_BYTES]>::with_capacity(capacity);
    let mut previous_activity = None;
    for _ in 0..rows {
        let mut activity_record = [0_u8; ACTIVITY_RECORD_BYTES];
        source_file.read_exact(&mut activity_record)?;
        if previous_activity.is_some_and(|previous| previous >= activity_record) {
            return invalid("activity source chunk is not strictly sorted");
        }
        previous_activity = Some(activity_record);
        let created_at = u32::from_be_bytes(
            activity_record[32..36]
                .try_into()
                .expect("fixed activity timestamp"),
        );
        if u64::from(created_at) >= complete_through_epoch {
            continue;
        }
        let mut identity = [0_u8; FLEXIBLE_DISTINCT_IDENTITY_BYTES];
        identity[..4].copy_from_slice(&(created_at / 3_600).to_be_bytes());
        identity[4..6].copy_from_slice(&activity_record[36..38]);
        identity[6..].copy_from_slice(&activity_record[..32]);
        identities.push(identity);
    }
    identities.sort_unstable();
    identities.dedup();
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    for identity in &identities {
        writer.write_all(identity)?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let min_key = identities.first().map(hex::encode);
    let max_key = identities.last().map(hex::encode);
    let checkpoint = publish_run_checkpoint(
        &partial,
        &completed,
        &checkpoint_path,
        identity,
        inputs,
        to_u64(identities.len())?,
        min_key,
        max_key,
    )?;
    Ok(completed_run(stem, completed, checkpoint_path, checkpoint))
}

fn merge_all(
    mut runs: Vec<CompletedRun>,
    activity: &BoundedFixedActivity,
    fan_in: usize,
    root: &Path,
) -> Result<MergeOutcome> {
    let mut merge_count = 0_u64;
    let mut max_buffered_bytes = 0_usize;
    let mut checkpoints = Vec::new();
    let mut round = 0_u32;
    while runs.len() > 1 {
        let mut next = Vec::new();
        for (group_index, group) in runs.chunks(fan_in).enumerate() {
            if group.len() == 1 {
                next.push(group[0].clone());
                continue;
            }
            let inputs = group.iter().map(run_input).collect::<Vec<_>>();
            let digest = merge_identity(&inputs);
            let stem = format!("merge-{round:04}-{group_index:08}-{}", &digest[..16]);
            let completed = root.join(format!("{stem}.run"));
            let checkpoint_path = root.join(format!("{stem}.json"));
            let identity = run_identity(activity, "merge");
            let checkpoint = if let Some(checkpoint) =
                load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &inputs)?
            {
                checkpoint
            } else {
                let partial = unique_partial(&completed)?;
                let paths = group.iter().map(|run| run.path.clone()).collect::<Vec<_>>();
                let stats = merge_fixed_runs(
                    &paths,
                    &partial,
                    FixedRecordLayout {
                        record_bytes: FLEXIBLE_DISTINCT_IDENTITY_BYTES,
                        key_bytes: FLEXIBLE_DISTINCT_IDENTITY_BYTES,
                    },
                    fan_in,
                )?;
                validate_merge(group, stats)?;
                max_buffered_bytes = max_buffered_bytes.max(stats.peak_buffered_bytes);
                publish_run_checkpoint(
                    &partial,
                    &completed,
                    &checkpoint_path,
                    identity,
                    inputs,
                    stats.output_records,
                    group
                        .iter()
                        .filter_map(|run| run.checkpoint.artifact.min_key.clone())
                        .min(),
                    group
                        .iter()
                        .filter_map(|run| run.checkpoint.artifact.max_key.clone())
                        .max(),
                )?
            };
            max_buffered_bytes = max_buffered_bytes
                .max((group.len() + 1).saturating_mul(FLEXIBLE_DISTINCT_IDENTITY_BYTES));
            merge_count = checked_add(merge_count, 1, "flexible merge count")?;
            checkpoints.push(checkpoint_path.to_string_lossy().into_owned());
            next.push(completed_run(stem, completed, checkpoint_path, checkpoint));
        }
        runs = next;
        round = round.checked_add(1).ok_or_else(|| {
            BoundedExecutionError::Invalid("flexible merge round overflow".to_owned())
        })?;
    }
    Ok(MergeOutcome {
        final_run: runs.pop().expect("at least one flexible run"),
        merge_count,
        max_buffered_bytes,
        checkpoints,
    })
}

fn build_leaves(
    activity: &BoundedFixedActivity,
    identities: &CompletedRun,
    root: &Path,
) -> Result<(CompletedRun, usize)> {
    let completed = root.join("hour-kind-hll.leaves");
    let checkpoint_path = root.join("hour-kind-hll.json");
    let identity = run_identity(activity, "leaves");
    let inputs = vec![run_input(identities)];
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &inputs)?
    {
        let max_leaf = validate_leaf_stream(
            Path::new(&checkpoint.artifact.path),
            checkpoint.artifact.row_count,
            floor_hour(activity.evidence.as_of_epoch),
        )?;
        return Ok((
            completed_run(
                "hour-kind-hll".to_owned(),
                completed,
                checkpoint_path,
                checkpoint,
            ),
            max_leaf,
        ));
    }

    let partial = unique_partial(&completed)?;
    let mut reader = IdentityReader::open(&identities.path)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    let mut current_key = None;
    let mut builder = DistinctSketchBuilder::new();
    let mut leaves = 0_u64;
    let mut min_key = None;
    let mut max_key = None;
    let mut max_leaf_bytes = 0_usize;
    while let Some(record) = reader.next()? {
        let key: [u8; LEAF_KEY_BYTES] =
            record[..LEAF_KEY_BYTES].try_into().expect("fixed leaf key");
        if current_key.is_some_and(|current| current != key) {
            let completed_key = current_key.expect("current leaf key");
            let sketch = std::mem::take(&mut builder).finish().serialize();
            write_leaf(&mut writer, &completed_key, &sketch)?;
            max_leaf_bytes = max_leaf_bytes.max(sketch.len());
            leaves = checked_add(leaves, 1, "flexible leaf rows")?;
        }
        current_key = Some(key);
        builder
            .push(record[LEAF_KEY_BYTES..].try_into().expect("fixed pubkey"))
            .map_err(sketch_error)?;
    }
    if let Some(key) = current_key {
        let sketch = builder.finish().serialize();
        write_leaf(&mut writer, &key, &sketch)?;
        max_leaf_bytes = max_leaf_bytes.max(sketch.len());
        leaves = checked_add(leaves, 1, "flexible leaf rows")?;
        min_key = identities
            .checkpoint
            .artifact
            .min_key
            .as_ref()
            .map(|value| value[..LEAF_KEY_BYTES * 2].to_owned());
        max_key = Some(hex::encode(key));
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let checkpoint = publish_run_checkpoint(
        &partial,
        &completed,
        &checkpoint_path,
        identity,
        inputs,
        leaves,
        min_key,
        max_key,
    )?;
    Ok((
        completed_run(
            "hour-kind-hll".to_owned(),
            completed,
            checkpoint_path,
            checkpoint,
        ),
        max_leaf_bytes,
    ))
}

fn write_leaf(writer: &mut impl Write, key: &[u8; LEAF_KEY_BYTES], sketch: &[u8]) -> Result<()> {
    let length = u32::try_from(sketch.len()).map_err(|_| {
        BoundedExecutionError::Invalid("serialized flexible leaf exceeds u32".to_owned())
    })?;
    writer.write_all(key)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(sketch)?;
    Ok(())
}

struct IdentityReader {
    reader: BufReader<File>,
    previous: Option<[u8; FLEXIBLE_DISTINCT_IDENTITY_BYTES]>,
}

impl IdentityReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<[u8; FLEXIBLE_DISTINCT_IDENTITY_BYTES]>> {
        let mut record = [0_u8; FLEXIBLE_DISTINCT_IDENTITY_BYTES];
        if !read_exact_or_eof(&mut self.reader, &mut record)? {
            return Ok(None);
        }
        if self.previous.is_some_and(|previous| previous >= record) {
            return invalid("flexible identity artifact is not strictly sorted and unique");
        }
        self.previous = Some(record);
        Ok(Some(record))
    }
}

struct Leaf {
    hour: u32,
    kind: u16,
    key: [u8; LEAF_KEY_BYTES],
    sketch: Vec<u8>,
}

struct LeafReader {
    reader: BufReader<File>,
    previous: Option<[u8; LEAF_KEY_BYTES]>,
}

impl LeafReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<Leaf>> {
        let mut key = [0_u8; LEAF_KEY_BYTES];
        if !read_exact_or_eof(&mut self.reader, &mut key)? {
            return Ok(None);
        }
        if self.previous.is_some_and(|previous| previous >= key) {
            return invalid("flexible leaf artifact is not strictly sorted and unique");
        }
        let mut length = [0_u8; LEAF_LENGTH_BYTES];
        self.reader.read_exact(&mut length)?;
        let length = usize::try_from(u32::from_be_bytes(length)).expect("u32 fits usize");
        if length > MAX_SERIALIZED_SKETCH_BYTES {
            return invalid("serialized flexible leaf exceeds the fixed decode bound");
        }
        let mut sketch = vec![0_u8; length];
        self.reader.read_exact(&mut sketch)?;
        DistinctSketch::deserialize(&sketch).map_err(sketch_error)?;
        self.previous = Some(key);
        Ok(Some(Leaf {
            hour: u32::from_be_bytes(key[..4].try_into().expect("fixed hour")),
            kind: u16::from_be_bytes(key[4..].try_into().expect("fixed kind")),
            key,
            sketch,
        }))
    }
}

fn validate_leaf_artifact(
    identities: &ArtifactIdentity,
    leaves: &ArtifactIdentity,
    complete_through_epoch: u64,
    expected_max_leaf_bytes: usize,
) -> Result<()> {
    let path = Path::new(&leaves.path);
    if !path.metadata()?.is_file()
        || path.metadata()?.len() != leaves.byte_size
        || pensieve_lake::sha256_file(path)? != leaves.sha256
    {
        return invalid("flexible leaf artifact identity mismatch");
    }
    let observed_max = validate_leaf_stream(path, leaves.row_count, complete_through_epoch)?;
    if observed_max != expected_max_leaf_bytes {
        return invalid("flexible leaf maximum-size evidence mismatch");
    }

    let mut identity_reader = IdentityReader::open(Path::new(&identities.path))?;
    let mut leaf_reader = LeafReader::open(path)?;
    let mut current_leaf = leaf_reader.next()?;
    let mut current_key = None;
    let mut builder = DistinctSketchBuilder::new();
    let mut matched = 0_u64;
    while let Some(record) = identity_reader.next()? {
        let key: [u8; LEAF_KEY_BYTES] =
            record[..LEAF_KEY_BYTES].try_into().expect("fixed leaf key");
        if current_key.is_some_and(|current| current != key) {
            let expected_key = current_key.expect("validation leaf key");
            validate_one_leaf(
                current_leaf.take(),
                expected_key,
                std::mem::take(&mut builder),
            )?;
            matched = checked_add(matched, 1, "validated leaf rows")?;
            current_leaf = leaf_reader.next()?;
        }
        current_key = Some(key);
        builder
            .push(record[LEAF_KEY_BYTES..].try_into().expect("fixed pubkey"))
            .map_err(sketch_error)?;
    }
    if let Some(key) = current_key {
        validate_one_leaf(current_leaf.take(), key, builder)?;
        matched = checked_add(matched, 1, "validated leaf rows")?;
        current_leaf = leaf_reader.next()?;
    }
    if current_leaf.is_some() || matched != leaves.row_count {
        return invalid("flexible leaves do not exactly cover identity groups");
    }
    Ok(())
}

fn validate_one_leaf(
    leaf: Option<Leaf>,
    expected_key: [u8; LEAF_KEY_BYTES],
    builder: DistinctSketchBuilder,
) -> Result<()> {
    let leaf = leaf.ok_or_else(|| {
        BoundedExecutionError::Invalid("missing flexible leaf for identity group".to_owned())
    })?;
    if leaf.key != expected_key || leaf.sketch != builder.finish().serialize() {
        return invalid("flexible leaf does not match exact identity group");
    }
    Ok(())
}

fn validate_leaf_stream(
    path: &Path,
    expected_rows: u64,
    complete_through_epoch: u64,
) -> Result<usize> {
    let complete_hour = u32::try_from(complete_through_epoch / SECONDS_PER_HOUR)
        .map_err(|_| BoundedExecutionError::Invalid("complete hour exceeds u32".to_owned()))?;
    let mut reader = LeafReader::open(path)?;
    let mut rows = 0_u64;
    let mut max_leaf = 0_usize;
    while let Some(leaf) = reader.next()? {
        if leaf.hour >= complete_hour {
            return invalid("flexible leaf includes an incomplete UTC hour");
        }
        rows = checked_add(rows, 1, "flexible leaf rows")?;
        max_leaf = max_leaf.max(leaf.sketch.len());
    }
    if rows != expected_rows {
        return invalid("flexible leaf row count mismatch");
    }
    Ok(max_leaf)
}

fn validate_fixed_artifact(artifact: &ArtifactIdentity, record_bytes: usize) -> Result<()> {
    let path = Path::new(&artifact.path);
    let metadata = path.metadata()?;
    if !metadata.is_file()
        || metadata.len() != artifact.byte_size
        || artifact.byte_size != artifact.row_count.saturating_mul(record_bytes as u64)
        || pensieve_lake::sha256_file(path)? != artifact.sha256
    {
        return invalid("flexible distinct fixed artifact identity mismatch");
    }
    Ok(())
}

fn validate_merge(inputs: &[CompletedRun], stats: MergeStats) -> Result<()> {
    let expected = inputs.iter().try_fold(0_u64, |sum, run| {
        checked_add(
            sum,
            run.checkpoint.artifact.row_count,
            "flexible merge inputs",
        )
    })?;
    if stats.input_records != expected || stats.output_records + stats.duplicate_records != expected
    {
        return invalid("flexible distinct merge accounting mismatch");
    }
    Ok(())
}

fn validate_config(config: &FlexibleDistinctConfig) -> Result<()> {
    if config.source_records_per_batch == 0 || config.merge_fan_in < 2 {
        return invalid("invalid flexible distinct build configuration");
    }
    Ok(())
}

fn build_empty(activity: &BoundedFixedActivity, root: &Path) -> Result<CompletedRun> {
    let completed = root.join("empty.run");
    let checkpoint_path = root.join("empty.json");
    let identity = run_identity(activity, "empty");
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &[])?
    {
        return Ok(completed_run(
            "empty".to_owned(),
            completed,
            checkpoint_path,
            checkpoint,
        ));
    }
    let partial = unique_partial(&completed)?;
    File::create(&partial)?.sync_all()?;
    let checkpoint = publish_run_checkpoint(
        &partial,
        &completed,
        &checkpoint_path,
        identity,
        Vec::new(),
        0,
        None,
        None,
    )?;
    Ok(completed_run(
        "empty".to_owned(),
        completed,
        checkpoint_path,
        checkpoint,
    ))
}

fn run_identity(activity: &BoundedFixedActivity, phase: &str) -> RunIdentity {
    RunIdentity {
        snapshot_id: activity.evidence.snapshot_id.clone(),
        as_of: activity.evidence.as_of_epoch,
        product: format!("flexible-distinct-{phase}"),
        product_version: FLEXIBLE_DISTINCT_VERSION.to_owned(),
        key_space: if phase == "leaves" {
            "hour-u32-kind-u16-pensieve-hll-v1".to_owned()
        } else {
            "hour-u32-kind-u16-pubkey-32-v1".to_owned()
        },
    }
}

fn source_chunk_input(source: &ArtifactIdentity, offset: u64, rows: u64) -> Result<InputIdentity> {
    Ok(InputIdentity {
        identity: format!("activity:{}:{offset}:{rows}", source.sha256),
        byte_size: source.byte_size,
        row_count: source.row_count,
        sha256: source.sha256.clone(),
    })
}

fn completed_run(
    identity: String,
    path: PathBuf,
    checkpoint_path: PathBuf,
    checkpoint: RunCheckpoint,
) -> CompletedRun {
    CompletedRun {
        identity,
        path,
        checkpoint_path,
        checkpoint,
    }
}

fn run_input(run: &CompletedRun) -> InputIdentity {
    InputIdentity {
        identity: run.identity.clone(),
        byte_size: run.checkpoint.artifact.byte_size,
        row_count: run.checkpoint.artifact.row_count,
        sha256: run.checkpoint.artifact.sha256.clone(),
    }
}

fn merge_identity(inputs: &[InputIdentity]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-flexible-distinct-merge-v1\0");
    for input in inputs {
        digest.update(input.identity.as_bytes());
        digest.update([0]);
        digest.update(input.sha256.as_bytes());
        digest.update(input.byte_size.to_be_bytes());
        digest.update(input.row_count.to_be_bytes());
    }
    hex::encode(digest.finalize())
}

fn unique_partial(completed: &Path) -> Result<PathBuf> {
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = completed
        .file_name()
        .ok_or_else(|| BoundedExecutionError::Invalid("run path has no filename".to_owned()))?
        .to_string_lossy();
    Ok(completed.with_file_name(format!(
        ".{name}.{}.{}.partial",
        std::process::id(),
        sequence
    )))
}

fn floor_hour(epoch: u64) -> u64 {
    epoch - (epoch % SECONDS_PER_HOUR)
}

fn read_exact_or_eof(reader: &mut impl Read, bytes: &mut [u8]) -> Result<bool> {
    let mut offset = 0;
    while offset < bytes.len() {
        let read = reader.read(&mut bytes[offset..])?;
        if read == 0 {
            if offset == 0 {
                return Ok(false);
            }
            return invalid("flexible distinct artifact ends with a truncated record");
        }
        offset += read;
    }
    Ok(true)
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")).into())
}

fn to_u64(value: usize) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BoundedExecutionError::Invalid("count exceeds u64".to_owned()).into())
}

fn estimate_run_bytes(rows: u64, mut runs: usize, fan_in: usize) -> Result<u64> {
    let base = rows
        .checked_mul(FLEXIBLE_DISTINCT_IDENTITY_BYTES as u64)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("flexible byte estimate overflow".to_owned())
        })?;
    let mut rounds = 1_u64;
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        rounds = checked_add(rounds, 1, "flexible merge rounds")?;
    }
    base.checked_mul(rounds)
        .and_then(|runs| runs.checked_add(rows.saturating_mul(128)))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("flexible run estimate overflow".to_owned()).into()
        })
}

fn completed_run_bytes(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && matches!(
                    entry.path().extension().and_then(|ext| ext.to_str()),
                    Some("run" | "leaves")
                )
            {
                total = checked_add(total, entry.metadata()?.len(), "completed run bytes")?;
            }
        }
    }
    Ok(total)
}

fn sketch_error(error: crate::DistinctSketchError) -> crate::Error {
    BoundedExecutionError::Invalid(format!("flexible distinct sketch: {error}")).into()
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(BoundedExecutionError::Invalid(message.into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_reader_rejects_unbounded_or_truncated_payloads() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let oversized = directory.path().join("oversized.leaves");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(
            &u32::try_from(MAX_SERIALIZED_SKETCH_BYTES + 1)
                .expect("test length fits u32")
                .to_be_bytes(),
        );
        fs::write(&oversized, bytes).expect("write oversized leaf");
        assert!(
            LeafReader::open(&oversized)
                .expect("open oversized")
                .next()
                .is_err()
        );

        let truncated = directory.path().join("truncated.leaves");
        let sketch = DistinctSketch::from_sorted_identities([[1_u8; 32]])
            .expect("build sketch")
            .serialize();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&u32::try_from(sketch.len()).expect("length").to_be_bytes());
        bytes.extend_from_slice(&sketch[..sketch.len() - 1]);
        fs::write(&truncated, bytes).expect("write truncated leaf");
        assert!(
            LeafReader::open(&truncated)
                .expect("open truncated")
                .next()
                .is_err()
        );
    }

    #[test]
    fn identity_reader_rejects_order_regressions_and_terminal_truncation() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let unsorted = directory.path().join("unsorted.run");
        let mut first = [0_u8; FLEXIBLE_DISTINCT_IDENTITY_BYTES];
        first[3] = 2;
        let mut second = first;
        second[3] = 1;
        let mut bytes = first.to_vec();
        bytes.extend_from_slice(&second);
        fs::write(&unsorted, bytes).expect("write unsorted identities");
        let mut reader = IdentityReader::open(&unsorted).expect("open identities");
        assert!(reader.next().expect("first identity").is_some());
        assert!(reader.next().is_err());

        let truncated = directory.path().join("truncated.run");
        fs::write(&truncated, &first[..first.len() - 1]).expect("write truncation");
        assert!(
            IdentityReader::open(&truncated)
                .expect("open truncated identities")
                .next()
                .is_err()
        );
    }
}
