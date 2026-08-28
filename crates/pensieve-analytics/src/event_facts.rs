//! Compact canonical event facts for the bounded Slice A replacement.
//!
//! Each record is fixed width and sorted by event ID. The ID commits to the
//! timestamp and kind, so repeated IDs must have byte-identical committed
//! fields. Conflicts fail closed before any immutable checkpoint is published.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use chrono::{DateTime, Utc};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{
    API_TIMESTAMP_MAX, CompactRollups, configure_execution, configure_remote_access,
    create_from_compact_rollups, sql_string,
};
use crate::{
    AnalyticsBuild, ArtifactIdentity, BatchLimits, BoundedExecutionError, BuildConfig, DiskBudget,
    EventDaily, EventDailyKind, FixedRecordLayout, InputIdentity, KindAllTime, MergeStats,
    ObjectLocation, Overview, ResolvedSnapshot, Result, RunCheckpoint, RunIdentity,
    load_reusable_checkpoint, merge_fixed_runs, plan_input_batches, preflight_disk,
    publish_canonical_json, publish_run_checkpoint,
};

/// Encoded event-ID bytes at the start of every event fact.
pub const EVENT_FACT_KEY_BYTES: usize = 32;

/// Fixed encoded bytes for event ID, big-endian timestamp, and big-endian kind.
pub const EVENT_FACT_BYTES: usize = EVENT_FACT_KEY_BYTES + 8 + 2;

/// Semantic version of the bounded canonical event-fact product.
pub const EVENT_FACTS_VERSION: &str = "canonical-event-facts-v1";

const EVENT_FACTS_EVIDENCE_SCHEMA_VERSION: u32 = 1;
const EVENT_FACTS_RUNNER_VERSION: &str = "pensieve-analytics-event-facts-v1";
const SEVEN_DAYS_SECS: u64 = 7 * 24 * 60 * 60;
const THIRTY_DAYS_SECS: u64 = 30 * 24 * 60 * 60;
const HOURS_PER_SEVEN_DAYS: f64 = 168.0;
const KIND_DOMAIN: usize = 65_536;
const KIND_WORDS: usize = 1_024;
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource and workspace settings for one bounded event-fact canary.
#[derive(Clone, Debug)]
pub struct EventFactsConfig {
    /// Dedicated immutable run and evidence root.
    pub work_root: PathBuf,
    /// Hard catalog byte and row ceilings for each DuckDB scan.
    pub batch_limits: BatchLimits,
    /// Maximum immutable runs opened by one streaming merge.
    pub merge_fan_in: usize,
    /// Free bytes left untouched on the work filesystem.
    pub disk_reserve_bytes: u64,
}

/// Measured bounded-state maxima for one completed event-fact build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventFactsMemoryEvidence {
    /// Maximum catalog bytes assigned to one bounded scan.
    pub max_batch_bytes: u64,
    /// Maximum physical rows assigned to one bounded scan.
    pub max_batch_rows: u64,
    /// Maximum encoded record bytes held by a merge.
    pub max_merge_buffered_bytes: usize,
    /// Distinct UTC day counters retained during finalization.
    pub daily_keys: usize,
    /// Distinct UTC day/kind counters retained during finalization.
    pub daily_kind_keys: usize,
    /// Fixed all-time kind counter slots.
    pub kind_counter_slots: usize,
}

/// Canonical completion evidence for the bounded Slice A replacement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventFactsEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Completion state; only `completed` is publishable.
    pub status: String,
    /// Frozen canonical catalog snapshot.
    pub snapshot_id: String,
    /// Fixed analytics time boundary.
    pub as_of_epoch: u64,
    /// Catalog objects scanned.
    pub object_count: u64,
    /// Bounded batch runs completed.
    pub batch_count: u64,
    /// Fixed-fan-in merges completed or exactly reused.
    pub merge_count: u64,
    /// Catalog physical rows scanned.
    pub physical_rows: u64,
    /// Final unique event IDs.
    pub logical_events: u64,
    /// Exact physical rows removed by batch and merge deduplication.
    pub duplicate_rows: u64,
    /// Duplicates removed inside bounded scans.
    pub batch_duplicate_rows: u64,
    /// Duplicates removed across immutable runs.
    pub merge_duplicate_rows: u64,
    /// Final completed event-fact artifact.
    pub final_artifact: ArtifactIdentity,
    /// SHA-256 of canonical Slice A metric bytes.
    pub metric_sha256: String,
    /// Conservative total bytes for batch and merge run artifacts.
    pub estimated_run_bytes: u64,
    /// Configured free-space reserve enforced before work begins.
    pub disk_reserve_bytes: u64,
    /// Bounded-state maxima and explicit time/key state.
    pub memory: EventFactsMemoryEvidence,
    /// Immutable batch checkpoint paths.
    pub batch_checkpoints: Vec<String>,
    /// Immutable merge checkpoint paths.
    pub merge_checkpoints: Vec<String>,
}

/// Completed canary analytics products and their immutable evidence.
pub struct BoundedEventBuild {
    /// Compact Slice A products ready for the existing atomic publisher.
    pub analytics: AnalyticsBuild,
    /// Exact bounded-build evidence.
    pub evidence: EventFactsEvidence,
    /// SHA-256 of the canonical evidence file.
    pub evidence_sha256: String,
}

