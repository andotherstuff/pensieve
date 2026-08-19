//! Bounded exact first-seen state for eligible Nostr pubkeys.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use duckdb::Connection;
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{API_TIMESTAMP_MAX, configure_execution, configure_remote_access, sql_string};
use crate::event_facts::verify_local_batch_inputs;
use crate::{
    ArtifactIdentity, BOUNDED_CHECKPOINT_SCHEMA_VERSION, BOUNDED_RUNNER_VERSION, BatchLimits,
    BoundedExecutionError, BuildConfig, CatalogDeltaPlan, DiskBudget, InputBatch, InputIdentity,
    MergeStats, ObjectLocation, PlannedRunKind, ResolvedSnapshot, Result, RunCheckpoint,
    RunIdentity, load_reusable_checkpoint, merge_fixed_min_u64_runs, plan_input_batches,
    preflight_disk, publish_canonical_json, publish_run_checkpoint,
};

/// Raw pubkey bytes in every first-seen record.
pub const PUBKEY_FIRST_SEEN_KEY_BYTES: usize = 32;
/// Fixed pubkey plus big-endian first-seen timestamp.
pub const PUBKEY_FIRST_SEEN_BYTES: usize = PUBKEY_FIRST_SEEN_KEY_BYTES + 8;
/// Semantic product version.
pub const PUBKEY_FIRST_SEEN_VERSION: &str = "pubkey-first-seen-v1";
const RUNNER_VERSION: &str = "pensieve-analytics-pubkey-first-seen-v1";
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource settings for the bounded first-seen lane.
#[derive(Clone, Debug)]
pub struct PubkeyFirstSeenConfig {
    /// Dedicated immutable run root.
    pub work_root: PathBuf,
    /// Catalog byte and row ceilings for each DuckDB scan.
    pub batch_limits: BatchLimits,
    /// Maximum runs opened by one streaming merge.
    pub merge_fan_in: usize,
    /// Free bytes left untouched on the work filesystem.
    pub disk_reserve_bytes: u64,
}

/// One finalized daily new-user row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NewUsersDaily {
    /// ISO-8601 UTC date.
    pub day: String,
    /// Eligible pubkeys first seen on this date.
    pub new_pubkeys: u64,
}

/// Immutable completion evidence for one first-seen build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PubkeyFirstSeenEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner identity.
    pub runner_version: String,
    /// Completion status.
    pub status: String,
    /// Frozen catalog identity.
    pub snapshot_id: String,
    /// Fixed upper time boundary.
    pub as_of_epoch: u64,
    /// Catalog objects scanned.
    pub object_count: u64,
    /// Immutable batch count.
    pub batch_count: u64,
    /// Immutable merge count.
    pub merge_count: u64,
    /// Catalog rows covered by the scan.
    pub physical_rows: u64,
    /// Prior immutable evidence consumed by an incremental successor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_evidence_sha256: Option<String>,
    /// New catalog objects scanned for this generation.
    pub delta_object_count: u64,
    /// Unique pubkeys across all eligible event kinds, before date filtering.
    pub first_seen_records: u64,
    /// Pubkeys whose minimum timestamp is within the API date domain.
    pub eligible_pubkeys: u64,
    /// Daily serving rows.
    pub new_users_daily: Vec<NewUsersDaily>,
    /// SHA-256 of canonical daily serving bytes.
    pub metric_sha256: String,
    /// Final high-cardinality state artifact.
    pub final_artifact: ArtifactIdentity,
    /// Maximum encoded merge buffer.
    pub max_merge_buffered_bytes: usize,
    /// Conservative run-artifact estimate.
    pub estimated_run_bytes: u64,
    /// Operator-selected disk reserve.
    pub disk_reserve_bytes: u64,
    /// Immutable batch checkpoints.
    pub batch_checkpoints: Vec<String>,
    /// Immutable merge checkpoints.
    pub merge_checkpoints: Vec<String>,
}

/// Completed first-seen products.
#[derive(Clone, Debug)]
pub struct BoundedPubkeyFirstSeen {
    /// Validated completion evidence.
    pub evidence: PubkeyFirstSeenEvidence,
    /// SHA-256 of canonical evidence JSON.
    pub evidence_sha256: String,
}

