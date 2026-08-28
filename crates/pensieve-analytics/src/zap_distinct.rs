//! Bounded exact zap-participant identities and mergeable daily sketches.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ArtifactIdentity, BoundedExecutionError, BoundedSemanticFacts, DiskBudget, DistinctSketch,
    DistinctSketchBuilder, FixedRecordLayout, InputIdentity, Result, RunCheckpoint, RunIdentity,
    SEMANTIC_FACTS_RUNNER_VERSION, SemanticFactReader, SemanticPayload, ZAP_DISTINCT_SKETCH_LG_K,
    load_reusable_checkpoint, merge_fixed_runs, preflight_disk, publish_canonical_json,
    publish_run_checkpoint, read_run_checkpoint, validate_run_checkpoint,
};

/// Encoded bytes for `day_epoch`, participant role, and pubkey.
pub const ZAP_IDENTITY_BYTES: usize = 8 + 1 + 32;

/// Stable product version.
pub const ZAP_DISTINCT_VERSION: &str = "zap-distinct-daily-v3";

const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-zap-distinct-v3";
const MAX_RELATIVE_ERROR_PPM: u64 = 20_000;
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Zap participant role encoded in an identity key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZapParticipantRole {
    /// Validated uppercase `P` sender.
    Sender,
    /// Validated lowercase `p` recipient.
    Recipient,
}

impl ZapParticipantRole {
    fn code(self) -> u8 {
        match self {
            Self::Sender => 0,
            Self::Recipient => 1,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Sender),
            1 => Ok(Self::Recipient),
            _ => Err(BoundedExecutionError::Invalid(
                "zap identity has an invalid participant role".to_owned(),
            )
            .into()),
        }
    }
}

/// One immutable daily mergeable participant leaf.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ZapDistinctLeaf {
    /// UTC day start as Unix seconds.
    pub day_epoch: u64,
    /// Sender or recipient domain.
    pub role: ZapParticipantRole,
    /// Exact unique identities in this daily leaf.
    pub exact_identities: u64,
    /// Rounded sketch estimate for validation and serving diagnostics.
    pub estimated_identities: u64,
    /// Relative error in integer parts per million.
    pub relative_error_ppm: u64,
    /// Versioned deterministic HLL bytes.
    pub sketch: Vec<u8>,
}

/// Resource settings for one bounded zap-distinct build.
#[derive(Clone, Debug)]
pub struct ZapDistinctConfig {
    /// Dedicated immutable chunk/merge workspace.
    pub work_root: PathBuf,
    /// Maximum 41-byte identities held before sorting a chunk.
    pub chunk_records: usize,
    /// Maximum chunk/merge runs opened by one streaming merge.
    pub merge_fan_in: usize,
    /// Free work-filesystem bytes left untouched.
    pub disk_reserve_bytes: u64,
}

/// Canonical completion evidence for zap participant identities.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ZapDistinctEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner identity.
    pub runner_version: String,
    /// Completion status.
    pub status: String,
    /// Frozen source snapshot.
    pub snapshot_id: String,
    /// Frozen analytics boundary.
    pub as_of_epoch: u64,
    /// SHA-256 of the owning semantic evidence.
    pub semantic_evidence_sha256: String,
    /// SHA-256 of the owning semantic fact artifact.
    pub semantic_artifact_sha256: String,
    /// Valid participant occurrences before distinct reduction.
    pub physical_identities: u64,
    /// Exact unique `(day, role, pubkey)` identities.
    pub logical_identities: u64,
    /// Duplicate same-day participant occurrences removed.
    pub duplicate_identities: u64,
    /// Immutable sorted chunk count.
    pub chunk_count: u64,
    /// Immutable merge count.
    pub merge_count: u64,
    /// Final sorted exact identity artifact.
    pub identity_artifact: ArtifactIdentity,
    /// Daily HLL leaves, sorted by day and role.
    pub leaves: Vec<ZapDistinctLeaf>,
    /// Maximum encoded identities buffered while sorting.
    pub max_buffered_identity_bytes: usize,
    /// Maximum bytes in one serialized leaf.
    pub max_leaf_bytes: usize,
    /// HLL precision used by every daily participant leaf.
    pub sketch_lg_k: u8,
    /// Maximum accepted relative error.
    pub tolerance_ppm: u64,
    /// Configured disk reserve.
    pub disk_reserve_bytes: u64,
    /// Immutable chunk checkpoint paths.
    pub chunk_checkpoints: Vec<String>,
    /// Immutable merge checkpoint paths.
    pub merge_checkpoints: Vec<String>,
}