/// Load and fully revalidate immutable event-fact evidence for a downstream lane.
pub fn load_event_facts_evidence(path: impl AsRef<Path>) -> Result<(EventFactsEvidence, String)> {
    let path = path.as_ref();
    let evidence: EventFactsEvidence =
        serde_json::from_slice(&fs::read(path)?).map_err(|error| {
            BoundedExecutionError::Invalid(format!("decode event-facts evidence: {error}"))
        })?;
    if evidence.schema_version != EVENT_FACTS_EVIDENCE_SCHEMA_VERSION
        || evidence.runner_version != EVENT_FACTS_RUNNER_VERSION
        || evidence.status != "completed"
        || evidence.final_artifact.row_count != evidence.logical_events
    {
        return Err(BoundedExecutionError::Invalid(
            "event-facts evidence is not a completed canonical product".to_owned(),
        )
        .into());
    }
    let expected_bytes = evidence
        .logical_events
        .checked_mul(u64::try_from(EVENT_FACT_BYTES).expect("event-fact width fits u64"))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("event-fact byte accounting overflowed".to_owned())
        })?;
    let artifact_path = Path::new(&evidence.final_artifact.path);
    if evidence.final_artifact.byte_size != expected_bytes
        || artifact_path.metadata()?.len() != expected_bytes
        || pensieve_lake::sha256_file(artifact_path)? != evidence.final_artifact.sha256
    {
        return Err(BoundedExecutionError::Invalid(
            "event-facts artifact identity does not match evidence".to_owned(),
        )
        .into());
    }
    let mut reader = EventFactReader::open(artifact_path)?;
    let mut previous = None;
    let mut rows = 0_u64;
    while let Some(fact) = reader.read_next()? {
        if previous.is_some_and(|id| id >= fact.id) {
            return Err(BoundedExecutionError::Invalid(
                "event-facts artifact is not strictly sorted and unique".to_owned(),
            )
            .into());
        }
        previous = Some(fact.id);
        rows = checked_add(rows, 1, "validated event-fact rows")?;
    }
    if rows != evidence.logical_events {
        return Err(BoundedExecutionError::Invalid(
            "event-facts artifact row count differs from evidence".to_owned(),
        )
        .into());
    }
    Ok((evidence, pensieve_lake::sha256_file(path)?))
}