impl BoundedPubkeyFirstSeen {
    /// Revalidate immutable identity products immediately before publication.
    ///
    /// Publication deliberately re-reads the high-cardinality artifact instead
    /// of trusting an in-memory completion value. This keeps a transient
    /// Postgres failure safely retryable and makes artifact replacement or
    /// truncation fail before the current-run pointer can move.
    pub fn validate_for_publication(&self, snapshot_id: &str, as_of_epoch: u64) -> Result<()> {
        let evidence = &self.evidence;
        if evidence.schema_version != 1
            || evidence.runner_version != RUNNER_VERSION
            || evidence.status != "completed"
        {
            return Err(BoundedExecutionError::Invalid(
                "first-seen evidence is not a completed supported product".to_owned(),
            )
            .into());
        }
        if evidence.snapshot_id != snapshot_id || evidence.as_of_epoch != as_of_epoch {
            return Err(BoundedExecutionError::Invalid(format!(
                "first-seen evidence targets {}/{} instead of {snapshot_id}/{as_of_epoch}",
                evidence.snapshot_id, evidence.as_of_epoch
            ))
            .into());
        }
        let artifact_path = Path::new(&evidence.final_artifact.path);
        let metadata = artifact_path.metadata()?;
        if !metadata.is_file() || metadata.len() != evidence.final_artifact.byte_size {
            return Err(BoundedExecutionError::Invalid(
                "first-seen artifact byte accounting mismatch".to_owned(),
            )
            .into());
        }
        let artifact_sha256 = pensieve_lake::sha256_file(artifact_path)?;
        if artifact_sha256 != evidence.final_artifact.sha256 {
            return Err(BoundedExecutionError::Invalid(format!(
                "first-seen artifact SHA-256 {artifact_sha256} does not match evidence {}",
                evidence.final_artifact.sha256
            ))
            .into());
        }
        let (eligible_pubkeys, new_users_daily) = finalize(
            artifact_path,
            &evidence.final_artifact,
            evidence.as_of_epoch,
        )?;
        if eligible_pubkeys != evidence.eligible_pubkeys
            || new_users_daily != evidence.new_users_daily
        {
            return Err(BoundedExecutionError::Invalid(
                "first-seen serving metrics do not match the immutable artifact".to_owned(),
            )
            .into());
        }
        let mut metric_bytes = serde_json::to_vec_pretty(&new_users_daily).map_err(|error| {
            BoundedExecutionError::Invalid(format!("serialize new users: {error}"))
        })?;
        metric_bytes.push(b'\n');
        let metric_sha256 = hex::encode(Sha256::digest(&metric_bytes));
        if metric_sha256 != evidence.metric_sha256 {
            return Err(BoundedExecutionError::Invalid(format!(
                "first-seen metric SHA-256 {metric_sha256} does not match evidence {}",
                evidence.metric_sha256
            ))
            .into());
        }
        Ok(())
    }
}

