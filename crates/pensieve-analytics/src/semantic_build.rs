//! Resumable bounded construction of canonical Slice 7 semantic facts.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use duckdb::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{configure_execution, configure_remote_access};
use crate::event_facts::verify_local_batch_inputs;
use crate::{
    ArtifactIdentity, BatchLimits, BoundedExecutionError, BuildConfig, DiskBudget,
    FixedRecordLayout, InputIdentity, ObjectLocation, ResolvedSnapshot, Result, RunCheckpoint,
    RunIdentity, SEMANTIC_FACT_BYTES, SEMANTIC_FACT_KEY_BYTES, SemanticFactReader, SemanticPayload,
    SemanticRollups, load_reusable_checkpoint, merge_fixed_runs, plan_input_batches,
    preflight_disk, publish_canonical_json, publish_run_checkpoint, scan_semantic_facts,
};

/// Stable semantic version of the compact fact product.
pub const SEMANTIC_FACTS_VERSION: &str = "canonical-semantic-facts-v1";

const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-semantic-facts-v1";
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource and workspace settings for one bounded Slice 7 build.
#[derive(Clone, Debug)]
pub struct SemanticFactsConfig {
    /// Dedicated immutable run root.
    pub work_root: PathBuf,
    /// Maximum catalog bytes and rows in one DuckDB scan.
    pub batch_limits: BatchLimits,
    /// Maximum immutable runs opened by one streaming merge.
    pub merge_fan_in: usize,
    /// Free bytes left untouched on the work filesystem.
    pub disk_reserve_bytes: u64,
}

/// Exact domain accounting derived from unique compact facts.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticDomainCounts {
    /// Kind-1 originals.
    pub original_notes: u64,
    /// Kind-1 replies.
    pub replies: u64,
    /// Kind-7 reactions.
    pub reactions: u64,
    /// Kind-30023 articles.
    pub longform_articles: u64,
    /// Accepted kind-9735 zaps.
    pub accepted_zaps: u64,
    /// Rejected kind-9735 zaps.
    pub rejected_zaps: u64,
}

impl SemanticDomainCounts {
    fn total(&self) -> Result<u64> {
        [
            self.original_notes,
            self.replies,
            self.reactions,
            self.longform_articles,
            self.accepted_zaps,
            self.rejected_zaps,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| {
            total.checked_add(value).ok_or_else(|| {
                BoundedExecutionError::Invalid("semantic domain total overflowed".to_owned()).into()
            })
        })
    }
}

/// Measured bounded-state maxima for one completed build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticMemoryEvidence {
    /// Maximum compressed catalog bytes assigned to one scan.
    pub max_batch_bytes: u64,
    /// Maximum physical catalog rows assigned to one scan.
    pub max_batch_rows: u64,
    /// Maximum encoded bytes held by a streaming merge.
    pub max_merge_buffered_bytes: usize,
    /// UTC day keys retained for engagement.
    pub engagement_days: usize,
    /// UTC day keys retained for long-form.
    pub longform_days: usize,
    /// UTC day keys retained for zaps.
    pub zap_days: usize,
}

/// Immutable completion evidence for canonical Slice 7 facts and rollups.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticFactsEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Completion state; only `completed` is publishable.
    pub status: String,
    /// Frozen canonical catalog snapshot.
    pub snapshot_id: String,
    /// Fixed analytics boundary.
    pub as_of_epoch: u64,
    /// Catalog objects scanned.
    pub object_count: u64,
    /// Catalog physical rows covered by immutable inputs.
    pub physical_rows: u64,
    /// Relevant physical rows before ID deduplication.
    pub physical_relevant_rows: u64,
    /// Unique relevant semantic event IDs.
    pub logical_relevant_events: u64,
    /// Relevant duplicate rows suppressed across all stages.
    pub duplicate_relevant_rows: u64,
    /// Immutable batch runs.
    pub batch_count: u64,
    /// Immutable merge runs.
    pub merge_count: u64,
    /// Final compact fact artifact.
    pub final_artifact: ArtifactIdentity,
    /// Exact unique domain counts.
    pub domain_counts: SemanticDomainCounts,
    /// Exact additive UTC-day products.
    pub rollups: SemanticRollups,
    /// SHA-256 of canonical rollup JSON bytes.
    pub rollup_sha256: String,
    /// Configured disk reserve.
    pub disk_reserve_bytes: u64,
    /// Bounded-state evidence.
    pub memory: SemanticMemoryEvidence,
    /// Immutable batch checkpoint paths.
    pub batch_checkpoints: Vec<String>,
    /// Immutable merge checkpoint paths.
    pub merge_checkpoints: Vec<String>,
}