/// Build the bounded canonical event-fact canary and compact Slice A products.
///
/// This path is intentionally separate from the live incremental DuckDB
/// checkpoint. It writes only below `work_root` and the dedicated analytics
/// database selected by the caller.
pub fn build_bounded_event_facts(
    work_database: impl AsRef<Path>,
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build_config: BuildConfig,
    facts_config: EventFactsConfig,
) -> Result<BoundedEventBuild> {
    validate_config(&snapshot, &build_config, &facts_config)?;
    fs::create_dir_all(&facts_config.work_root)?;
    let inputs = catalog_inputs(&snapshot)?;
    let batches = plan_input_batches(&inputs, facts_config.batch_limits)?;
    let estimated_run_bytes = estimate_run_bytes(
        snapshot.catalog.totals().physical_rows,
        batches.len(),
        facts_config.merge_fan_in,
    )?;
    let retained_bytes = completed_run_bytes(&facts_config.work_root)?;
    let remaining_run_bytes = estimated_run_bytes.saturating_sub(retained_bytes);
    preflight_disk(
        &facts_config.work_root,
        DiskBudget {
            output_bytes: remaining_run_bytes,
            temporary_bytes: 0,
            retained_bytes,
            reserve_bytes: facts_config.disk_reserve_bytes,
        },
    )?;
    let connection = Connection::open_in_memory()?;
    configure_execution(&connection, &build_config)?;
    connection.execute_batch("SET TimeZone = 'UTC'; SET preserve_insertion_order = false")?;
    configure_remote_access(&connection, &snapshot, &build_config)?;

    let batch_root = facts_config.work_root.join("batches");
    fs::create_dir_all(&batch_root)?;
    let mut completed = Vec::with_capacity(batches.len());
    let mut offset = 0_usize;
    let mut batch_duplicate_rows = 0_u64;
    let mut batch_checkpoints = Vec::with_capacity(batches.len());
    let mut max_batch_bytes = 0_u64;
    let mut max_batch_rows = 0_u64;
    for batch in &batches {
        let end = offset.checked_add(batch.inputs.len()).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch location offset overflowed usize".to_owned())
        })?;
        let locations = snapshot.locations.get(offset..end).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch locations do not cover frozen inputs".to_owned())
        })?;
        let run = build_batch_run(
            &connection,
            &snapshot,
            &build_config,
            batch,
            locations,
            &batch_root,
        )?;
        batch_duplicate_rows = checked_add(
            batch_duplicate_rows,
            batch
                .row_count
                .checked_sub(run.checkpoint.artifact.row_count)
                .ok_or_else(|| {
                    BoundedExecutionError::Invalid(
                        "batch artifact has more logical rows than physical inputs".to_owned(),
                    )
                })?,
            "batch duplicate rows",
        )?;
        max_batch_bytes = max_batch_bytes.max(batch.byte_size);
        max_batch_rows = max_batch_rows.max(batch.row_count);
        batch_checkpoints.push(run.checkpoint_path.to_string_lossy().into_owned());
        completed.push(run);
        offset = end;
    }
    if offset != snapshot.locations.len() {
        return Err(BoundedExecutionError::Invalid(
            "bounded batches did not consume every frozen object location".to_owned(),
        )
        .into());
    }

    if completed.is_empty() {
        completed.push(build_empty_run(
            &snapshot,
            &build_config,
            &facts_config.work_root,
        )?);
    }
    let merge_root = facts_config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merge = merge_to_single(
        completed,
        &snapshot,
        &build_config,
        facts_config.merge_fan_in,
        &merge_root,
    )?;
    let final_run = merge.final_run;
    let physical_rows = snapshot.catalog.totals().physical_rows;
    let logical_events = final_run.checkpoint.artifact.row_count;
    let duplicate_rows = physical_rows.checked_sub(logical_events).ok_or_else(|| {
        BoundedExecutionError::Invalid(format!(
            "catalog has {physical_rows} physical rows but final facts have {logical_events} rows"
        ))
    })?;
    if checked_add(
        batch_duplicate_rows,
        merge.duplicate_rows,
        "total duplicate rows",
    )? != duplicate_rows
    {
        return Err(BoundedExecutionError::Invalid(
            "batch and merge duplicate accounting does not reconcile to physical rows".to_owned(),
        )
        .into());
    }

    let finalized = finalize_rollups(
        &final_run.path,
        &final_run.checkpoint.artifact,
        build_config.as_of_epoch,
    )?;
    if finalized.rollups.logical_events != logical_events {
        return Err(BoundedExecutionError::Invalid(
            "rollup finalization did not consume every canonical event fact".to_owned(),
        )
        .into());
    }
    let analytics = create_from_compact_rollups(
        work_database,
        snapshot.clone(),
        build_config.clone(),
        finalized.rollups,
    )?;
    let metric_bytes = analytics.canonical_metric_bytes()?;
    let metric_sha256 = hex::encode(Sha256::digest(&metric_bytes));
    let object_count = to_u64(snapshot.catalog.objects().len(), "object count")?;
    let batch_count = to_u64(batches.len(), "batch count")?;
    let evidence = EventFactsEvidence {
        schema_version: EVENT_FACTS_EVIDENCE_SCHEMA_VERSION,
        runner_version: EVENT_FACTS_RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build_config.as_of_epoch,
        object_count,
        batch_count,
        merge_count: merge.merge_count,
        physical_rows,
        logical_events,
        duplicate_rows,
        batch_duplicate_rows,
        merge_duplicate_rows: merge.duplicate_rows,
        final_artifact: final_run.checkpoint.artifact,
        metric_sha256,
        estimated_run_bytes,
        disk_reserve_bytes: facts_config.disk_reserve_bytes,
        memory: EventFactsMemoryEvidence {
            max_batch_bytes,
            max_batch_rows,
            max_merge_buffered_bytes: merge.max_buffered_bytes,
            daily_keys: finalized.daily_keys,
            daily_kind_keys: finalized.daily_kind_keys,
            kind_counter_slots: KIND_DOMAIN,
        },
        batch_checkpoints,
        merge_checkpoints: merge.checkpoints,
    };
    let evidence_path = evidence_path.as_ref();
    publish_canonical_json(evidence_path, &evidence)?;
    let evidence_sha256 = pensieve_lake::sha256_file(evidence_path)?;
    Ok(BoundedEventBuild {
        analytics,
        evidence,
        evidence_sha256,
    })
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
    duplicate_rows: u64,
    max_buffered_bytes: usize,
    checkpoints: Vec<String>,
}

struct FinalizedRollups {
    rollups: CompactRollups,
    daily_keys: usize,
    daily_kind_keys: usize,
}

/// One compact canonical event fact.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct EventFact {
    /// Raw 32-byte Nostr event ID.
    pub id: [u8; EVENT_FACT_KEY_BYTES],
    /// Unsigned event timestamp committed by the ID.
    pub created_at: u64,
    /// Unsigned Nostr event kind committed by the ID.
    pub kind: u16,
}

impl EventFact {
    /// Encode the stable fixed-width on-disk representation.
    pub fn encode(self) -> [u8; EVENT_FACT_BYTES] {
        let mut bytes = [0_u8; EVENT_FACT_BYTES];
        bytes[..EVENT_FACT_KEY_BYTES].copy_from_slice(&self.id);
        bytes[EVENT_FACT_KEY_BYTES..EVENT_FACT_KEY_BYTES + 8]
            .copy_from_slice(&self.created_at.to_be_bytes());
        bytes[EVENT_FACT_KEY_BYTES + 8..].copy_from_slice(&self.kind.to_be_bytes());
        bytes
    }

    /// Decode one exact fixed-width record.
    pub fn decode(bytes: [u8; EVENT_FACT_BYTES]) -> Self {
        let mut id = [0_u8; EVENT_FACT_KEY_BYTES];
        id.copy_from_slice(&bytes[..EVENT_FACT_KEY_BYTES]);
        let mut created_at = [0_u8; 8];
        created_at.copy_from_slice(&bytes[EVENT_FACT_KEY_BYTES..EVENT_FACT_KEY_BYTES + 8]);
        let mut kind = [0_u8; 2];
        kind.copy_from_slice(&bytes[EVENT_FACT_KEY_BYTES + 8..]);
        Self {
            id,
            created_at: u64::from_be_bytes(created_at),
            kind: u16::from_be_bytes(kind),
        }
    }

    fn id_hex(self) -> String {
        hex::encode(self.id)
    }
}