/// Load and fully revalidate a completed first-seen evidence file.
pub fn load_bounded_pubkey_first_seen(
    evidence_path: impl AsRef<Path>,
) -> Result<BoundedPubkeyFirstSeen> {
    let evidence_path = evidence_path.as_ref();
    let evidence: PubkeyFirstSeenEvidence = serde_json::from_slice(&fs::read(evidence_path)?)
        .map_err(|error| {
            BoundedExecutionError::Invalid(format!("decode first-seen evidence: {error}"))
        })?;
    let completed = BoundedPubkeyFirstSeen {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    };
    completed.validate_for_publication(
        &completed.evidence.snapshot_id,
        completed.evidence.as_of_epoch,
    )?;
    Ok(completed)
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

/// Build exact first-seen state and daily new-user products from one snapshot.
pub fn build_bounded_pubkey_first_seen(
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: PubkeyFirstSeenConfig,
) -> Result<BoundedPubkeyFirstSeen> {
    validate_config(&snapshot, &build, &config)?;
    fs::create_dir_all(&config.work_root)?;
    let inputs = catalog_inputs(&snapshot)?;
    let batches = plan_input_batches(&inputs, config.batch_limits)?;
    let estimated_run_bytes = estimate_run_bytes(
        snapshot.catalog.totals().physical_rows,
        batches.len(),
        config.merge_fan_in,
    )?;
    preflight_disk(
        &config.work_root,
        DiskBudget {
            output_bytes: estimated_run_bytes
                .saturating_sub(completed_run_bytes(&config.work_root)?),
            temporary_bytes: 0,
            retained_bytes: completed_run_bytes(&config.work_root)?,
            reserve_bytes: config.disk_reserve_bytes,
        },
    )?;
    let connection = Connection::open_in_memory()?;
    configure_execution(&connection, &build)?;
    connection.execute_batch("SET TimeZone='UTC'; SET preserve_insertion_order=false")?;
    configure_remote_access(&connection, &snapshot, &build)?;

    let batch_root = config.work_root.join("batches");
    fs::create_dir_all(&batch_root)?;
    let mut runs = Vec::with_capacity(batches.len().max(1));
    let mut checkpoints = Vec::with_capacity(batches.len());
    let mut offset = 0;
    for batch in &batches {
        let end = offset + batch.inputs.len();
        let locations = snapshot.locations.get(offset..end).ok_or_else(|| {
            BoundedExecutionError::Invalid("batch locations do not cover frozen inputs".to_owned())
        })?;
        let run = build_batch(
            &connection,
            &snapshot,
            &build,
            batch,
            locations,
            &batch_root,
        )?;
        checkpoints.push(run.checkpoint_path.to_string_lossy().into_owned());
        runs.push(run);
        offset = end;
    }
    if offset != snapshot.locations.len() {
        return Err(
            BoundedExecutionError::Invalid("unconsumed snapshot locations".to_owned()).into(),
        );
    }
    if runs.is_empty() {
        runs.push(build_empty(&snapshot, &build, &config.work_root)?);
    }
    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_all(runs, &snapshot, &build, config.merge_fan_in, &merge_root)?;
    let (eligible_pubkeys, new_users_daily) = finalize(
        &merged.final_run.path,
        &merged.final_run.checkpoint.artifact,
        build.as_of_epoch,
    )?;
    let mut metric_bytes = serde_json::to_vec_pretty(&new_users_daily)
        .map_err(|error| BoundedExecutionError::Invalid(format!("serialize new users: {error}")))?;
    metric_bytes.push(b'\n');
    let metric_sha256 = hex::encode(Sha256::digest(&metric_bytes));
    let evidence = PubkeyFirstSeenEvidence {
        schema_version: 1,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build.as_of_epoch,
        object_count: to_u64(snapshot.catalog.objects().len())?,
        batch_count: to_u64(batches.len())?,
        merge_count: merged.merge_count,
        physical_rows: snapshot.catalog.totals().physical_rows,
        baseline_evidence_sha256: None,
        delta_object_count: to_u64(snapshot.catalog.objects().len())?,
        first_seen_records: merged.final_run.checkpoint.artifact.row_count,
        eligible_pubkeys,
        new_users_daily,
        metric_sha256,
        final_artifact: merged.final_run.checkpoint.artifact,
        max_merge_buffered_bytes: merged.max_buffered_bytes,
        estimated_run_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
        batch_checkpoints: checkpoints,
        merge_checkpoints: merged.checkpoints,
    };
    let evidence_path = evidence_path.as_ref();
    publish_canonical_json(evidence_path, &evidence)?;
    Ok(BoundedPubkeyFirstSeen {
        evidence,
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
    })
}

/// Advance exact first-seen state from one verified append-only catalog delta.
///
/// Only newly added Parquet objects are scanned. Their level-zero runs are
/// streaming-min merged with the prior immutable state, so a late historical
/// event can move exactly one pubkey from a newer daily bucket to an older one
/// without loading cardinality-sized state into memory.
pub fn advance_bounded_pubkey_first_seen(
    evidence_path: impl AsRef<Path>,
    baseline: &BoundedPubkeyFirstSeen,
    target: ResolvedSnapshot,
    plan: &CatalogDeltaPlan,
    delta_locations: &[ObjectLocation],
    build: BuildConfig,
    config: PubkeyFirstSeenConfig,
) -> Result<BoundedPubkeyFirstSeen> {
    baseline.validate_for_publication(
        &baseline.evidence.snapshot_id,
        baseline.evidence.as_of_epoch,
    )?;
    if build.as_of_epoch > API_TIMESTAMP_MAX
        || config.merge_fan_in < 2
        || plan.run_kind != PlannedRunKind::Incremental
        || plan.snapshot_id != target.catalog.snapshot_id
        || plan.previous_snapshot_id.as_deref() != Some(&baseline.evidence.snapshot_id)
        || !plan.removed_objects.is_empty()
        || plan.added_objects.len() != delta_locations.len()
    {
        return Err(BoundedExecutionError::Invalid(
            "invalid incremental first-seen build configuration".to_owned(),
        )
        .into());
    }
    fs::create_dir_all(&config.work_root)?;
    let inputs = plan
        .added_objects
        .iter()
        .map(|object| InputIdentity {
            identity: object.object_key.clone(),
            byte_size: object.byte_size,
            row_count: object.row_count,
            sha256: object.sha256.clone(),
        })
        .collect::<Vec<_>>();
    let batches = plan_input_batches(&inputs, config.batch_limits)?;
    let estimated_rows = baseline
        .evidence
        .first_seen_records
        .checked_add(plan.added_physical_rows)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("first-seen row estimate overflow".to_owned())
        })?;
    let estimated_run_bytes = estimate_run_bytes(
        estimated_rows,
        batches.len().saturating_add(1),
        config.merge_fan_in,
    )?;
    let retained_bytes = completed_run_bytes(&config.work_root)?;
    preflight_disk(
        &config.work_root,
        DiskBudget {
            output_bytes: estimated_run_bytes.saturating_sub(retained_bytes),
            temporary_bytes: 0,
            retained_bytes,
            reserve_bytes: config.disk_reserve_bytes,
        },
    )?;

    let connection = Connection::open_in_memory()?;
    configure_execution(&connection, &build)?;
    connection.execute_batch("SET TimeZone='UTC'; SET preserve_insertion_order=false")?;
    configure_remote_access(&connection, &target, &build)?;
    let batch_root = config.work_root.join("batches");
    fs::create_dir_all(&batch_root)?;
    let baseline_artifact = baseline.evidence.final_artifact.clone();
    let baseline_path = PathBuf::from(&baseline_artifact.path);
    let baseline_run = CompletedRun {
        identity: format!("baseline-evidence:{}", baseline.evidence_sha256),
        path: baseline_path,
        checkpoint_path: PathBuf::new(),
        checkpoint: RunCheckpoint {
            schema_version: BOUNDED_CHECKPOINT_SCHEMA_VERSION,
            runner_version: BOUNDED_RUNNER_VERSION.to_owned(),
            run: run_identity(&target, &build, "baseline"),
            inputs: Vec::new(),
            artifact: baseline_artifact,
        },
    };
    let mut runs = vec![baseline_run];
    let mut checkpoints = Vec::with_capacity(batches.len());
    let mut offset = 0;
    for batch in &batches {
        let end = offset + batch.inputs.len();
        let locations = delta_locations.get(offset..end).ok_or_else(|| {
            BoundedExecutionError::Invalid("delta locations do not cover batches".to_owned())
        })?;
        let run = build_batch(&connection, &target, &build, batch, locations, &batch_root)?;
        checkpoints.push(run.checkpoint_path.to_string_lossy().into_owned());
        runs.push(run);
        offset = end;
    }
    if offset != delta_locations.len() {
        return Err(BoundedExecutionError::Invalid(
            "unconsumed incremental first-seen locations".to_owned(),
        )
        .into());
    }
    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_all(runs, &target, &build, config.merge_fan_in, &merge_root)?;
    let (eligible_pubkeys, new_users_daily) = finalize(
        &merged.final_run.path,
        &merged.final_run.checkpoint.artifact,
        build.as_of_epoch,
    )?;
    let mut metric_bytes = serde_json::to_vec_pretty(&new_users_daily)
        .map_err(|error| BoundedExecutionError::Invalid(format!("serialize new users: {error}")))?;
    metric_bytes.push(b'\n');
    let evidence = PubkeyFirstSeenEvidence {
        schema_version: 1,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: target.catalog.snapshot_id.clone(),
        as_of_epoch: build.as_of_epoch,
        object_count: to_u64(target.catalog.objects().len())?,
        batch_count: to_u64(batches.len())?,
        merge_count: merged.merge_count,
        physical_rows: target.catalog.totals().physical_rows,
        baseline_evidence_sha256: Some(baseline.evidence_sha256.clone()),
        delta_object_count: to_u64(plan.added_objects.len())?,
        first_seen_records: merged.final_run.checkpoint.artifact.row_count,
        eligible_pubkeys,
        new_users_daily,
        metric_sha256: hex::encode(Sha256::digest(&metric_bytes)),
        final_artifact: merged.final_run.checkpoint.artifact,
        max_merge_buffered_bytes: merged.max_buffered_bytes,
        estimated_run_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
        batch_checkpoints: checkpoints,
        merge_checkpoints: merged.checkpoints,
    };
    let evidence_path = evidence_path.as_ref();
    publish_canonical_json(evidence_path, &evidence)?;
    Ok(BoundedPubkeyFirstSeen {
        evidence,
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
    })
}