/// Fully validated bounded zap-distinct product.
pub struct BoundedZapDistinct {
    /// Final exact identity artifact.
    pub identity_path: PathBuf,
    /// Canonical completion evidence.
    pub evidence: ZapDistinctEvidence,
    /// SHA-256 of completion evidence.
    pub evidence_sha256: String,
}

impl BoundedZapDistinct {
    /// Revalidate immutable identities and checkpoints before publication.
    pub fn validate_for_publication(&self, semantic: &BoundedSemanticFacts) -> Result<()> {
        validate_evidence(&self.evidence, &self.identity_path, semantic)
    }
}

#[derive(Clone)]
struct CompletedRun {
    path: PathBuf,
    checkpoint_path: PathBuf,
    checkpoint: RunCheckpoint,
}

struct MergeOutcome {
    final_run: CompletedRun,
    merge_count: u64,
    duplicate_rows: u64,
    checkpoints: Vec<String>,
}

/// Build exact daily participant identities and deterministic HLL leaves.
pub fn build_bounded_zap_distinct(
    semantic: &BoundedSemanticFacts,
    evidence_path: impl AsRef<Path>,
    config: ZapDistinctConfig,
) -> Result<BoundedZapDistinct> {
    validate_config(semantic, &config)?;
    fs::create_dir_all(&config.work_root)?;
    let worst_case_identities = semantic
        .evidence
        .domain_counts
        .accepted_zaps
        .checked_mul(2)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("zap identity estimate overflowed".to_owned())
        })?;
    let chunk_records = u64::try_from(config.chunk_records).map_err(|_| {
        BoundedExecutionError::Invalid("zap chunk record bound exceeds u64".to_owned())
    })?;
    let initial_runs = usize::try_from(worst_case_identities.div_ceil(chunk_records))
        .map_err(|_| {
            BoundedExecutionError::Invalid("zap identity run count exceeds usize".to_owned())
        })?
        .max(1);
    let estimated_run_bytes =
        estimate_run_bytes(worst_case_identities, initial_runs, config.merge_fan_in)?;
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

    let chunk_root = config.work_root.join("chunks");
    fs::create_dir_all(&chunk_root)?;
    let semantic_input = semantic_input(semantic);
    let mut chunks = Vec::new();
    let mut buffer = Vec::with_capacity(config.chunk_records);
    let mut physical_identities = 0_u64;
    let mut chunk_duplicate_rows = 0_u64;
    let mut max_buffered_identity_bytes = 0_usize;
    let mut chunk_checkpoints = Vec::new();
    let mut reader = SemanticFactReader::new(BufReader::new(File::open(&semantic.artifact_path)?));
    while let Some(record) = reader.next_record()? {
        if record.created_at > semantic.evidence.as_of_epoch {
            continue;
        }
        if let SemanticPayload::Zap {
            sender_pubkey,
            recipient_pubkey,
            ..
        } = record.payload
        {
            let day = record.created_at - record.created_at % 86_400;
            for (role, pubkey) in [
                (ZapParticipantRole::Sender, sender_pubkey),
                (ZapParticipantRole::Recipient, recipient_pubkey),
            ] {
                if let Some(pubkey) = pubkey {
                    buffer.push(encode_identity(day, role, pubkey));
                    physical_identities =
                        checked_add(physical_identities, 1, "physical zap identities")?;
                    max_buffered_identity_bytes = max_buffered_identity_bytes.max(
                        buffer
                            .len()
                            .checked_mul(ZAP_IDENTITY_BYTES)
                            .ok_or_else(|| {
                                BoundedExecutionError::Invalid(
                                    "zap identity buffer accounting overflowed".to_owned(),
                                )
                            })?,
                    );
                    if buffer.len() == config.chunk_records {
                        let built = publish_chunk(
                            &mut buffer,
                            chunks.len(),
                            semantic,
                            &semantic_input,
                            &chunk_root,
                        )?;
                        chunk_duplicate_rows = checked_add(
                            chunk_duplicate_rows,
                            built.1,
                            "chunk duplicate identities",
                        )?;
                        chunk_checkpoints
                            .push(built.0.checkpoint_path.to_string_lossy().into_owned());
                        chunks.push(built.0);
                    }
                }
            }
        }
    }
    if !buffer.is_empty() {
        let built = publish_chunk(
            &mut buffer,
            chunks.len(),
            semantic,
            &semantic_input,
            &chunk_root,
        )?;
        chunk_duplicate_rows =
            checked_add(chunk_duplicate_rows, built.1, "chunk duplicate identities")?;
        chunk_checkpoints.push(built.0.checkpoint_path.to_string_lossy().into_owned());
        chunks.push(built.0);
    }
    if chunks.is_empty() {
        let empty = publish_empty(semantic, &semantic_input, &config.work_root)?;
        chunk_checkpoints.push(empty.checkpoint_path.to_string_lossy().into_owned());
        chunks.push(empty);
    }

    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_to_single(chunks, semantic, config.merge_fan_in, &merge_root)?;
    let logical_identities = merged.final_run.checkpoint.artifact.row_count;
    let duplicate_identities = physical_identities
        .checked_sub(logical_identities)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("logical zap identities exceed physical".to_owned())
        })?;
    if checked_add(
        chunk_duplicate_rows,
        merged.duplicate_rows,
        "zap duplicate reconciliation",
    )? != duplicate_identities
    {
        return Err(BoundedExecutionError::Invalid(
            "zap chunk and merge duplicates do not reconcile".to_owned(),
        )
        .into());
    }
    let leaves = build_leaves(&merged.final_run.path)?;
    let max_leaf_bytes = leaves
        .iter()
        .map(|leaf| leaf.sketch.len())
        .max()
        .unwrap_or(0);
    let evidence = ZapDistinctEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: semantic.evidence.snapshot_id.clone(),
        as_of_epoch: semantic.evidence.as_of_epoch,
        semantic_evidence_sha256: semantic.evidence_sha256.clone(),
        semantic_artifact_sha256: semantic.evidence.final_artifact.sha256.clone(),
        physical_identities,
        logical_identities,
        duplicate_identities,
        chunk_count: u64::try_from(chunk_checkpoints.len()).map_err(|_| {
            BoundedExecutionError::Invalid("zap chunk count exceeds u64".to_owned())
        })?,
        merge_count: merged.merge_count,
        identity_artifact: merged.final_run.checkpoint.artifact.clone(),
        leaves,
        max_buffered_identity_bytes,
        max_leaf_bytes,
        sketch_lg_k: ZAP_DISTINCT_SKETCH_LG_K,
        tolerance_ppm: MAX_RELATIVE_ERROR_PPM,
        disk_reserve_bytes: config.disk_reserve_bytes,
        chunk_checkpoints,
        merge_checkpoints: merged.checkpoints,
    };
    validate_evidence(&evidence, &merged.final_run.path, semantic)?;
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    Ok(BoundedZapDistinct {
        identity_path: merged.final_run.path,
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

/// Load and fully validate an existing zap-distinct product.
pub fn load_bounded_zap_distinct(
    evidence_path: impl AsRef<Path>,
    identity_path: impl AsRef<Path>,
    semantic: &BoundedSemanticFacts,
) -> Result<BoundedZapDistinct> {
    let evidence: ZapDistinctEvidence =
        serde_json::from_slice(&fs::read(&evidence_path)?).map_err(BoundedExecutionError::from)?;
    validate_evidence(&evidence, identity_path.as_ref(), semantic)?;
    Ok(BoundedZapDistinct {
        identity_path: identity_path.as_ref().to_owned(),
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

fn publish_chunk(
    buffer: &mut Vec<[u8; ZAP_IDENTITY_BYTES]>,
    index: usize,
    semantic: &BoundedSemanticFacts,
    semantic_input: &InputIdentity,
    root: &Path,
) -> Result<(CompletedRun, u64)> {
    buffer.sort_unstable();
    let physical_rows = buffer.len() as u64;
    buffer.dedup();
    let logical_rows = buffer.len() as u64;
    let stem = format!("chunk-{index:08}");
    let path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let inputs = vec![InputIdentity {
        identity: format!("{}#chunk-{index:08}", semantic_input.identity),
        ..semantic_input.clone()
    }];
    let identity = run_identity(semantic, "chunk");
    let expected_bytes = buffer.concat();
    let expected_sha = hex::encode(Sha256::digest(&expected_bytes));
    if let Some(checkpoint) = load_reusable_checkpoint(&checkpoint_path, &path, &identity, &inputs)?
    {
        if checkpoint.artifact.row_count != logical_rows
            || checkpoint.artifact.sha256 != expected_sha
        {
            return Err(BoundedExecutionError::Invalid(
                "reusable zap identity chunk differs from deterministic input".to_owned(),
            )
            .into());
        }
        buffer.clear();
        return Ok((
            CompletedRun {
                path,
                checkpoint_path,
                checkpoint,
            },
            physical_rows.checked_sub(logical_rows).ok_or_else(|| {
                BoundedExecutionError::Invalid(
                    "zap chunk logical identities exceed physical identities".to_owned(),
                )
            })?,
        ));
    }
    let partial = unique_partial(&path)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    writer.write_all(&expected_bytes)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let checkpoint = publish_run_checkpoint(
        &partial,
        &path,
        &checkpoint_path,
        identity,
        inputs,
        logical_rows,
        buffer.first().map(hex::encode),
        buffer.last().map(hex::encode),
    )?;
    buffer.clear();
    Ok((
        CompletedRun {
            path,
            checkpoint_path,
            checkpoint,
        },
        physical_rows.checked_sub(logical_rows).ok_or_else(|| {
            BoundedExecutionError::Invalid(
                "zap chunk logical identities exceed physical identities".to_owned(),
            )
        })?,
    ))
}

fn publish_empty(
    semantic: &BoundedSemanticFacts,
    input: &InputIdentity,
    root: &Path,
) -> Result<CompletedRun> {
    let path = root.join("empty.run");
    let checkpoint_path = root.join("empty.json");
    let identity = run_identity(semantic, "empty");
    let inputs = vec![input.clone()];
    if let Some(checkpoint) = load_reusable_checkpoint(&checkpoint_path, &path, &identity, &inputs)?
    {
        return Ok(CompletedRun {
            path,
            checkpoint_path,
            checkpoint,
        });
    }
    let partial = unique_partial(&path)?;
    File::create(&partial)?.sync_all()?;
    let checkpoint = publish_run_checkpoint(
        &partial,
        &path,
        &checkpoint_path,
        identity,
        inputs,
        0,
        None,
        None,
    )?;
    Ok(CompletedRun {
        path,
        checkpoint_path,
        checkpoint,
    })
}

fn merge_to_single(
    mut runs: Vec<CompletedRun>,
    semantic: &BoundedSemanticFacts,
    fan_in: usize,
    root: &Path,
) -> Result<MergeOutcome> {
    let mut round = 0_u32;
    let mut merge_count = 0_u64;
    let mut duplicate_rows = 0_u64;
    let mut checkpoints = Vec::new();
    while runs.len() > 1 {
        let mut next = Vec::new();
        for (group_index, group) in runs.chunks(fan_in).enumerate() {
            if group.len() == 1 {
                next.push(group[0].clone());
                continue;
            }
            let input_identities = group.iter().map(run_input).collect::<Vec<_>>();
            let digest = hex::encode(Sha256::digest(
                serde_json::to_vec(&input_identities).map_err(BoundedExecutionError::from)?,
            ));
            let stem = format!("merge-{round:04}-{group_index:08}-{}", &digest[..16]);
            let path = root.join(format!("{stem}.run"));
            let checkpoint_path = root.join(format!("{stem}.json"));
            let identity = run_identity(semantic, "merge");
            let completed = if let Some(checkpoint) =
                load_reusable_checkpoint(&checkpoint_path, &path, &identity, &input_identities)?
            {
                CompletedRun {
                    path,
                    checkpoint_path,
                    checkpoint,
                }
            } else {
                let partial = unique_partial(&path)?;
                let paths = group.iter().map(|run| run.path.clone()).collect::<Vec<_>>();
                let stats = merge_fixed_runs(
                    &paths,
                    &partial,
                    FixedRecordLayout {
                        record_bytes: ZAP_IDENTITY_BYTES,
                        key_bytes: ZAP_IDENTITY_BYTES,
                    },
                    fan_in,
                )?;
                let expected = group.iter().try_fold(0_u64, |sum, run| {
                    checked_add(sum, run.checkpoint.artifact.row_count, "zap merge input")
                })?;
                if stats.input_records != expected
                    || checked_add(
                        stats.output_records,
                        stats.duplicate_records,
                        "zap merge accounting",
                    )? != expected
                {
                    return Err(BoundedExecutionError::Invalid(
                        "zap identity merge accounting mismatch".to_owned(),
                    )
                    .into());
                }
                let checkpoint = publish_run_checkpoint(
                    &partial,
                    &path,
                    &checkpoint_path,
                    identity,
                    input_identities,
                    stats.output_records,
                    group
                        .iter()
                        .filter_map(|run| run.checkpoint.artifact.min_key.clone())
                        .min(),
                    group
                        .iter()
                        .filter_map(|run| run.checkpoint.artifact.max_key.clone())
                        .max(),
                )?;
                CompletedRun {
                    path,
                    checkpoint_path,
                    checkpoint,
                }
            };
            let inputs = group.iter().try_fold(0_u64, |sum, run| {
                checked_add(sum, run.checkpoint.artifact.row_count, "zap merge rows")
            })?;
            let output_rows = completed.checkpoint.artifact.row_count;
            let merge_duplicates = inputs.checked_sub(output_rows).ok_or_else(|| {
                BoundedExecutionError::Invalid(
                    "zap merge output exceeds input identities".to_owned(),
                )
            })?;
            duplicate_rows = checked_add(duplicate_rows, merge_duplicates, "zap merge duplicates")?;
            merge_count = checked_add(merge_count, 1, "zap merge count")?;
            checkpoints.push(completed.checkpoint_path.to_string_lossy().into_owned());
            next.push(completed);
        }
        runs = next;
        round = round.checked_add(1).ok_or_else(|| {
            BoundedExecutionError::Invalid("zap merge round overflowed".to_owned())
        })?;
    }
    Ok(MergeOutcome {
        final_run: runs.pop().expect("at least one zap identity run"),
        merge_count,
        duplicate_rows,
        checkpoints,
    })
}

fn build_leaves(path: &Path) -> Result<Vec<ZapDistinctLeaf>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut leaves = Vec::new();
    let mut current: Option<(u64, ZapParticipantRole, DistinctSketchBuilder, u64)> = None;
    while let Some(encoded) = read_identity(&mut reader)? {
        let (day, role, pubkey) = decode_identity(&encoded)?;
        match current.take() {
            Some((current_day, current_role, mut builder, count))
                if current_day == day && current_role == role =>
            {
                builder.push(pubkey).map_err(|error| {
                    BoundedExecutionError::Invalid(format!(
                        "zap identity leaf is not sorted: {error}"
                    ))
                })?;
                current = Some((current_day, current_role, builder, count + 1));
            }
            Some(previous) => {
                leaves.push(finish_leaf(previous)?);
                let mut builder = zap_sketch_builder()?;
                builder.push(pubkey).map_err(|error| {
                    BoundedExecutionError::Invalid(format!("build zap leaf: {error}"))
                })?;
                current = Some((day, role, builder, 1));
            }
            None => {
                let mut builder = zap_sketch_builder()?;
                builder.push(pubkey).map_err(|error| {
                    BoundedExecutionError::Invalid(format!("build zap leaf: {error}"))
                })?;
                current = Some((day, role, builder, 1));
            }
        }
    }
    if let Some(current) = current {
        leaves.push(finish_leaf(current)?);
    }
    Ok(leaves)
}

fn finish_leaf(
    (day_epoch, role, builder, exact_identities): (
        u64,
        ZapParticipantRole,
        DistinctSketchBuilder,
        u64,
    ),
) -> Result<ZapDistinctLeaf> {
    let sketch = builder.finish();
    let estimated_identities = sketch.estimate();
    let relative_error_ppm = relative_error_ppm(exact_identities, estimated_identities);
    if relative_error_ppm > MAX_RELATIVE_ERROR_PPM {
        return Err(BoundedExecutionError::Invalid(format!(
            "zap distinct leaf day={day_epoch} role={role:?} exact={exact_identities} \
             estimate={estimated_identities} error={relative_error_ppm} ppm exceeds tolerance"
        ))
        .into());
    }
    Ok(ZapDistinctLeaf {
        day_epoch,
        role,
        exact_identities,
        estimated_identities,
        relative_error_ppm,
        sketch: sketch.serialize(),
    })
}

fn validate_evidence(
    evidence: &ZapDistinctEvidence,
    path: &Path,
    semantic: &BoundedSemanticFacts,
) -> Result<()> {
    let bytes = fs::metadata(path)?.len();
    let expected_bytes = evidence
        .logical_identities
        .checked_mul(ZAP_IDENTITY_BYTES as u64)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("zap identity byte accounting overflowed".to_owned())
        })?;
    let reconciled_physical = evidence
        .logical_identities
        .checked_add(evidence.duplicate_identities)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("zap identity count accounting overflowed".to_owned())
        })?;
    let semantic_physical =
        semantic
            .evidence
            .rollups
            .zaps
            .values()
            .try_fold(0_u64, |sum, day| {
                checked_add(
                    checked_add(sum, day.validated_senders, "semantic zap senders")?,
                    day.validated_recipients,
                    "semantic zap recipients",
                )
            })?;
    if evidence.schema_version != EVIDENCE_SCHEMA_VERSION
        || evidence.runner_version != RUNNER_VERSION
        || evidence.status != "completed"
        || evidence.snapshot_id != semantic.evidence.snapshot_id
        || evidence.as_of_epoch != semantic.evidence.as_of_epoch
        || evidence.semantic_evidence_sha256 != semantic.evidence_sha256
        || evidence.semantic_artifact_sha256 != semantic.evidence.final_artifact.sha256
        || evidence.identity_artifact.byte_size != bytes
        || evidence.identity_artifact.row_count != evidence.logical_identities
        || evidence.identity_artifact.sha256 != pensieve_lake::sha256_file(path)?
        || bytes != expected_bytes
        || reconciled_physical != evidence.physical_identities
        || semantic_physical != evidence.physical_identities
        || evidence.sketch_lg_k != ZAP_DISTINCT_SKETCH_LG_K
        || evidence.tolerance_ppm != MAX_RELATIVE_ERROR_PPM
        || evidence.max_leaf_bytes
            != evidence
                .leaves
                .iter()
                .map(|leaf| leaf.sketch.len())
                .max()
                .unwrap_or(0)
    {
        return Err(BoundedExecutionError::Invalid(
            "zap distinct evidence identity or accounting mismatch".to_owned(),
        )
        .into());
    }
    let rebuilt = build_leaves(path)?;
    if rebuilt != evidence.leaves {
        return Err(BoundedExecutionError::Invalid(
            "zap distinct leaves do not match exact identity artifact".to_owned(),
        )
        .into());
    }
    for leaf in &evidence.leaves {
        let sketch = DistinctSketch::deserialize(&leaf.sketch).map_err(|error| {
            BoundedExecutionError::Invalid(format!("decode zap distinct leaf: {error}"))
        })?;
        if sketch.estimate() != leaf.estimated_identities
            || sketch.lg_k() != evidence.sketch_lg_k
            || relative_error_ppm(leaf.exact_identities, leaf.estimated_identities)
                != leaf.relative_error_ppm
            || leaf.relative_error_ppm > evidence.tolerance_ppm
        {
            return Err(BoundedExecutionError::Invalid(
                "zap distinct leaf estimate does not validate".to_owned(),
            )
            .into());
        }
    }
    validate_checkpoint_set(
        &evidence.chunk_checkpoints,
        evidence.chunk_count,
        evidence,
        &["zap-distinct-chunk", "zap-distinct-empty"],
    )?;
    validate_checkpoint_set(
        &evidence.merge_checkpoints,
        evidence.merge_count,
        evidence,
        &["zap-distinct-merge"],
    )?;
    Ok(())
}