/// Physical and logical accounting for one bounded batch run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EventFactBatchStats {
    /// Physical Parquet rows consumed.
    pub physical_rows: u64,
    /// Canonical event facts emitted.
    pub logical_events: u64,
    /// Byte-identical duplicate rows suppressed inside the batch.
    pub duplicate_rows: u64,
    /// Minimum event ID when the batch is non-empty.
    pub min_event_id: Option<String>,
    /// Maximum event ID when the batch is non-empty.
    pub max_event_id: Option<String>,
}

/// Streaming reader for a completed fixed-width event-fact run.
pub struct EventFactReader {
    reader: BufReader<File>,
}

impl EventFactReader {
    /// Open a completed event-fact run.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
        })
    }

    /// Read the next fact, rejecting a truncated terminal record.
    pub fn read_next(&mut self) -> Result<Option<EventFact>> {
        let mut bytes = [0_u8; EVENT_FACT_BYTES];
        let mut offset = 0;
        while offset < bytes.len() {
            let read = self.reader.read(&mut bytes[offset..])?;
            if read == 0 {
                if offset == 0 {
                    return Ok(None);
                }
                return Err(BoundedExecutionError::Invalid(
                    "event-fact run ends with a truncated record".to_owned(),
                )
                .into());
            }
            offset += read;
        }
        Ok(Some(EventFact::decode(bytes)))
    }
}

fn validate_config(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    facts: &EventFactsConfig,
) -> Result<()> {
    if build.as_of_epoch > API_TIMESTAMP_MAX {
        return Err(BoundedExecutionError::Invalid(format!(
            "as_of {} exceeds the Slice A timestamp maximum {API_TIMESTAMP_MAX}",
            build.as_of_epoch
        ))
        .into());
    }
    if snapshot.locations.len() != snapshot.catalog.objects().len() {
        return Err(BoundedExecutionError::Invalid(format!(
            "snapshot has {} objects but {} resolved locations",
            snapshot.catalog.objects().len(),
            snapshot.locations.len()
        ))
        .into());
    }
    if facts.merge_fan_in < 2 {
        return Err(BoundedExecutionError::Invalid(
            "event-fact merge fan-in must be at least two".to_owned(),
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
                    "active raw object {} has zero rows",
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

fn build_batch_run(
    connection: &Connection,
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
    batch: &crate::InputBatch,
    locations: &[ObjectLocation],
    root: &Path,
) -> Result<CompletedRun> {
    let stem = format!("batch-{:08}", batch.index);
    let completed_path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let run_identity = RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: build_config.as_of_epoch,
        product: "canonical-event-facts-batch".to_owned(),
        product_version: EVENT_FACTS_VERSION.to_owned(),
        key_space: "event-id-32-created-at-u64-kind-u16-be-v1".to_owned(),
    };
    if let Some(checkpoint) = load_reusable_checkpoint(
        &checkpoint_path,
        &completed_path,
        &run_identity,
        &batch.inputs,
    )? {
        return Ok(completed_run(
            stem,
            completed_path,
            checkpoint_path,
            checkpoint,
        ));
    }
    verify_local_batch_inputs(&batch.inputs, locations)?;
    let partial_path = unique_partial(&completed_path)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial_path)?;
    let mut writer = BufWriter::new(file);
    let stats = scan_batch_event_facts(connection, locations, &mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if stats.physical_rows != batch.row_count {
        return Err(BoundedExecutionError::Invalid(format!(
            "batch {} scanned {} physical rows, catalog requires {}",
            batch.index, stats.physical_rows, batch.row_count
        ))
        .into());
    }
    let checkpoint = publish_run_checkpoint(
        &partial_path,
        &completed_path,
        &checkpoint_path,
        run_identity,
        batch.inputs.clone(),
        stats.logical_events,
        stats.min_event_id,
        stats.max_event_id,
    )?;
    Ok(completed_run(
        stem,
        completed_path,
        checkpoint_path,
        checkpoint,
    ))
}

fn build_empty_run(
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
    root: &Path,
) -> Result<CompletedRun> {
    let stem = "empty-snapshot".to_owned();
    let completed_path = root.join("empty-snapshot.run");
    let checkpoint_path = root.join("empty-snapshot.json");
    let run_identity = RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: build_config.as_of_epoch,
        product: "canonical-event-facts-empty".to_owned(),
        product_version: EVENT_FACTS_VERSION.to_owned(),
        key_space: "event-id-32-created-at-u64-kind-u16-be-v1".to_owned(),
    };
    let snapshot_sha256 = snapshot
        .catalog
        .snapshot_id
        .strip_prefix("sha256:")
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("snapshot ID is not SHA-256 content ID".to_owned())
        })?
        .to_owned();
    let inputs = vec![InputIdentity {
        identity: format!("catalog: {}", snapshot.catalog.snapshot_id),
        byte_size: 0,
        row_count: 0,
        sha256: snapshot_sha256,
    }];
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed_path, &run_identity, &inputs)?
    {
        return Ok(completed_run(
            stem,
            completed_path,
            checkpoint_path,
            checkpoint,
        ));
    }
    let partial_path = unique_partial(&completed_path)?;
    File::create(&partial_path)?.sync_all()?;
    let checkpoint = publish_run_checkpoint(
        &partial_path,
        &completed_path,
        &checkpoint_path,
        run_identity,
        inputs,
        0,
        None,
        None,
    )?;
    Ok(completed_run(
        stem,
        completed_path,
        checkpoint_path,
        checkpoint,
    ))
}