/// Completed bounded facts, rollups, and canonical evidence.
pub struct BoundedSemanticFacts {
    /// Final compact fact artifact path.
    pub artifact_path: PathBuf,
    /// Fully validated completion evidence.
    pub evidence: SemanticFactsEvidence,
    /// SHA-256 of canonical evidence JSON.
    pub evidence_sha256: String,
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
    max_buffered_bytes: usize,
    checkpoints: Vec<String>,
}

/// Build resumable compact semantic facts and exact additive rollups.
pub fn build_bounded_semantic_facts(
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build_config: BuildConfig,
    config: SemanticFactsConfig,
) -> Result<BoundedSemanticFacts> {
    let evidence_path = evidence_path.as_ref();
    validate_config(&snapshot, &build_config, &config)?;
    fs::create_dir_all(&config.work_root)?;
    let inputs = catalog_inputs(&snapshot)?;
    let batches = plan_input_batches(&inputs, config.batch_limits)?;
    let estimated_bytes = estimate_run_bytes(
        snapshot.catalog.totals().physical_rows,
        batches.len(),
        config.merge_fan_in,
    )?;
    preflight_disk(
        &config.work_root,
        DiskBudget {
            output_bytes: estimated_bytes,
            temporary_bytes: 0,
            retained_bytes: 0,
            reserve_bytes: config.disk_reserve_bytes,
        },
    )?;

    let connection = Connection::open_in_memory()?;
    configure_execution(&connection, &build_config)?;
    connection.execute_batch("SET TimeZone='UTC'; SET preserve_insertion_order=false")?;
    configure_remote_access(&connection, &snapshot, &build_config)?;

    let batch_root = config.work_root.join("batches");
    fs::create_dir_all(&batch_root)?;
    let mut runs = Vec::with_capacity(batches.len().max(1));
    let mut offset = 0_usize;
    let mut physical_relevant_rows = 0_u64;
    let mut batch_duplicate_rows = 0_u64;
    let mut max_batch_bytes = 0_u64;
    let mut max_batch_rows = 0_u64;
    let mut batch_checkpoints = Vec::with_capacity(batches.len());
    for batch in &batches {
        let end = offset.checked_add(batch.inputs.len()).ok_or_else(|| {
            BoundedExecutionError::Invalid("semantic batch offset overflowed".to_owned())
        })?;
        let locations = snapshot.locations.get(offset..end).ok_or_else(|| {
            BoundedExecutionError::Invalid("semantic batch locations are incomplete".to_owned())
        })?;
        let built = build_batch(
            &connection,
            &snapshot,
            &build_config,
            batch,
            locations,
            &batch_root,
        )?;
        physical_relevant_rows = checked_add(
            physical_relevant_rows,
            built.physical_relevant_rows,
            "physical relevant rows",
        )?;
        batch_duplicate_rows = checked_add(
            batch_duplicate_rows,
            built.duplicate_relevant_rows,
            "batch duplicate rows",
        )?;
        max_batch_bytes = max_batch_bytes.max(batch.byte_size);
        max_batch_rows = max_batch_rows.max(batch.row_count);
        batch_checkpoints.push(built.run.checkpoint_path.to_string_lossy().into_owned());
        runs.push(built.run);
        offset = end;
    }
    if offset != snapshot.locations.len() {
        return Err(BoundedExecutionError::Invalid(
            "semantic batches did not consume all snapshot locations".to_owned(),
        )
        .into());
    }
    if runs.is_empty() {
        runs.push(build_empty(&snapshot, &build_config, &config.work_root)?);
    }

    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_to_single(
        runs,
        &snapshot,
        &build_config,
        config.merge_fan_in,
        &merge_root,
    )?;
    let logical_relevant_events = merged.final_run.checkpoint.artifact.row_count;
    let duplicate_relevant_rows = physical_relevant_rows
        .checked_sub(logical_relevant_events)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid(
                "semantic logical rows exceed relevant physical rows".to_owned(),
            )
        })?;
    if checked_add(
        batch_duplicate_rows,
        merged.duplicate_rows,
        "semantic duplicate reconciliation",
    )? != duplicate_relevant_rows
    {
        return Err(BoundedExecutionError::Invalid(
            "semantic batch and merge duplicates do not reconcile".to_owned(),
        )
        .into());
    }

    let (rollups, domain_counts) = finalize(&merged.final_run.path)?;
    if domain_counts.total()? != logical_relevant_events {
        return Err(BoundedExecutionError::Invalid(
            "semantic domain counts do not reconcile to unique facts".to_owned(),
        )
        .into());
    }
    let rollup_bytes = serde_json::to_vec(&rollups).map_err(BoundedExecutionError::from)?;
    let rollup_sha256 = hex::encode(Sha256::digest(&rollup_bytes));
    let evidence = SemanticFactsEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build_config.as_of_epoch,
        object_count: snapshot.catalog.objects().len().try_into().map_err(|_| {
            BoundedExecutionError::Invalid("semantic object count exceeds u64".to_owned())
        })?,
        physical_rows: snapshot.catalog.totals().physical_rows,
        physical_relevant_rows,
        logical_relevant_events,
        duplicate_relevant_rows,
        batch_count: batches.len().try_into().map_err(|_| {
            BoundedExecutionError::Invalid("semantic batch count exceeds u64".to_owned())
        })?,
        merge_count: merged.merge_count,
        final_artifact: merged.final_run.checkpoint.artifact.clone(),
        domain_counts,
        memory: SemanticMemoryEvidence {
            max_batch_bytes,
            max_batch_rows,
            max_merge_buffered_bytes: merged.max_buffered_bytes,
            engagement_days: rollups.engagement.len(),
            longform_days: rollups.longform.len(),
            zap_days: rollups.zaps.len(),
        },
        rollups,
        rollup_sha256,
        disk_reserve_bytes: config.disk_reserve_bytes,
        batch_checkpoints,
        merge_checkpoints: merged.checkpoints,
    };
    publish_canonical_json(evidence_path, &evidence)?;
    let evidence_sha256 = pensieve_lake::sha256_file(evidence_path)?;
    Ok(BoundedSemanticFacts {
        artifact_path: merged.final_run.path,
        evidence,
        evidence_sha256,
    })
}