fn zap_sketch_builder() -> Result<DistinctSketchBuilder> {
    DistinctSketchBuilder::with_lg_k(ZAP_DISTINCT_SKETCH_LG_K).map_err(|error| {
        BoundedExecutionError::Invalid(format!("create zap distinct sketch: {error}")).into()
    })
}

fn validate_checkpoint_set(
    paths: &[String],
    expected_count: u64,
    evidence: &ZapDistinctEvidence,
    allowed_products: &[&str],
) -> Result<()> {
    if u64::try_from(paths.len()).map_err(|_| {
        BoundedExecutionError::Invalid("zap checkpoint count exceeds u64".to_owned())
    })? != expected_count
    {
        return Err(BoundedExecutionError::Invalid(
            "zap checkpoint count does not match completion evidence".to_owned(),
        )
        .into());
    }
    let mut unique = BTreeSet::new();
    for path in paths {
        if !unique.insert(path) {
            return Err(BoundedExecutionError::Invalid(
                "zap completion evidence repeats a checkpoint path".to_owned(),
            )
            .into());
        }
        let checkpoint = read_run_checkpoint(path)?;
        if checkpoint.run.snapshot_id != evidence.snapshot_id
            || checkpoint.run.as_of != evidence.as_of_epoch
            || checkpoint.run.product_version != ZAP_DISTINCT_VERSION
            || !allowed_products.contains(&checkpoint.run.product.as_str())
        {
            return Err(BoundedExecutionError::Invalid(
                "zap checkpoint run identity differs from completion evidence".to_owned(),
            )
            .into());
        }
        validate_run_checkpoint(path, &checkpoint.artifact.path, &checkpoint)?;
    }
    Ok(())
}