fn build_batch(
    connection: &Connection,
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    batch: &InputBatch,
    locations: &[ObjectLocation],
    root: &Path,
) -> Result<CompletedRun> {
    let stem = format!("batch-{:08}", batch.index);
    let completed = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let identity = run_identity(snapshot, build, "batch");
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &batch.inputs)?
    {
        return Ok(completed_run(stem, completed, checkpoint_path, checkpoint));
    }
    verify_local_batch_inputs(&batch.inputs, locations)?;
    let partial = unique_partial(&completed)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    let (rows, min_key, max_key) = scan_batch(connection, locations, &mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let checkpoint = publish_run_checkpoint(
        &partial,
        &completed,
        &checkpoint_path,
        identity,
        batch.inputs.clone(),
        rows,
        min_key,
        max_key,
    )?;
    Ok(completed_run(stem, completed, checkpoint_path, checkpoint))
}

fn scan_batch(
    connection: &Connection,
    locations: &[ObjectLocation],
    writer: &mut impl Write,
) -> Result<(u64, Option<String>, Option<String>)> {
    let paths = locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT pubkey, min(created_at)::UBIGINT FROM read_parquet([{paths}], union_by_name=false) WHERE kind NOT IN (445,1059) GROUP BY pubkey ORDER BY pubkey"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut count = 0_u64;
    let mut previous: Option<[u8; 32]> = None;
    let mut min_key = None;
    let mut max_key = None;
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(0)?;
        let pubkey: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            BoundedExecutionError::Invalid(format!("Parquet pubkey has {} bytes", bytes.len()))
        })?;
        if previous.is_some_and(|value| value >= pubkey) {
            return Err(BoundedExecutionError::Invalid(
                "batch pubkeys are not strictly sorted".to_owned(),
            )
            .into());
        }
        let key = hex::encode(pubkey);
        min_key.get_or_insert_with(|| key.clone());
        max_key = Some(key);
        writer.write_all(&pubkey)?;
        writer.write_all(&row.get::<_, u64>(1)?.to_be_bytes())?;
        count = checked_add(count, 1, "batch pubkeys")?;
        previous = Some(pubkey);
    }
    Ok((count, min_key, max_key))
}