struct BuiltBatch {
    run: CompletedRun,
    physical_relevant_rows: u64,
    duplicate_relevant_rows: u64,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct BatchEvidence {
    schema_version: u32,
    runner_version: String,
    snapshot_id: String,
    as_of_epoch: u64,
    batch_index: usize,
    inputs: Vec<InputIdentity>,
    physical_relevant_rows: u64,
    logical_relevant_events: u64,
    duplicate_relevant_rows: u64,
    min_event_id: Option<String>,
    max_event_id: Option<String>,
    artifact_sha256: String,
}

fn build_batch(
    connection: &Connection,
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
    batch: &crate::InputBatch,
    locations: &[ObjectLocation],
    root: &Path,
) -> Result<BuiltBatch> {
    let stem = format!("batch-{:08}", batch.index);
    let completed_path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let stats_path = root.join(format!("{stem}-stats.json"));
    let identity = run_identity(snapshot, build_config, "batch");
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed_path, &identity, &batch.inputs)?
    {
        let stats: BatchEvidence =
            serde_json::from_slice(&fs::read(&stats_path)?).map_err(BoundedExecutionError::from)?;
        validate_batch_evidence(&stats, snapshot, build_config, batch, &checkpoint)?;
        return Ok(BuiltBatch {
            run: CompletedRun {
                path: completed_path,
                checkpoint_path,
                checkpoint,
            },
            physical_relevant_rows: stats.physical_relevant_rows,
            duplicate_relevant_rows: stats.duplicate_relevant_rows,
        });
    }
    verify_local_batch_inputs(&batch.inputs, locations)?;
    let partial = unique_partial(&completed_path)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let mut writer = BufWriter::new(file);
    let stats = scan_semantic_facts(connection, locations, build_config.as_of_epoch, &mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let batch_evidence = BatchEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build_config.as_of_epoch,
        batch_index: batch.index,
        inputs: batch.inputs.clone(),
        physical_relevant_rows: stats.physical_relevant_rows,
        logical_relevant_events: stats.logical_relevant_events,
        duplicate_relevant_rows: stats.duplicate_relevant_rows,
        min_event_id: stats.min_event_id.map(hex::encode),
        max_event_id: stats.max_event_id.map(hex::encode),
        artifact_sha256: pensieve_lake::sha256_file(&partial)?,
    };
    publish_canonical_json(&stats_path, &batch_evidence)?;
    let checkpoint = publish_run_checkpoint(
        &partial,
        &completed_path,
        &checkpoint_path,
        identity,
        batch.inputs.clone(),
        stats.logical_relevant_events,
        stats.min_event_id.map(hex::encode),
        stats.max_event_id.map(hex::encode),
    )?;
    Ok(BuiltBatch {
        run: CompletedRun {
            path: completed_path,
            checkpoint_path,
            checkpoint,
        },
        physical_relevant_rows: stats.physical_relevant_rows,
        duplicate_relevant_rows: stats.duplicate_relevant_rows,
    })
}