fn merge_to_single(
    mut runs: Vec<CompletedRun>,
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
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
            let merged = build_merge_run(
                group,
                snapshot,
                build_config,
                round,
                group_index,
                fan_in,
                root,
            )?;
            let input_rows = checked_sum(
                group.iter().map(|run| run.checkpoint.artifact.row_count),
                "merge input rows",
            )?;
            let removed = input_rows
                .checked_sub(merged.run.checkpoint.artifact.row_count)
                .ok_or_else(|| {
                    BoundedExecutionError::Invalid("merge output rows exceed input rows".to_owned())
                })?;
            duplicate_rows = checked_add(duplicate_rows, removed, "merge duplicate rows")?;
            max_buffered_bytes = max_buffered_bytes.max(merged.peak_buffered_bytes);
            merge_count = checked_add(merge_count, 1, "merge count")?;
            checkpoints.push(merged.run.checkpoint_path.to_string_lossy().into_owned());
            next.push(merged.run);
        }
        runs = next;
        round = round.checked_add(1).ok_or_else(|| {
            BoundedExecutionError::Invalid("merge round overflowed u32".to_owned())
        })?;
    }
    Ok(MergeOutcome {
        final_run: runs
            .pop()
            .expect("event-fact execution always has an empty or non-empty run"),
        merge_count,
        duplicate_rows,
        max_buffered_bytes,
        checkpoints,
    })
}

struct BuiltMerge {
    run: CompletedRun,
    peak_buffered_bytes: usize,
}

#[allow(clippy::too_many_arguments)]
fn build_merge_run(
    inputs: &[CompletedRun],
    snapshot: &ResolvedSnapshot,
    build_config: &BuildConfig,
    round: u32,
    group_index: usize,
    fan_in: usize,
    root: &Path,
) -> Result<BuiltMerge> {
    let input_identities: Vec<_> = inputs.iter().map(run_input_identity).collect();
    let digest = merge_identity(&input_identities);
    let stem = format!("merge-{round:04}-{group_index:08}-{}", &digest[..16]);
    let completed_path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let run_identity = RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: build_config.as_of_epoch,
        product: "canonical-event-facts-merge".to_owned(),
        product_version: EVENT_FACTS_VERSION.to_owned(),
        key_space: "event-id-32-created-at-u64-kind-u16-be-v1".to_owned(),
    };
    if let Some(checkpoint) = load_reusable_checkpoint(
        &checkpoint_path,
        &completed_path,
        &run_identity,
        &input_identities,
    )? {
        return Ok(BuiltMerge {
            run: completed_run(stem, completed_path, checkpoint_path, checkpoint),
            peak_buffered_bytes: inputs
                .len()
                .checked_add(1)
                .and_then(|records| records.checked_mul(EVENT_FACT_BYTES))
                .ok_or_else(|| {
                    BoundedExecutionError::Invalid(
                        "merge buffer accounting overflowed usize".to_owned(),
                    )
                })?,
        });
    }
    let partial_path = unique_partial(&completed_path)?;
    let input_paths: Vec<_> = inputs.iter().map(|run| run.path.clone()).collect();
    let stats = merge_fixed_runs(
        &input_paths,
        &partial_path,
        FixedRecordLayout {
            record_bytes: EVENT_FACT_BYTES,
            key_bytes: EVENT_FACT_KEY_BYTES,
        },
        fan_in,
    )?;
    validate_merge_stats(inputs, stats)?;
    let min_key = inputs
        .iter()
        .filter_map(|run| run.checkpoint.artifact.min_key.as_ref())
        .min()
        .cloned();
    let max_key = inputs
        .iter()
        .filter_map(|run| run.checkpoint.artifact.max_key.as_ref())
        .max()
        .cloned();
    let checkpoint = publish_run_checkpoint(
        &partial_path,
        &completed_path,
        &checkpoint_path,
        run_identity,
        input_identities,
        stats.output_records,
        min_key,
        max_key,
    )?;
    Ok(BuiltMerge {
        run: completed_run(stem, completed_path, checkpoint_path, checkpoint),
        peak_buffered_bytes: stats.peak_buffered_bytes,
    })
}

