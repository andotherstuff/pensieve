//! Bounded exact fixed-grain pubkey activity products.
//!
//! The durable state is one sorted `(pubkey, timestamp, kind, event ID)` record.
//! Retaining the committed event ID suppresses duplicates across Parquet
//! objects before any count is finalized. Retaining the exact timestamp lets
//! append-only successors advance `as_of` without rescanning unchanged objects:
//! newly eligible records are filtered during finalization. Finalization
//! streams by pubkey and retains at most two 65,536-kind sets plus compact
//! time/key counters.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use duckdb::Connection;
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{API_TIMESTAMP_MAX, configure_execution, configure_remote_access, sql_string};
use crate::event_facts::verify_local_batch_inputs;
use crate::{
    ArtifactIdentity, BOUNDED_CHECKPOINT_SCHEMA_VERSION, BOUNDED_RUNNER_VERSION, BatchLimits,
    BoundedExecutionError, BuildConfig, CatalogDeltaPlan, DiskBudget, FixedRecordLayout,
    InputBatch, InputIdentity, MergeStats, ObjectLocation, PlannedRunKind, ResolvedSnapshot,
    Result, RunCheckpoint, RunIdentity, load_reusable_checkpoint, merge_fixed_runs,
    plan_input_batches, preflight_disk, publish_canonical_json, publish_run_checkpoint,
};

/// Pubkey, exact timestamp, kind, and committed event-ID bytes in each record.
pub const FIXED_ACTIVITY_KEY_BYTES: usize = 32 + 4 + 2 + 32;
/// Every activity record is entirely key material.
pub const FIXED_ACTIVITY_RECORD_BYTES: usize = FIXED_ACTIVITY_KEY_BYTES;
/// Pubkey plus exact ever-observed profile/follows flags.
pub const PUBKEY_FLAGS_RECORD_BYTES: usize = 33;
/// Semantic product version.
pub const FIXED_ACTIVITY_VERSION: &str = "fixed-activity-v2";
const RUNNER_VERSION: &str = "pensieve-analytics-fixed-activity-v2";
const PROFILE_FLAG: u8 = 1;
const FOLLOWS_FLAG: u8 = 2;
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource settings for the fixed-grain activity lane.
#[derive(Clone, Debug)]
pub struct FixedActivityConfig {
    /// Dedicated immutable run root.
    pub work_root: PathBuf,
    /// Catalog byte and row ceilings for each DuckDB scan.
    pub batch_limits: BatchLimits,
    /// Maximum runs opened by one streaming merge.
    pub merge_fan_in: usize,
    /// Free bytes left untouched on the work filesystem.
    pub disk_reserve_bytes: u64,
}

/// Exact distinct pubkeys for one fixed period, optionally scoped to a kind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DistinctPubkeysPeriod {
    /// `day`, `week`, or `month`.
    pub grain: String,
    /// ISO-8601 UTC period start.
    pub period_start: String,
    /// Event kind, or `None` for all kinds.
    pub kind: Option<u16>,
    /// Exact distinct pubkeys.
    pub unique_pubkeys: u64,
}

/// Exact active-user metrics for one fixed period.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveUsersPeriod {
    /// `day`, `week`, or `month`.
    pub grain: String,
    /// ISO-8601 UTC period start.
    pub period_start: String,
    /// Pubkeys with at least one event other than kinds 445 and 1059.
    pub active_users: u64,
    /// Active pubkeys with an ever-observed kind-0 event.
    pub has_profile: u64,
    /// Active pubkeys with an ever-observed kind-3 event.
    pub has_follows_list: u64,
    /// Active pubkeys with both flags.
    pub has_profile_and_follows_list: u64,
    /// Eligible events produced by active pubkeys.
    pub total_events: u64,
}