fn validate_batch_evidence(
    stats: &BatchEvidence,
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
    batch: &crate::InputBatch,
    checkpoint: &RunCheckpoint,
) -> Result<()> {
    let expected = BatchEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build_config.as_of_epoch,
        batch_index: batch.index,
        inputs: batch.inputs.clone(),
        physical_relevant_rows: stats.physical_relevant_rows,
        logical_relevant_events: checkpoint.artifact.row_count,
        duplicate_relevant_rows: stats.duplicate_relevant_rows,
        min_event_id: checkpoint.artifact.min_key.clone(),
        max_event_id: checkpoint.artifact.max_key.clone(),
        artifact_sha256: checkpoint.artifact.sha256.clone(),
    };
    if stats != &expected
        || checked_add(
            stats.logical_relevant_events,
            stats.duplicate_relevant_rows,
            "batch evidence accounting",
        )? != stats.physical_relevant_rows
    {
        return Err(BoundedExecutionError::Invalid(
            "reusable semantic batch evidence does not match its checkpoint".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn build_empty(
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
    root: &Path,
) -> Result<CompletedRun> {
    let path = root.join("empty.run");
    let checkpoint_path = root.join("empty.json");
    let inputs = vec![InputIdentity {
        identity: format!("catalog:{}", snapshot.catalog.snapshot_id),
        byte_size: 0,
        row_count: 0,
        sha256: snapshot
            .catalog
            .snapshot_id
            .strip_prefix("sha256:")
            .ok_or_else(|| BoundedExecutionError::Invalid("snapshot ID is not SHA-256".to_owned()))?
            .to_owned(),
    }];
    let identity = run_identity(snapshot, build_config, "empty");
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
    snapshot: &ResolvedSnapshot,
    config: &BuildConfig,
    fan_in: usize,
    root: &Path,
) -> Result<MergeOutcome> {
    let mut merge_count = 0_u64;
    let mut duplicate_rows = 0_u64;
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
            let built = merge_group(group, snapshot, config, round, group_index, fan_in, root)?;
            let input_rows = group.iter().try_fold(0_u64, |total, run| {
                checked_add(total, run.checkpoint.artifact.row_count, "merge input rows")
            })?;
            duplicate_rows = checked_add(
                duplicate_rows,
                input_rows
                    .checked_sub(built.checkpoint.artifact.row_count)
                    .ok_or_else(|| {
                        BoundedExecutionError::Invalid("merge output exceeds inputs".to_owned())
                    })?,
                "merge duplicate rows",
            )?;
            max_buffered_bytes = max_buffered_bytes.max(
                group
                    .len()
                    .checked_add(1)
                    .and_then(|count| count.checked_mul(SEMANTIC_FACT_BYTES))
                    .ok_or_else(|| {
                        BoundedExecutionError::Invalid("merge memory overflowed".to_owned())
                    })?,
            );
            merge_count = checked_add(merge_count, 1, "merge count")?;
            checkpoints.push(built.checkpoint_path.to_string_lossy().into_owned());
            next.push(built);
        }
        runs = next;
        round = round
            .checked_add(1)
            .ok_or_else(|| BoundedExecutionError::Invalid("merge round overflowed".to_owned()))?;
    }
    Ok(MergeOutcome {
        final_run: runs.pop().expect("at least one run"),
        merge_count,
        duplicate_rows,
        max_buffered_bytes,
        checkpoints,
    })
}

fn merge_group(
    inputs: &[CompletedRun],
    snapshot: &ResolvedSnapshot,
    config: &BuildConfig,
    round: u32,
    group_index: usize,
    fan_in: usize,
    root: &Path,
) -> Result<CompletedRun> {
    let input_identities = inputs.iter().map(run_input).collect::<Vec<_>>();
    let identity_bytes =
        serde_json::to_vec(&input_identities).map_err(BoundedExecutionError::from)?;
    let digest = hex::encode(Sha256::digest(identity_bytes));
    let stem = format!("merge-{round:04}-{group_index:08}-{}", &digest[..16]);
    let path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let identity = run_identity(snapshot, config, "merge");
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &path, &identity, &input_identities)?
    {
        return Ok(CompletedRun {
            path,
            checkpoint_path,
            checkpoint,
        });
    }
    let partial = unique_partial(&path)?;
    let paths = inputs
        .iter()
        .map(|run| run.path.clone())
        .collect::<Vec<_>>();
    let stats = merge_fixed_runs(
        &paths,
        &partial,
        FixedRecordLayout {
            record_bytes: SEMANTIC_FACT_BYTES,
            key_bytes: SEMANTIC_FACT_KEY_BYTES,
        },
        fan_in,
    )?;
    let expected = inputs.iter().try_fold(0_u64, |total, run| {
        checked_add(total, run.checkpoint.artifact.row_count, "merge rows")
    })?;
    if checked_add(
        stats.output_records,
        stats.duplicate_records,
        "merge accounting",
    )? != expected
        || stats.input_records != expected
    {
        return Err(BoundedExecutionError::Invalid("merge accounting mismatch".to_owned()).into());
    }
    let checkpoint = publish_run_checkpoint(
        &partial,
        &path,
        &checkpoint_path,
        identity,
        input_identities,
        stats.output_records,
        inputs
            .iter()
            .filter_map(|run| run.checkpoint.artifact.min_key.clone())
            .min(),
        inputs
            .iter()
            .filter_map(|run| run.checkpoint.artifact.max_key.clone())
            .max(),
    )?;
    Ok(CompletedRun {
        path,
        checkpoint_path,
        checkpoint,
    })
}