fn merge_all(
    mut runs: Vec<CompletedRun>,
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    fan_in: usize,
    root: &Path,
) -> Result<MergeOutcome> {
    let mut merge_count = 0;
    let mut max_buffered_bytes = 0;
    let mut checkpoints = Vec::new();
    let mut round = 0_u32;
    while runs.len() > 1 {
        let mut next = Vec::new();
        for (group_index, group) in runs.chunks(fan_in).enumerate() {
            if group.len() == 1 {
                next.push(group[0].clone());
                continue;
            }
            let input_ids: Vec<_> = group.iter().map(run_input).collect();
            let digest = merge_identity(&input_ids);
            let stem = format!("merge-{round:04}-{group_index:08}-{}", &digest[..16]);
            let completed = root.join(format!("{stem}.run"));
            let checkpoint_path = root.join(format!("{stem}.json"));
            let identity = run_identity(snapshot, build, "merge");
            let checkpoint = if let Some(checkpoint) =
                load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &input_ids)?
            {
                checkpoint
            } else {
                let partial = unique_partial(&completed)?;
                let paths = group.iter().map(|run| run.path.clone()).collect::<Vec<_>>();
                let stats = merge_fixed_min_u64_runs(&paths, &partial, 32, fan_in)?;
                validate_merge(group, stats)?;
                max_buffered_bytes = max_buffered_bytes.max(stats.peak_buffered_bytes);
                publish_run_checkpoint(
                    &partial,
                    &completed,
                    &checkpoint_path,
                    identity,
                    input_ids,
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
            max_buffered_bytes =
                max_buffered_bytes.max((group.len() + 1) * PUBKEY_FIRST_SEEN_BYTES);
            merge_count = checked_add(merge_count, 1, "merge count")?;
            checkpoints.push(checkpoint_path.to_string_lossy().into_owned());
            next.push(completed_run(stem, completed, checkpoint_path, checkpoint));
        }
        runs = next;
        round = round
            .checked_add(1)
            .ok_or_else(|| BoundedExecutionError::Invalid("merge round overflow".to_owned()))?;
    }
    Ok(MergeOutcome {
        final_run: runs.pop().expect("at least empty run"),
        merge_count,
        max_buffered_bytes,
        checkpoints,
    })
}

fn finalize(
    path: &Path,
    artifact: &ArtifactIdentity,
    as_of: u64,
) -> Result<(u64, Vec<NewUsersDaily>)> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut record = [0_u8; PUBKEY_FIRST_SEEN_BYTES];
    let mut previous: Option<[u8; 32]> = None;
    let mut rows = 0_u64;
    let mut eligible = 0_u64;
    let mut daily = BTreeMap::<u32, u64>::new();
    loop {
        let mut offset = 0;
        while offset < record.len() {
            let read = reader.read(&mut record[offset..])?;
            if read == 0 {
                break;
            }
            offset += read;
        }
        if offset == 0 {
            break;
        }
        if offset != record.len() {
            return Err(
                BoundedExecutionError::Invalid("truncated first-seen record".to_owned()).into(),
            );
        }
        let pubkey: [u8; 32] = record[..32].try_into().expect("fixed slice");
        if previous.is_some_and(|value| value >= pubkey) {
            return Err(BoundedExecutionError::Invalid(
                "first-seen state is not unique and sorted".to_owned(),
            )
            .into());
        }
        let first_seen = u64::from_be_bytes(record[32..].try_into().expect("fixed slice"));
        rows = checked_add(rows, 1, "first-seen rows")?;
        if first_seen >= u64::from(NOSTR_GENESIS_TIMESTAMP) && first_seen <= as_of {
            eligible = checked_add(eligible, 1, "eligible pubkeys")?;
            let day = u32::try_from(first_seen / 86_400).map_err(|_| {
                BoundedExecutionError::Invalid("first-seen day exceeds u32".to_owned())
            })?;
            *daily.entry(day).or_default() =
                checked_add(*daily.get(&day).unwrap_or(&0), 1, "daily new users")?;
        }
        previous = Some(pubkey);
    }
    if rows != artifact.row_count || path.metadata()?.len() != rows * PUBKEY_FIRST_SEEN_BYTES as u64
    {
        return Err(BoundedExecutionError::Invalid(
            "first-seen artifact accounting mismatch".to_owned(),
        )
        .into());
    }
    let new_users_daily = daily
        .into_iter()
        .map(|(day, new_pubkeys)| {
            let seconds = i64::from(day) * 86_400;
            let day = DateTime::<Utc>::from_timestamp(seconds, 0)
                .ok_or_else(|| BoundedExecutionError::Invalid("invalid UTC day".to_owned()))?
                .date_naive()
                .to_string();
            Ok(NewUsersDaily { day, new_pubkeys })
        })
        .collect::<Result<Vec<_>>>()?;
    let sum = new_users_daily.iter().try_fold(0_u64, |sum, row| {
        checked_add(sum, row.new_pubkeys, "daily sum")
    })?;
    if sum != eligible {
        return Err(BoundedExecutionError::Invalid(
            "daily new users do not sum to eligible pubkeys".to_owned(),
        )
        .into());
    }
    Ok((eligible, new_users_daily))
}