/// Immutable completion evidence for one fixed-grain activity build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FixedActivityEvidence {
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
    /// Catalog objects represented by the target snapshot.
    pub object_count: u64,
    /// New objects scanned for this generation.
    pub delta_object_count: u64,
    /// Catalog physical rows represented by the target snapshot.
    pub physical_rows: u64,
    /// Prior evidence consumed by an incremental successor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_evidence_sha256: Option<String>,
    /// Immutable batch count.
    pub batch_count: u64,
    /// Immutable merge count.
    pub merge_count: u64,
    /// Exact high-cardinality daily activity state.
    pub activity_artifact: ArtifactIdentity,
    /// Exact ever-observed per-pubkey flags.
    pub flags_artifact: ArtifactIdentity,
    /// Compact distinct serving rows.
    pub distinct_pubkeys: Vec<DistinctPubkeysPeriod>,
    /// Compact active-user serving rows.
    pub active_users: Vec<ActiveUsersPeriod>,
    /// SHA-256 of the canonical serving products.
    pub metric_sha256: String,
    /// Compact distinct serving row count.
    pub distinct_period_rows: u64,
    /// Compact active-user serving row count.
    pub active_period_rows: u64,
    /// Maximum kind identities buffered for one pubkey/week during finalization.
    pub max_week_kinds_buffered: usize,
    /// Maximum kind identities buffered for one pubkey/month during finalization.
    pub max_month_kinds_buffered: usize,
    /// Maximum encoded merge buffer.
    pub max_merge_buffered_bytes: usize,
    /// Conservative immutable-run estimate.
    pub estimated_run_bytes: u64,
    /// Operator-selected disk reserve.
    pub disk_reserve_bytes: u64,
    /// Immutable batch checkpoints.
    pub batch_checkpoints: Vec<String>,
    /// Immutable merge checkpoints.
    pub merge_checkpoints: Vec<String>,
}

/// Completed fixed-grain activity products.
#[derive(Clone, Debug)]
pub struct BoundedFixedActivity {
    /// Validated completion evidence.
    pub evidence: FixedActivityEvidence,
    /// SHA-256 of canonical evidence JSON.
    pub evidence_sha256: String,
}

impl BoundedFixedActivity {
    /// Revalidate immutable state and all compact serving rows.
    pub fn validate_for_publication(&self, snapshot_id: &str, as_of_epoch: u64) -> Result<()> {
        let evidence = &self.evidence;
        if evidence.schema_version != 1
            || evidence.runner_version != RUNNER_VERSION
            || evidence.status != "completed"
            || evidence.snapshot_id != snapshot_id
            || evidence.as_of_epoch != as_of_epoch
        {
            return Err(BoundedExecutionError::Invalid(
                "fixed-activity evidence is not a completed matching product".to_owned(),
            )
            .into());
        }
        validate_artifact(&evidence.activity_artifact, FIXED_ACTIVITY_RECORD_BYTES)?;
        validate_artifact(&evidence.flags_artifact, PUBKEY_FLAGS_RECORD_BYTES)?;
        let finalized = finalize(
            Path::new(&evidence.activity_artifact.path),
            Path::new(&evidence.flags_artifact.path),
            &evidence.activity_artifact,
            &evidence.flags_artifact,
            evidence.as_of_epoch,
        )?;
        if finalized.distinct_pubkeys != evidence.distinct_pubkeys
            || finalized.active_users != evidence.active_users
            || to_u64(finalized.distinct_pubkeys.len())? != evidence.distinct_period_rows
            || to_u64(finalized.active_users.len())? != evidence.active_period_rows
            || finalized.max_week_kinds_buffered != evidence.max_week_kinds_buffered
            || finalized.max_month_kinds_buffered != evidence.max_month_kinds_buffered
        {
            return Err(BoundedExecutionError::Invalid(
                "fixed-activity metrics do not match immutable state".to_owned(),
            )
            .into());
        }
        if metric_sha256(&finalized.distinct_pubkeys, &finalized.active_users)?
            != evidence.metric_sha256
        {
            return Err(BoundedExecutionError::Invalid(
                "fixed-activity metric SHA-256 mismatch".to_owned(),
            )
            .into());
        }
        Ok(())
    }
}