fn validate_config(semantic: &BoundedSemanticFacts, config: &ZapDistinctConfig) -> Result<()> {
    if semantic.evidence.status != "completed"
        || semantic.evidence.runner_version != SEMANTIC_FACTS_RUNNER_VERSION
        || config.chunk_records == 0
        || config.merge_fan_in < 2
    {
        return Err(BoundedExecutionError::Invalid(
            "zap distinct source or bounded limits are invalid".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn semantic_input(semantic: &BoundedSemanticFacts) -> InputIdentity {
    InputIdentity {
        identity: format!("sha256:{}", semantic.evidence.final_artifact.sha256),
        byte_size: semantic.evidence.final_artifact.byte_size,
        row_count: semantic.evidence.final_artifact.row_count,
        sha256: semantic.evidence.final_artifact.sha256.clone(),
    }
}

fn run_identity(semantic: &BoundedSemanticFacts, stage: &str) -> RunIdentity {
    RunIdentity {
        snapshot_id: semantic.evidence.snapshot_id.clone(),
        as_of: semantic.evidence.as_of_epoch,
        product: format!("zap-distinct-{stage}"),
        product_version: ZAP_DISTINCT_VERSION.to_owned(),
        key_space: "day-u64-role-u8-pubkey-32-be-v1".to_owned(),
    }
}

fn run_input(run: &CompletedRun) -> InputIdentity {
    InputIdentity {
        identity: format!("sha256:{}", run.checkpoint.artifact.sha256),
        byte_size: run.checkpoint.artifact.byte_size,
        row_count: run.checkpoint.artifact.row_count,
        sha256: run.checkpoint.artifact.sha256.clone(),
    }
}

fn encode_identity(
    day_epoch: u64,
    role: ZapParticipantRole,
    pubkey: [u8; 32],
) -> [u8; ZAP_IDENTITY_BYTES] {
    let mut encoded = [0_u8; ZAP_IDENTITY_BYTES];
    encoded[..8].copy_from_slice(&day_epoch.to_be_bytes());
    encoded[8] = role.code();
    encoded[9..].copy_from_slice(&pubkey);
    encoded
}

fn decode_identity(
    encoded: &[u8; ZAP_IDENTITY_BYTES],
) -> Result<(u64, ZapParticipantRole, [u8; 32])> {
    let day = u64::from_be_bytes(encoded[..8].try_into().expect("fixed day"));
    if day % 86_400 != 0 {
        return Err(BoundedExecutionError::Invalid(
            "zap identity day is not UTC-aligned".to_owned(),
        )
        .into());
    }
    Ok((
        day,
        ZapParticipantRole::from_code(encoded[8])?,
        encoded[9..].try_into().expect("fixed pubkey"),
    ))
}

fn read_identity(reader: &mut impl Read) -> Result<Option<[u8; ZAP_IDENTITY_BYTES]>> {
    let mut encoded = [0_u8; ZAP_IDENTITY_BYTES];
    let mut offset = 0;
    while offset < encoded.len() {
        match reader.read(&mut encoded[offset..])? {
            0 if offset == 0 => return Ok(None),
            0 => {
                return Err(BoundedExecutionError::Invalid(
                    "truncated zap identity artifact".to_owned(),
                )
                .into());
            }
            count => offset += count,
        }
    }
    Ok(Some(encoded))
}

fn relative_error_ppm(exact: u64, estimate: u64) -> u64 {
    if exact == 0 {
        return u64::from(estimate != 0) * 1_000_000;
    }
    exact.abs_diff(estimate).saturating_mul(1_000_000) / exact
}

fn estimate_run_bytes(rows: u64, mut runs: usize, fan_in: usize) -> Result<u64> {
    let base = rows.checked_mul(ZAP_IDENTITY_BYTES as u64).ok_or_else(|| {
        BoundedExecutionError::Invalid("zap identity byte estimate overflowed".to_owned())
    })?;
    let mut generations = 1_u64;
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        generations = checked_add(generations, 1, "zap merge generations")?;
    }
    base.checked_mul(generations).ok_or_else(|| {
        BoundedExecutionError::Invalid("zap identity run estimate overflowed".to_owned()).into()
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
                && entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("run")
            {
                total = checked_add(total, entry.metadata()?.len(), "zap completed run bytes")?;
            }
        }
    }
    Ok(total)
}

fn unique_partial(path: &Path) -> Result<PathBuf> {
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file = path
        .file_name()
        .ok_or_else(|| BoundedExecutionError::Invalid("zap run path has no filename".to_owned()))?;
    Ok(path.with_file_name(format!(
        "{}.{}.{}.partial",
        file.to_string_lossy(),
        std::process::id(),
        sequence
    )))
}

fn checked_add(left: u64, right: u64, field: &'static str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{field} overflowed u64")).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_encoding_orders_day_role_and_pubkey() {
        let sender = encode_identity(86_400, ZapParticipantRole::Sender, [2; 32]);
        let recipient = encode_identity(86_400, ZapParticipantRole::Recipient, [1; 32]);
        let next_day = encode_identity(172_800, ZapParticipantRole::Sender, [0; 32]);
        assert!(sender < recipient);
        assert!(recipient < next_day);
        assert_eq!(
            decode_identity(&sender).expect("decode"),
            (86_400, ZapParticipantRole::Sender, [2; 32])
        );
    }

    #[test]
    fn exact_identity_artifact_builds_deterministic_tolerant_leaves() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("identities.run");
        let mut records = Vec::new();
        for value in 0..1_000_u16 {
            let mut pubkey = [0_u8; 32];
            pubkey[..2].copy_from_slice(&value.to_be_bytes());
            records.push(encode_identity(86_400, ZapParticipantRole::Sender, pubkey));
        }
        records.push(encode_identity(
            86_400,
            ZapParticipantRole::Recipient,
            [9; 32],
        ));
        let mut writer = BufWriter::new(File::create(&path).expect("create"));
        for record in &records {
            writer.write_all(record).expect("write");
        }
        writer.flush().expect("flush");
        let first = build_leaves(&path).expect("leaves");
        let second = build_leaves(&path).expect("repeat leaves");
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].exact_identities, 1_000);
        assert_eq!(
            DistinctSketch::deserialize(&first[0].sketch)
                .expect("decode")
                .lg_k(),
            ZAP_DISTINCT_SKETCH_LG_K
        );
        assert!(first[0].relative_error_ppm <= MAX_RELATIVE_ERROR_PPM);
        assert_eq!(first[1].estimated_identities, 1);
    }

    #[test]
    fn truncated_identity_fails_closed() {
        let bytes = [0_u8; ZAP_IDENTITY_BYTES - 1];
        assert!(read_identity(&mut bytes.as_slice()).is_err());
    }
}