fn validate_merge(inputs: &[CompletedRun], stats: MergeStats) -> Result<()> {
    let expected = inputs.iter().try_fold(0_u64, |sum, run| {
        checked_add(sum, run.checkpoint.artifact.row_count, "merge inputs")
    })?;
    if stats.input_records != expected || stats.output_records + stats.duplicate_records != expected
    {
        return Err(BoundedExecutionError::Invalid(
            "first-seen merge accounting mismatch".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn validate_config(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    config: &PubkeyFirstSeenConfig,
) -> Result<()> {
    if build.as_of_epoch > API_TIMESTAMP_MAX
        || config.merge_fan_in < 2
        || snapshot.locations.len() != snapshot.catalog.objects().len()
    {
        return Err(BoundedExecutionError::Invalid(
            "invalid first-seen build configuration".to_owned(),
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
            Ok(InputIdentity {
                identity: object.object_key.clone(),
                byte_size: object.byte_size,
                row_count: object.row_count,
                sha256: object.sha256.clone(),
            })
        })
        .collect()
}

fn run_identity(snapshot: &ResolvedSnapshot, build: &BuildConfig, phase: &str) -> RunIdentity {
    RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: build.as_of_epoch,
        product: format!("pubkey-first-seen-{phase}"),
        product_version: PUBKEY_FIRST_SEEN_VERSION.to_owned(),
        key_space: "pubkey-32-first-seen-u64-be-v1".to_owned(),
    }
}

fn build_empty(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    root: &Path,
) -> Result<CompletedRun> {
    let completed = root.join("empty-snapshot.run");
    let checkpoint_path = root.join("empty-snapshot.json");
    let sha = snapshot
        .catalog
        .snapshot_id
        .strip_prefix("sha256:")
        .ok_or_else(|| BoundedExecutionError::Invalid("invalid snapshot ID".to_owned()))?
        .to_owned();
    let inputs = vec![InputIdentity {
        identity: format!("catalog: {}", snapshot.catalog.snapshot_id),
        byte_size: 0,
        row_count: 0,
        sha256: sha,
    }];
    let identity = run_identity(snapshot, build, "empty");
    let checkpoint = if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &inputs)?
    {
        checkpoint
    } else {
        let partial = unique_partial(&completed)?;
        File::create(&partial)?.sync_all()?;
        publish_run_checkpoint(
            &partial,
            &completed,
            &checkpoint_path,
            identity,
            inputs,
            0,
            None,
            None,
        )?
    };
    Ok(completed_run(
        "empty-snapshot".to_owned(),
        completed,
        checkpoint_path,
        checkpoint,
    ))
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
    let mut hash = Sha256::new();
    for input in inputs {
        hash.update(input.identity.as_bytes());
        hash.update(input.sha256.as_bytes());
    }
    hex::encode(hash.finalize())
}
fn unique_partial(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| BoundedExecutionError::Invalid("run path has no filename".to_owned()))?;
    Ok(path.with_file_name(format!(
        ".{}.{}.{}.partial",
        name.to_string_lossy(),
        std::process::id(),
        PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}
fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")).into())
}
fn to_u64(value: usize) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BoundedExecutionError::Invalid("count exceeds u64".to_owned()).into())
}
fn estimate_run_bytes(rows: u64, batches: usize, fan_in: usize) -> Result<u64> {
    let mut rounds = 0_u64;
    let mut runs = batches.max(1);
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        rounds = checked_add(rounds, 1, "merge rounds")?;
    }
    rows.checked_mul(PUBKEY_FIRST_SEEN_BYTES as u64)
        .and_then(|v| v.checked_mul(rounds + 1))
        .ok_or_else(|| BoundedExecutionError::Invalid("run estimate overflowed".to_owned()).into())
}
fn completed_run_bytes(root: &Path) -> Result<u64> {
    let mut total = 0;
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            } else if entry.path().extension().is_some_and(|v| v == "run") {
                total = checked_add(total, entry.metadata()?.len(), "retained bytes")?;
            }
        }
    }
    Ok(total)
}