fn finalize(path: &Path) -> Result<(SemanticRollups, SemanticDomainCounts)> {
    let mut reader = SemanticFactReader::new(BufReader::new(File::open(path)?));
    let mut rollups = SemanticRollups::default();
    let mut counts = SemanticDomainCounts::default();
    while let Some(record) = reader.next_record()? {
        rollups.observe_record(&record).map_err(|message| {
            BoundedExecutionError::Invalid(format!("semantic rollup failed: {message}"))
        })?;
        let counter = match record.payload {
            SemanticPayload::OriginalNote => &mut counts.original_notes,
            SemanticPayload::Reply => &mut counts.replies,
            SemanticPayload::Reaction => &mut counts.reactions,
            SemanticPayload::Longform { .. } => &mut counts.longform_articles,
            SemanticPayload::Zap { .. } => &mut counts.accepted_zaps,
            SemanticPayload::RejectedZap(_) => &mut counts.rejected_zaps,
        };
        *counter = counter.checked_add(1).ok_or_else(|| {
            BoundedExecutionError::Invalid("semantic domain count overflowed".to_owned())
        })?;
    }
    Ok((rollups, counts))
}

fn validate_config(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    config: &SemanticFactsConfig,
) -> Result<()> {
    if snapshot.locations.len() != snapshot.catalog.objects().len() {
        return Err(BoundedExecutionError::Invalid(
            "semantic snapshot locations do not match objects".to_owned(),
        )
        .into());
    }
    if build.as_of_epoch == 0
        || config.batch_limits.max_bytes == 0
        || config.batch_limits.max_rows == 0
        || config.merge_fan_in < 2
    {
        return Err(BoundedExecutionError::Invalid(
            "semantic build limits and as-of must be positive".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn catalog_inputs(snapshot: &ResolvedSnapshot) -> Result<Vec<InputIdentity>> {
    snapshot
        .catalog
        .objects()
        .iter()
        .map(|object| {
            if object.row_count == 0 {
                return Err(BoundedExecutionError::Invalid(format!(
                    "active semantic input {} has zero rows",
                    object.object_key
                ))
                .into());
            }
            Ok(InputIdentity {
                identity: object.object_key.clone(),
                byte_size: object.byte_size,
                row_count: object.row_count,
                sha256: object.sha256.clone(),
            })
        })
        .collect()
}

fn estimate_run_bytes(rows: u64, batches: usize, fan_in: usize) -> Result<u64> {
    let mut generations = 1_u64;
    let mut runs = batches.max(1);
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        generations = checked_add(generations, 1, "semantic run generations")?;
    }
    rows.checked_mul(SEMANTIC_FACT_BYTES as u64)
        .and_then(|bytes| bytes.checked_mul(generations))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("semantic disk estimate overflowed".to_owned()).into()
        })
}

fn run_identity(snapshot: &ResolvedSnapshot, config: &BuildConfig, stage: &str) -> RunIdentity {
    RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: config.as_of_epoch,
        product: format!("canonical-semantic-facts-{stage}"),
        product_version: SEMANTIC_FACTS_VERSION.to_owned(),
        key_space: "event-id-32-semantic-fixed-115-v1".to_owned(),
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

fn unique_partial(completed: &Path) -> Result<PathBuf> {
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = completed
        .file_name()
        .ok_or_else(|| BoundedExecutionError::Invalid("run path has no filename".to_owned()))?
        .to_string_lossy();
    Ok(completed.with_file_name(format!(
        "{file_name}.{}.{}.partial",
        std::process::id(),
        sequence
    )))
}

fn checked_add(left: u64, right: u64, field: &'static str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{field} overflowed u64")).into())
}