fn validate_merge_stats(inputs: &[CompletedRun], stats: MergeStats) -> Result<()> {
    let expected = checked_sum(
        inputs.iter().map(|run| run.checkpoint.artifact.row_count),
        "expected merge rows",
    )?;
    if stats.input_records != expected
        || checked_add(
            stats.output_records,
            stats.duplicate_records,
            "merge output accounting",
        )? != expected
    {
        return Err(BoundedExecutionError::Invalid(format!(
            "merge consumed {} records and produced {} plus {} duplicates, expected {expected}",
            stats.input_records, stats.output_records, stats.duplicate_records
        ))
        .into());
    }
    Ok(())
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

fn run_input_identity(run: &CompletedRun) -> InputIdentity {
    InputIdentity {
        identity: run.identity.clone(),
        byte_size: run.checkpoint.artifact.byte_size,
        row_count: run.checkpoint.artifact.row_count,
        sha256: run.checkpoint.artifact.sha256.clone(),
    }
}

fn merge_identity(inputs: &[InputIdentity]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-event-facts-merge-v1\0");
    for input in inputs {
        digest.update(input.identity.as_bytes());
        digest.update(input.sha256.as_bytes());
        digest.update(input.row_count.to_be_bytes());
    }
    hex::encode(digest.finalize())
}

fn finalize_rollups(
    facts_path: &Path,
    artifact: &ArtifactIdentity,
    as_of_epoch: u64,
) -> Result<FinalizedRollups> {
    let mut reader = EventFactReader::open(facts_path)?;
    let mut daily = BTreeMap::<u32, u64>::new();
    let mut daily_kind = BTreeMap::<(u32, u16), u64>::new();
    let mut kind_counts = vec![0_u64; KIND_DOMAIN];
    let mut rolling_kinds = vec![0_u64; KIND_WORDS];
    let seven_day_start = as_of_epoch.saturating_sub(SEVEN_DAYS_SECS);
    let thirty_day_start = as_of_epoch.saturating_sub(THIRTY_DAYS_SECS);
    let mut logical_events = 0_u64;
    let mut api_representable_events = 0_u64;
    let mut earliest_representable: Option<u64> = None;
    let mut latest_event: Option<u64> = None;
    let mut events_7d = 0_u64;
    let mut previous_id = None;
    while let Some(fact) = reader.read_next()? {
        if previous_id.is_some_and(|previous| fact.id <= previous) {
            return Err(BoundedExecutionError::Invalid(
                "completed event facts are not strictly sorted and unique".to_owned(),
            )
            .into());
        }
        previous_id = Some(fact.id);
        logical_events = checked_add(logical_events, 1, "logical events")?;
        let kind_index = usize::from(fact.kind);
        kind_counts[kind_index] = checked_add(kind_counts[kind_index], 1, "kind count")?;
        if fact.created_at <= API_TIMESTAMP_MAX {
            api_representable_events =
                checked_add(api_representable_events, 1, "API-representable event count")?;
            earliest_representable = Some(
                earliest_representable.map_or(fact.created_at, |value| value.min(fact.created_at)),
            );
            let day = u32::try_from(fact.created_at / 86_400).map_err(|_| {
                BoundedExecutionError::Invalid("representable UTC day exceeds u32".to_owned())
            })?;
            increment_map(&mut daily, day, "daily event count")?;
            increment_map(&mut daily_kind, (day, fact.kind), "daily-kind event count")?;
        }
        if fact.created_at <= as_of_epoch {
            latest_event =
                Some(latest_event.map_or(fact.created_at, |value| value.max(fact.created_at)));
        }
        if fact.created_at >= seven_day_start && fact.created_at <= as_of_epoch {
            events_7d = checked_add(events_7d, 1, "seven-day event count")?;
        }
        if fact.created_at >= thirty_day_start && fact.created_at <= as_of_epoch {
            let word = kind_index / 64;
            let bit = kind_index % 64;
            rolling_kinds[word] |= 1_u64 << bit;
        }
    }
    if logical_events != artifact.row_count {
        return Err(BoundedExecutionError::Invalid(format!(
            "read {logical_events} event facts, checkpoint records {}",
            artifact.row_count
        ))
        .into());
    }
    let actual_bytes = facts_path.metadata()?.len();
    let expected_bytes = logical_events
        .checked_mul(u64::try_from(EVENT_FACT_BYTES).expect("event fact width fits u64"))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("event-fact byte accounting overflowed u64".to_owned())
        })?;
    if actual_bytes != expected_bytes || actual_bytes != artifact.byte_size {
        return Err(BoundedExecutionError::Invalid(format!(
            "event facts have {actual_bytes} bytes, expected {expected_bytes}"
        ))
        .into());
    }

    let event_daily = daily
        .into_iter()
        .map(|(day, event_count)| {
            Ok(EventDaily {
                day: utc_day(day)?,
                event_count,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let event_daily_kind = daily_kind
        .into_iter()
        .map(|((day, kind), event_count)| {
            Ok(EventDailyKind {
                day: utc_day(day)?,
                kind,
                event_count,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let kind_all_time = kind_counts
        .into_iter()
        .enumerate()
        .filter(|(_, event_count)| *event_count != 0)
        .map(|(kind, event_count)| {
            Ok(KindAllTime {
                kind: u16::try_from(kind).expect("kind counter domain is u16"),
                event_count,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let kinds_30d = rolling_kinds
        .into_iter()
        .map(|word| u64::from(word.count_ones()))
        .sum();
    let earliest_event = earliest_representable
        .unwrap_or(0)
        .max(u64::from(pensieve_core::NOSTR_GENESIS_TIMESTAMP));
    let overview = Overview {
        total_events: logical_events,
        api_representable_events,
        earliest_event: u32::try_from(earliest_event)
            .map_err(|_| BoundedExecutionError::Invalid("earliest event exceeds u32".to_owned()))?,
        latest_event: u32::try_from(latest_event.unwrap_or(0))
            .map_err(|_| BoundedExecutionError::Invalid("latest event exceeds u32".to_owned()))?,
        events_7d,
        events_per_hour_7d: events_7d as f64 / HOURS_PER_SEVEN_DAYS,
        kinds_30d,
    };
    let daily_keys = event_daily.len();
    let daily_kind_keys = event_daily_kind.len();
    Ok(FinalizedRollups {
        rollups: CompactRollups {
            overview,
            event_daily,
            event_daily_kind,
            kind_all_time,
            logical_events,
            facts_path: facts_path.to_string_lossy().into_owned(),
            facts_bytes: actual_bytes,
            facts_sha256: artifact.sha256.clone(),
        },
        daily_keys,
        daily_kind_keys,
    })
}

fn increment_map<K: Ord>(map: &mut BTreeMap<K, u64>, key: K, label: &str) -> Result<()> {
    let value = map.entry(key).or_default();
    *value = checked_add(*value, 1, label)?;
    Ok(())
}

fn utc_day(day: u32) -> Result<String> {
    let seconds = i64::from(day)
        .checked_mul(86_400)
        .ok_or_else(|| BoundedExecutionError::Invalid("UTC day overflowed i64".to_owned()))?;
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .map(|value| value.date_naive().to_string())
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("UTC day is not representable".to_owned()).into()
        })
}

fn unique_partial(completed_path: &Path) -> Result<PathBuf> {
    let file_name = completed_path.file_name().ok_or_else(|| {
        BoundedExecutionError::Invalid("completed run path must have a file name".to_owned())
    })?;
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    Ok(completed_path.with_file_name(format!(
        ".{}.{}.{}.partial",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    )))
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")).into())
}

fn checked_sum(values: impl IntoIterator<Item = u64>, label: &str) -> Result<u64> {
    values
        .into_iter()
        .try_fold(0_u64, |total, value| checked_add(total, value, label))
}

fn to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BoundedExecutionError::Invalid(format!("{label} exceeds u64")).into())
}

fn estimate_run_bytes(physical_rows: u64, batch_count: usize, fan_in: usize) -> Result<u64> {
    let fact_bytes = physical_rows
        .checked_mul(u64::try_from(EVENT_FACT_BYTES).expect("event fact width fits u64"))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("event-fact run estimate overflowed u64".to_owned())
        })?;
    let mut rounds = 0_u64;
    let mut runs = batch_count.max(1);
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        rounds = checked_add(rounds, 1, "merge round estimate")?;
    }
    fact_bytes
        .checked_mul(checked_add(rounds, 1, "run generation count")?)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("total run byte estimate overflowed u64".to_owned())
                .into()
        })
}

fn completed_run_bytes(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "run")
            {
                total = checked_add(total, entry.metadata()?.len(), "retained work bytes")?;
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
fn write_sorted_event_facts(
    facts: impl IntoIterator<Item = Result<EventFact>>,
    writer: &mut impl Write,
) -> Result<EventFactBatchStats> {
    let mut reducer = SortedEventFactWriter::new(writer);
    for fact in facts {
        reducer.push(fact?)?;
    }
    reducer.finish()
}

pub(crate) fn scan_batch_event_facts(
    connection: &Connection,
    locations: &[ObjectLocation],
    writer: &mut impl Write,
) -> Result<EventFactBatchStats> {
    if locations.is_empty() {
        return Err(BoundedExecutionError::Invalid(
            "an event-fact batch must include at least one Parquet object".to_owned(),
        )
        .into());
    }
    let paths = locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "
        SELECT id, created_at, kind
        FROM read_parquet([{paths}], union_by_name = false)
        ORDER BY id, created_at, kind
        "
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut reducer = SortedEventFactWriter::new(writer);
    while let Some(row) = rows.next()? {
        let id: Vec<u8> = row.get(0)?;
        let id: [u8; EVENT_FACT_KEY_BYTES] = id.try_into().map_err(|id: Vec<u8>| {
            BoundedExecutionError::Invalid(format!(
                "Parquet event ID has {} bytes, expected {EVENT_FACT_KEY_BYTES}",
                id.len()
            ))
        })?;
        reducer.push(EventFact {
            id,
            created_at: row.get(1)?,
            kind: row.get(2)?,
        })?;
    }
    reducer.finish()
}

pub(crate) fn verify_local_batch_inputs(
    expected: &[crate::InputIdentity],
    locations: &[ObjectLocation],
) -> Result<()> {
    if expected.len() != locations.len() {
        return Err(BoundedExecutionError::Invalid(format!(
            "batch has {} identities but {} locations",
            expected.len(),
            locations.len()
        ))
        .into());
    }
    for (input, location) in expected.iter().zip(locations) {
        let ObjectLocation::Local(path) = location else {
            continue;
        };
        verify_local_input(input, path)?;
    }
    Ok(())
}

fn verify_local_input(input: &crate::InputIdentity, path: &PathBuf) -> Result<()> {
    let actual_bytes = path.metadata()?.len();
    if actual_bytes != input.byte_size {
        return Err(BoundedExecutionError::Invalid(format!(
            "local input {} has {actual_bytes} bytes, expected {}",
            path.display(),
            input.byte_size
        ))
        .into());
    }
    let actual_sha256 = pensieve_lake::sha256_file(path)?;
    if actual_sha256 != input.sha256 {
        return Err(BoundedExecutionError::Invalid(format!(
            "local input {} has SHA-256 {actual_sha256}, expected {}",
            path.display(),
            input.sha256
        ))
        .into());
    }
    Ok(())
}

struct SortedEventFactWriter<'a, W> {
    writer: &'a mut W,
    physical_rows: u64,
    logical_events: u64,
    previous: Option<EventFact>,
    min_event_id: Option<String>,
    max_event_id: Option<String>,
}

impl<'a, W: Write> SortedEventFactWriter<'a, W> {
    fn new(writer: &'a mut W) -> Self {
        Self {
            writer,
            physical_rows: 0,
            logical_events: 0,
            previous: None,
            min_event_id: None,
            max_event_id: None,
        }
    }

    fn push(&mut self, fact: EventFact) -> Result<()> {
        self.physical_rows = checked_increment(self.physical_rows, "physical event-fact rows")?;
        if let Some(previous) = self.previous {
            if fact.id < previous.id {
                return Err(BoundedExecutionError::Invalid(
                    "event-fact input is not sorted by event ID".to_owned(),
                )
                .into());
            }
            if fact.id == previous.id {
                if fact != previous {
                    return Err(BoundedExecutionError::Invalid(format!(
                        "event ID {} has conflicting committed timestamp or kind",
                        fact.id_hex()
                    ))
                    .into());
                }
                return Ok(());
            }
        }
        self.writer.write_all(&fact.encode())?;
        self.logical_events = checked_increment(self.logical_events, "logical event facts")?;
        let id_hex = fact.id_hex();
        self.min_event_id.get_or_insert_with(|| id_hex.clone());
        self.max_event_id = Some(id_hex);
        self.previous = Some(fact);
        Ok(())
    }

    fn finish(self) -> Result<EventFactBatchStats> {
        let duplicate_rows = self
            .physical_rows
            .checked_sub(self.logical_events)
            .ok_or_else(|| {
                BoundedExecutionError::Invalid(
                    "event-fact duplicate accounting underflowed".to_owned(),
                )
            })?;
        Ok(EventFactBatchStats {
            physical_rows: self.physical_rows,
            logical_events: self.logical_events,
            duplicate_rows,
            min_event_id: self.min_event_id,
            max_event_id: self.max_event_id,
        })
    }
}

fn checked_increment(value: u64, label: &str) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: u8, created_at: u64, kind: u16) -> EventFact {
        EventFact {
            id: [id; EVENT_FACT_KEY_BYTES],
            created_at,
            kind,
        }
    }

    #[test]
    fn encoding_is_fixed_width_ordered_and_round_trips() {
        let first = fact(1, u64::MAX, u16::MAX);
        let second = fact(2, 0, 0);
        assert_eq!(first.encode().len(), EVENT_FACT_BYTES);
        assert!(first.encode() < second.encode());
        assert_eq!(EventFact::decode(first.encode()), first);
    }

    #[test]
    fn sorted_writer_suppresses_exact_duplicates_with_exact_accounting() {
        let mut bytes = Vec::new();
        let stats = write_sorted_event_facts(
            [Ok(fact(1, 10, 1)), Ok(fact(1, 10, 1)), Ok(fact(2, 20, 2))],
            &mut bytes,
        )
        .expect("write facts");
        assert_eq!(stats.physical_rows, 3);
        assert_eq!(stats.logical_events, 2);
        assert_eq!(stats.duplicate_rows, 1);
        assert_eq!(bytes.len(), 2 * EVENT_FACT_BYTES);
        assert_eq!(stats.min_event_id, Some("01".repeat(32)));
        assert_eq!(stats.max_event_id, Some("02".repeat(32)));
    }

    #[test]
    fn sorted_writer_rejects_conflicts_and_order_regressions() {
        let mut conflict = Vec::new();
        assert!(
            write_sorted_event_facts([Ok(fact(1, 10, 1)), Ok(fact(1, 11, 1))], &mut conflict,)
                .is_err()
        );
        let mut unsorted = Vec::new();
        assert!(
            write_sorted_event_facts([Ok(fact(2, 10, 1)), Ok(fact(1, 10, 1))], &mut unsorted,)
                .is_err()
        );
    }

    #[test]
    fn reader_rejects_truncated_terminal_record() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("facts.run");
        std::fs::write(&path, &fact(1, 10, 1).encode()[..EVENT_FACT_BYTES - 1])
            .expect("write truncated run");
        let mut reader = EventFactReader::open(path).expect("reader");
        assert!(reader.read_next().is_err());
    }

    #[test]
    fn rollup_state_plateaus_across_hundredfold_event_growth() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut observed = Vec::new();
        for cardinality in [100_u64, 1_000, 10_000] {
            let path = root.path().join(format!("facts-{cardinality}.run"));
            let mut file = BufWriter::new(File::create(&path).expect("facts file"));
            for value in 0..cardinality {
                let mut id = [0_u8; EVENT_FACT_KEY_BYTES];
                id[EVENT_FACT_KEY_BYTES - 8..].copy_from_slice(&value.to_be_bytes());
                file.write_all(
                    &EventFact {
                        id,
                        created_at: 1_700_000_000,
                        kind: 1,
                    }
                    .encode(),
                )
                .expect("fact");
            }
            file.flush().expect("flush");
            let bytes = path.metadata().expect("metadata").len();
            let artifact = ArtifactIdentity {
                path: path.to_string_lossy().into_owned(),
                byte_size: bytes,
                row_count: cardinality,
                min_key: Some(format!("{:064x}", 0)),
                max_key: Some(format!("{:064x}", cardinality - 1)),
                sha256: "0".repeat(64),
            };
            let finalized =
                finalize_rollups(&path, &artifact, 1_700_000_000).expect("finalize rollups");
            assert_eq!(finalized.rollups.logical_events, cardinality);
            observed.push((finalized.daily_keys, finalized.daily_kind_keys));
        }
        assert_eq!(observed, vec![(1, 1), (1, 1), (1, 1)]);
    }
}