/// Load and fully revalidate completed fixed-grain activity evidence.
pub fn load_bounded_fixed_activity(path: impl AsRef<Path>) -> Result<BoundedFixedActivity> {
    let path = path.as_ref();
    let evidence: FixedActivityEvidence =
        serde_json::from_slice(&fs::read(path)?).map_err(|e| {
            BoundedExecutionError::Invalid(format!("decode fixed-activity evidence: {e}"))
        })?;
    let completed = BoundedFixedActivity {
        evidence_sha256: pensieve_lake::sha256_file(path)?,
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

/// Build exact fixed-grain activity state from one frozen snapshot.
pub fn build_bounded_fixed_activity(
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: FixedActivityConfig,
) -> Result<BoundedFixedActivity> {
    validate_config(&snapshot, &build, &config)?;
    build_from_inputs(
        evidence_path.as_ref(),
        snapshot.clone(),
        build,
        config,
        catalog_inputs(&snapshot)?,
        snapshot.locations.clone(),
        None,
        None,
    )
}

/// Advance exact fixed-grain activity state from an append-only catalog delta.
pub fn advance_bounded_fixed_activity(
    evidence_path: impl AsRef<Path>,
    baseline: &BoundedFixedActivity,
    target: ResolvedSnapshot,
    plan: &CatalogDeltaPlan,
    delta_locations: &[ObjectLocation],
    build: BuildConfig,
    config: FixedActivityConfig,
) -> Result<BoundedFixedActivity> {
    baseline.validate_for_publication(
        &baseline.evidence.snapshot_id,
        baseline.evidence.as_of_epoch,
    )?;
    if plan.run_kind != PlannedRunKind::Incremental
        || plan.snapshot_id != target.catalog.snapshot_id
        || plan.previous_snapshot_id.as_deref() != Some(&baseline.evidence.snapshot_id)
        || !plan.removed_objects.is_empty()
        || plan.added_objects.len() != delta_locations.len()
    {
        return Err(BoundedExecutionError::Invalid(
            "invalid incremental fixed-activity plan".to_owned(),
        )
        .into());
    }
    let inputs = plan
        .added_objects
        .iter()
        .map(|object| InputIdentity {
            identity: object.object_key.clone(),
            byte_size: object.byte_size,
            row_count: object.row_count,
            sha256: object.sha256.clone(),
        })
        .collect();
    build_from_inputs(
        evidence_path.as_ref(),
        target,
        build,
        config,
        inputs,
        delta_locations.to_vec(),
        Some(baseline),
        Some(baseline.evidence_sha256.clone()),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_from_inputs(
    evidence_path: &Path,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: FixedActivityConfig,
    inputs: Vec<InputIdentity>,
    locations: Vec<ObjectLocation>,
    baseline: Option<&BoundedFixedActivity>,
    baseline_evidence_sha256: Option<String>,
) -> Result<BoundedFixedActivity> {
    validate_config(&snapshot, &build, &config)?;
    fs::create_dir_all(&config.work_root)?;
    let batches = plan_input_batches(&inputs, config.batch_limits)?;
    let estimated_rows = baseline
        .map_or(0, |value| value.evidence.activity_artifact.row_count)
        .checked_add(inputs.iter().try_fold(0_u64, |sum, input| {
            sum.checked_add(input.row_count).ok_or_else(|| {
                BoundedExecutionError::Invalid("activity row estimate overflow".to_owned())
            })
        })?)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("activity row estimate overflow".to_owned())
        })?;
    let estimated_run_bytes = estimate_run_bytes(
        estimated_rows,
        batches.len() + usize::from(baseline.is_some()),
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

    let connection = Connection::open_in_memory()?;
    configure_execution(&connection, &build)?;
    connection.execute_batch("SET TimeZone='UTC'; SET preserve_insertion_order=false")?;
    configure_remote_access(&connection, &snapshot, &build)?;
    let batch_root = config.work_root.join("batches");
    fs::create_dir_all(&batch_root)?;
    let mut runs = Vec::with_capacity(batches.len() + usize::from(baseline.is_some()));
    if let Some(baseline) = baseline {
        runs.push(CompletedRun {
            identity: format!("baseline-evidence:{}", baseline.evidence_sha256),
            path: PathBuf::from(&baseline.evidence.activity_artifact.path),
            checkpoint_path: PathBuf::new(),
            checkpoint: RunCheckpoint {
                schema_version: BOUNDED_CHECKPOINT_SCHEMA_VERSION,
                runner_version: BOUNDED_RUNNER_VERSION.to_owned(),
                run: run_identity(&snapshot, &build, "baseline"),
                inputs: Vec::new(),
                artifact: baseline.evidence.activity_artifact.clone(),
            },
        });
    }
    let mut batch_checkpoints = Vec::with_capacity(batches.len());
    let mut offset = 0;
    for batch in &batches {
        let end = offset + batch.inputs.len();
        let batch_locations = locations.get(offset..end).ok_or_else(|| {
            BoundedExecutionError::Invalid("activity locations do not cover batches".to_owned())
        })?;
        let run = build_batch(
            &connection,
            &snapshot,
            &build,
            batch,
            batch_locations,
            &batch_root,
        )?;
        batch_checkpoints.push(run.checkpoint_path.to_string_lossy().into_owned());
        runs.push(run);
        offset = end;
    }
    if offset != locations.len() {
        return Err(BoundedExecutionError::Invalid(
            "unconsumed fixed-activity locations".to_owned(),
        )
        .into());
    }
    if runs.is_empty() {
        runs.push(build_empty(&snapshot, &build, &config.work_root)?);
    }
    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_all(runs, &snapshot, &build, config.merge_fan_in, &merge_root)?;
    let flags = build_flags(
        &snapshot,
        &build,
        &merged.final_run,
        &config.work_root.join("flags"),
    )?;
    let finalized = finalize(
        &merged.final_run.path,
        &flags.path,
        &merged.final_run.checkpoint.artifact,
        &flags.checkpoint.artifact,
        build.as_of_epoch,
    )?;
    let evidence = FixedActivityEvidence {
        schema_version: 1,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build.as_of_epoch,
        object_count: to_u64(snapshot.catalog.objects().len())?,
        delta_object_count: to_u64(inputs.len())?,
        physical_rows: snapshot.catalog.totals().physical_rows,
        baseline_evidence_sha256,
        batch_count: to_u64(batches.len())?,
        merge_count: merged.merge_count,
        activity_artifact: merged.final_run.checkpoint.artifact,
        flags_artifact: flags.checkpoint.artifact,
        metric_sha256: metric_sha256(&finalized.distinct_pubkeys, &finalized.active_users)?,
        distinct_period_rows: to_u64(finalized.distinct_pubkeys.len())?,
        active_period_rows: to_u64(finalized.active_users.len())?,
        max_week_kinds_buffered: finalized.max_week_kinds_buffered,
        max_month_kinds_buffered: finalized.max_month_kinds_buffered,
        distinct_pubkeys: finalized.distinct_pubkeys,
        active_users: finalized.active_users,
        max_merge_buffered_bytes: merged.max_buffered_bytes,
        estimated_run_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
        batch_checkpoints,
        merge_checkpoints: merged.checkpoints,
    };
    publish_canonical_json(evidence_path, &evidence)?;
    Ok(BoundedFixedActivity {
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
    // DuckDB 1.5's compressed-materialization optimizer can abort while
    // deriving integral statistics when an unsigned Parquet column contains
    // values just outside a narrowed projection, even when the WHERE clause
    // excludes them. Activity scans must safely tolerate pre-genesis and
    // post-API timestamps, so disable that one optimizer on this dedicated
    // bounded-build connection rather than risking a process abort.
    connection.execute_batch("SET disabled_optimizers = 'compressed_materialization'")?;
    let paths = locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT pubkey, TRY_CAST(created_at AS UINTEGER), kind::USMALLINT, id FROM read_parquet([{paths}], union_by_name=false) WHERE created_at >= {} AND created_at <= {} GROUP BY pubkey, created_at, kind, id ORDER BY pubkey, created_at, kind, id",
        NOSTR_GENESIS_TIMESTAMP, API_TIMESTAMP_MAX
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut count = 0_u64;
    let mut previous: Option<[u8; FIXED_ACTIVITY_KEY_BYTES]> = None;
    let mut min_key = None;
    let mut max_key = None;
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(0)?;
        let pubkey: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            BoundedExecutionError::Invalid(format!("Parquet pubkey has {} bytes", bytes.len()))
        })?;
        let mut key = [0_u8; FIXED_ACTIVITY_KEY_BYTES];
        key[..32].copy_from_slice(&pubkey);
        key[32..36].copy_from_slice(&row.get::<_, u32>(1)?.to_be_bytes());
        key[36..38].copy_from_slice(&row.get::<_, u16>(2)?.to_be_bytes());
        let id: Vec<u8> = row.get(3)?;
        let id: [u8; 32] = id.try_into().map_err(|id: Vec<u8>| {
            BoundedExecutionError::Invalid(format!("Parquet event ID has {} bytes", id.len()))
        })?;
        key[38..70].copy_from_slice(&id);
        if previous.is_some_and(|value| value >= key) {
            return Err(BoundedExecutionError::Invalid(
                "fixed-activity batch is not strictly sorted".to_owned(),
            )
            .into());
        }
        let encoded = hex::encode(key);
        min_key.get_or_insert_with(|| encoded.clone());
        max_key = Some(encoded);
        writer.write_all(&key)?;
        count = checked_add(count, 1, "activity batch rows")?;
        previous = Some(key);
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
            let input_ids = group.iter().map(run_input).collect::<Vec<_>>();
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
                let stats = merge_fixed_runs(
                    &paths,
                    &partial,
                    FixedRecordLayout {
                        record_bytes: FIXED_ACTIVITY_RECORD_BYTES,
                        key_bytes: FIXED_ACTIVITY_KEY_BYTES,
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
            max_buffered_bytes = max_buffered_bytes
                .max((group.len() + 1).saturating_mul(FIXED_ACTIVITY_RECORD_BYTES));
            merge_count = checked_add(merge_count, 1, "activity merge count")?;
            checkpoints.push(checkpoint_path.to_string_lossy().into_owned());
            next.push(completed_run(stem, completed, checkpoint_path, checkpoint));
        }
        runs = next;
        round = round.checked_add(1).ok_or_else(|| {
            BoundedExecutionError::Invalid("activity merge round overflow".to_owned())
        })?;
    }
    Ok(MergeOutcome {
        final_run: runs.pop().expect("at least empty activity run"),
        merge_count,
        max_buffered_bytes,
        checkpoints,
    })
}

fn build_flags(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    activity: &CompletedRun,
    root: &Path,
) -> Result<CompletedRun> {
    fs::create_dir_all(root)?;
    let completed = root.join("pubkey-flags.run");
    let checkpoint_path = root.join("pubkey-flags.json");
    let identity = RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: build.as_of_epoch,
        product: "fixed-activity-flags".to_owned(),
        product_version: FIXED_ACTIVITY_VERSION.to_owned(),
        key_space: "pubkey-32-flags-u8-v1".to_owned(),
    };
    let inputs = vec![run_input(activity)];
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &completed, &identity, &inputs)?
    {
        return Ok(completed_run(
            "pubkey-flags".to_owned(),
            completed,
            checkpoint_path,
            checkpoint,
        ));
    }
    let partial = unique_partial(&completed)?;
    let mut reader = ActivityReader::open(&activity.path)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    let mut current: Option<[u8; 32]> = None;
    let mut flags = 0_u8;
    let mut rows = 0_u64;
    let mut min_key = None;
    let mut max_key = None;
    while let Some(record) = reader.next()? {
        if u64::from(record.created_at) > build.as_of_epoch {
            continue;
        }
        if current.is_some_and(|pubkey| pubkey != record.pubkey) {
            let pubkey = current.expect("current pubkey");
            writer.write_all(&pubkey)?;
            writer.write_all(&[flags])?;
            rows = checked_add(rows, 1, "flag rows")?;
            flags = 0;
        }
        current = Some(record.pubkey);
        if record.kind == 0 {
            flags |= PROFILE_FLAG;
        } else if record.kind == 3 {
            flags |= FOLLOWS_FLAG;
        }
    }
    if let Some(pubkey) = current {
        writer.write_all(&pubkey)?;
        writer.write_all(&[flags])?;
        rows = checked_add(rows, 1, "flag rows")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if rows > 0 {
        let mut first = [0_u8; 32];
        File::open(&partial)?.read_exact(&mut first)?;
        min_key = Some(hex::encode(first));
        let mut file = File::open(&partial)?;
        file.seek(std::io::SeekFrom::End(-(PUBKEY_FLAGS_RECORD_BYTES as i64)))?;
        let mut last = [0_u8; 32];
        file.read_exact(&mut last)?;
        max_key = Some(hex::encode(last));
    }
    let checkpoint = publish_run_checkpoint(
        &partial,
        &completed,
        &checkpoint_path,
        identity,
        inputs,
        rows,
        min_key,
        max_key,
    )?;
    Ok(completed_run(
        "pubkey-flags".to_owned(),
        completed,
        checkpoint_path,
        checkpoint,
    ))
}

#[derive(Clone, Copy)]
struct ActivityRecord {
    pubkey: [u8; 32],
    created_at: u32,
    kind: u16,
}

struct ActivityReader {
    reader: BufReader<File>,
    previous: Option<[u8; FIXED_ACTIVITY_KEY_BYTES]>,
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
        let key: [u8; FIXED_ACTIVITY_KEY_BYTES] = bytes[..FIXED_ACTIVITY_KEY_BYTES]
            .try_into()
            .expect("fixed activity key");
        if self.previous.is_some_and(|previous| previous >= key) {
            return Err(BoundedExecutionError::Invalid(
                "activity state is not strictly sorted and unique".to_owned(),
            )
            .into());
        }
        self.previous = Some(key);
        Ok(Some(ActivityRecord {
            pubkey: key[..32].try_into().expect("fixed pubkey"),
            created_at: u32::from_be_bytes(key[32..36].try_into().expect("fixed timestamp")),
            kind: u16::from_be_bytes(key[36..38].try_into().expect("fixed kind")),
        }))
    }
}

#[derive(Default)]
struct ActiveAccumulator {
    active_users: u64,
    has_profile: u64,
    has_follows_list: u64,
    has_both: u64,
    total_events: u64,
}

struct FinalizedActivity {
    distinct_pubkeys: Vec<DistinctPubkeysPeriod>,
    active_users: Vec<ActiveUsersPeriod>,
    max_week_kinds_buffered: usize,
    max_month_kinds_buffered: usize,
}

fn finalize(
    activity_path: &Path,
    flags_path: &Path,
    activity_artifact: &ArtifactIdentity,
    flags_artifact: &ArtifactIdentity,
    as_of_epoch: u64,
) -> Result<FinalizedActivity> {
    let mut activity = ActivityReader::open(activity_path)?;
    let mut flags = BufReader::new(File::open(flags_path)?);
    let mut distinct = BTreeMap::<(u8, u32, Option<u16>), u64>::new();
    let mut active = BTreeMap::<(u8, u32), ActiveAccumulator>::new();
    let mut current_pubkey = None;
    let mut current_flags = 0_u8;
    let mut last_day = None;
    let mut last_week = None;
    let mut last_month = None;
    let mut active_day = None;
    let mut active_week = None;
    let mut active_month = None;
    let mut week_kinds = BTreeSet::new();
    let mut month_kinds = BTreeSet::new();
    let mut activity_rows = 0_u64;
    let mut flags_consumed = 0_u64;
    let mut max_week_kinds_buffered = 0_usize;
    let mut max_month_kinds_buffered = 0_usize;

    while let Some(record) = activity.next()? {
        activity_rows = checked_add(activity_rows, 1, "finalized activity rows")?;
        if u64::from(record.created_at) > as_of_epoch {
            continue;
        }
        if current_pubkey != Some(record.pubkey) {
            let (flag_pubkey, value) = read_flag_record(&mut flags)?.ok_or_else(|| {
                BoundedExecutionError::Invalid("flags end before activity state".to_owned())
            })?;
            if flag_pubkey != record.pubkey {
                return Err(BoundedExecutionError::Invalid(
                    "flags and activity pubkeys do not align".to_owned(),
                )
                .into());
            }
            flags_consumed = checked_add(flags_consumed, 1, "consumed flag rows")?;
            current_pubkey = Some(record.pubkey);
            current_flags = value;
            last_day = None;
            last_week = None;
            last_month = None;
            active_day = None;
            active_week = None;
            active_month = None;
            week_kinds.clear();
            month_kinds.clear();
        }
        let day = record.created_at / 86_400;
        let week = week_start(day);
        let month = month_start(day)?;
        if last_day != Some(day) {
            increment_map(&mut distinct, (0, day, None), 1, "daily distinct")?;
            last_day = Some(day);
        }
        increment_map(
            &mut distinct,
            (0, day, Some(record.kind)),
            1,
            "daily kind distinct",
        )?;
        if last_week != Some(week) {
            increment_map(&mut distinct, (1, week, None), 1, "weekly distinct")?;
            last_week = Some(week);
            week_kinds.clear();
        }
        if week_kinds.insert(record.kind) {
            max_week_kinds_buffered = max_week_kinds_buffered.max(week_kinds.len());
            increment_map(
                &mut distinct,
                (1, week, Some(record.kind)),
                1,
                "weekly kind distinct",
            )?;
        }
        if last_month != Some(month) {
            increment_map(&mut distinct, (2, month, None), 1, "monthly distinct")?;
            last_month = Some(month);
            month_kinds.clear();
        }
        if month_kinds.insert(record.kind) {
            max_month_kinds_buffered = max_month_kinds_buffered.max(month_kinds.len());
            increment_map(
                &mut distinct,
                (2, month, Some(record.kind)),
                1,
                "monthly kind distinct",
            )?;
        }

        if !matches!(record.kind, 445 | 1059) {
            add_active_events(&mut active, (0, day), 1)?;
            add_active_events(&mut active, (1, week), 1)?;
            add_active_events(&mut active, (2, month), 1)?;
            if active_day != Some(day) {
                add_active_pubkey(&mut active, (0, day), current_flags)?;
                active_day = Some(day);
            }
            if active_week != Some(week) {
                add_active_pubkey(&mut active, (1, week), current_flags)?;
                active_week = Some(week);
            }
            if active_month != Some(month) {
                add_active_pubkey(&mut active, (2, month), current_flags)?;
                active_month = Some(month);
            }
        }
    }
    if read_flag_record(&mut flags)?.is_some() {
        return Err(BoundedExecutionError::Invalid(
            "flags contain pubkeys absent from activity state".to_owned(),
        )
        .into());
    }
    if activity_rows != activity_artifact.row_count || flags_consumed != flags_artifact.row_count {
        return Err(BoundedExecutionError::Invalid(
            "fixed-activity finalization accounting mismatch".to_owned(),
        )
        .into());
    }
    let distinct_pubkeys = distinct
        .into_iter()
        .map(|((grain, period, kind), unique_pubkeys)| {
            Ok(DistinctPubkeysPeriod {
                grain: grain_name(grain)?.to_owned(),
                period_start: day_string(period)?,
                kind,
                unique_pubkeys,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let active_users = active
        .into_iter()
        .map(|((grain, period), value)| {
            Ok(ActiveUsersPeriod {
                grain: grain_name(grain)?.to_owned(),
                period_start: day_string(period)?,
                active_users: value.active_users,
                has_profile: value.has_profile,
                has_follows_list: value.has_follows_list,
                has_profile_and_follows_list: value.has_both,
                total_events: value.total_events,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(FinalizedActivity {
        distinct_pubkeys,
        active_users,
        max_week_kinds_buffered,
        max_month_kinds_buffered,
    })
}

fn add_active_events(
    rows: &mut BTreeMap<(u8, u32), ActiveAccumulator>,
    key: (u8, u32),
    count: u64,
) -> Result<()> {
    let row = rows.entry(key).or_default();
    row.total_events = checked_add(row.total_events, count, "active event total")?;
    Ok(())
}

fn add_active_pubkey(
    rows: &mut BTreeMap<(u8, u32), ActiveAccumulator>,
    key: (u8, u32),
    flags: u8,
) -> Result<()> {
    let row = rows.entry(key).or_default();
    row.active_users = checked_add(row.active_users, 1, "active users")?;
    if flags & PROFILE_FLAG != 0 {
        row.has_profile = checked_add(row.has_profile, 1, "profile users")?;
    }
    if flags & FOLLOWS_FLAG != 0 {
        row.has_follows_list = checked_add(row.has_follows_list, 1, "follows users")?;
    }
    if flags & (PROFILE_FLAG | FOLLOWS_FLAG) == PROFILE_FLAG | FOLLOWS_FLAG {
        row.has_both = checked_add(row.has_both, 1, "profile and follows users")?;
    }
    Ok(())
}

fn increment_map<K: Ord>(
    rows: &mut BTreeMap<K, u64>,
    key: K,
    value: u64,
    label: &str,
) -> Result<()> {
    let current = rows.get(&key).copied().unwrap_or(0);
    rows.insert(key, checked_add(current, value, label)?);
    Ok(())
}

fn read_flag_record(reader: &mut impl Read) -> Result<Option<([u8; 32], u8)>> {
    let mut bytes = [0_u8; PUBKEY_FLAGS_RECORD_BYTES];
    if !read_exact_or_eof(reader, &mut bytes)? {
        return Ok(None);
    }
    Ok(Some((
        bytes[..32].try_into().expect("fixed flag pubkey"),
        bytes[32],
    )))
}

fn read_exact_or_eof(reader: &mut impl Read, bytes: &mut [u8]) -> Result<bool> {
    let mut offset = 0;
    while offset < bytes.len() {
        let read = reader.read(&mut bytes[offset..])?;
        if read == 0 {
            if offset == 0 {
                return Ok(false);
            }
            return Err(BoundedExecutionError::Invalid(
                "fixed-activity artifact ends with a truncated record".to_owned(),
            )
            .into());
        }
        offset += read;
    }
    Ok(true)
}

fn week_start(day: u32) -> u32 {
    day - ((day + 3) % 7)
}

fn month_start(day: u32) -> Result<u32> {
    let date = day_date(day)?;
    let first = NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
        .ok_or_else(|| BoundedExecutionError::Invalid("invalid month start".to_owned()))?;
    u32::try_from(
        first
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| BoundedExecutionError::Invalid("invalid month timestamp".to_owned()))?
            .and_utc()
            .timestamp()
            / 86_400,
    )
    .map_err(|_| BoundedExecutionError::Invalid("month day exceeds u32".to_owned()).into())
}

fn day_date(day: u32) -> Result<NaiveDate> {
    DateTime::<Utc>::from_timestamp(i64::from(day) * 86_400, 0)
        .ok_or_else(|| BoundedExecutionError::Invalid("invalid UTC activity day".to_owned()).into())
        .map(|value| value.date_naive())
}

fn day_string(day: u32) -> Result<String> {
    Ok(day_date(day)?.to_string())
}

fn grain_name(grain: u8) -> Result<&'static str> {
    match grain {
        0 => Ok("day"),
        1 => Ok("week"),
        2 => Ok("month"),
        _ => Err(BoundedExecutionError::Invalid("invalid activity grain".to_owned()).into()),
    }
}

fn metric_sha256(
    distinct: &[DistinctPubkeysPeriod],
    active: &[ActiveUsersPeriod],
) -> Result<String> {
    let mut bytes = serde_json::to_vec_pretty(&(distinct, active)).map_err(|error| {
        BoundedExecutionError::Invalid(format!("serialize fixed-activity metrics: {error}"))
    })?;
    bytes.push(b'\n');
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn validate_artifact(artifact: &ArtifactIdentity, record_bytes: usize) -> Result<()> {
    let path = Path::new(&artifact.path);
    let metadata = path.metadata()?;
    if !metadata.is_file()
        || metadata.len() != artifact.byte_size
        || artifact.byte_size != artifact.row_count.saturating_mul(record_bytes as u64)
        || pensieve_lake::sha256_file(path)? != artifact.sha256
    {
        return Err(BoundedExecutionError::Invalid(
            "fixed-activity artifact identity mismatch".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn validate_merge(inputs: &[CompletedRun], stats: MergeStats) -> Result<()> {
    let expected = inputs.iter().try_fold(0_u64, |sum, run| {
        checked_add(
            sum,
            run.checkpoint.artifact.row_count,
            "activity merge inputs",
        )
    })?;
    if stats.input_records != expected || stats.output_records + stats.duplicate_records != expected
    {
        return Err(BoundedExecutionError::Invalid(
            "fixed-activity merge accounting mismatch".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn validate_config(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    config: &FixedActivityConfig,
) -> Result<()> {
    if build.as_of_epoch > API_TIMESTAMP_MAX
        || config.merge_fan_in < 2
        || snapshot.locations.len() != snapshot.catalog.objects().len()
    {
        return Err(BoundedExecutionError::Invalid(
            "invalid fixed-activity build configuration".to_owned(),
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
        product: format!("fixed-activity-{phase}"),
        product_version: FIXED_ACTIVITY_VERSION.to_owned(),
        key_space: "pubkey-32-created-at-u32-kind-u16-event-id-32-v2".to_owned(),
    }
}

fn build_empty(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    root: &Path,
) -> Result<CompletedRun> {
    let completed = root.join("empty.run");
    let checkpoint_path = root.join("empty.json");
    let identity = run_identity(snapshot, build, "empty");
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
    digest.update(b"pensieve-fixed-activity-merge-v1\0");
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
        .checked_mul(FIXED_ACTIVITY_RECORD_BYTES as u64)
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("activity byte estimate overflow".to_owned())
        })?;
    let mut rounds = 1_u64;
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        rounds = checked_add(rounds, 1, "activity merge rounds")?;
    }
    base.checked_mul(rounds)
        .and_then(|value| value.checked_add(rows.saturating_mul(PUBKEY_FLAGS_RECORD_BYTES as u64)))
        .ok_or_else(|| {
            BoundedExecutionError::Invalid("activity run estimate overflow".to_owned()).into()
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
            } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "run") {
                total = checked_add(total, entry.metadata()?.len(), "completed run bytes")?;
            }
        }
    }
    Ok(total)
}
