//! Bounded canonical facts required to finish the Postgres serving contract.
//!
//! The existing event-fact artifact deliberately omits content. This lane
//! retains only event ID and UTF-8 content bytes, joins it one-for-one with
//! the validated event-fact stream, and combines that with the validated
//! pubkey-sorted activity stream. The result is compact exact hourly counts
//! and exact per-kind summaries without cardinality-sized RAM state.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use duckdb::Connection;
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{API_TIMESTAMP_MAX, configure_execution, configure_remote_access, sql_string};
use crate::event_facts::verify_local_batch_inputs;
use crate::fixed_activity::exact_kind_unique_pubkeys_from_artifact;
use crate::{
    ArtifactIdentity, BatchLimits, BoundedExecutionError, BuildConfig, DiskBudget,
    EVENT_FACT_BYTES, EventFactReader, EventFactsEvidence, FIXED_ACTIVITY_RECORD_BYTES,
    FixedRecordLayout, InputBatch, InputIdentity, ObjectLocation, ResolvedSnapshot, Result,
    RunCheckpoint, RunIdentity, exact_kind_unique_pubkeys, load_bounded_fixed_activity,
    load_event_facts_evidence, load_reusable_checkpoint, merge_fixed_runs, plan_input_batches,
    preflight_disk, publish_canonical_json, publish_run_checkpoint,
};

/// Event ID plus exact UTF-8 content byte length.
pub const CONTENT_FACT_BYTES: usize = 32 + 8;
/// Sparse hourly row: hour, all-kind/kind key, exact count.
pub const HOURLY_COUNT_BYTES: usize = 4 + 4 + 8;
/// Kind, event/unique counts, first/last, content bytes/rows.
pub const KIND_SUMMARY_BYTES: usize = 2 + 8 + 8 + 4 + 4 + 8 + 8;
/// Stable product version.
pub const SERVING_FACTS_VERSION: &str = "serving-facts-v1";
/// Stable runner identity.
pub const SERVING_FACTS_RUNNER_VERSION: &str = "pensieve-analytics-serving-facts-v1";

const SCHEMA_VERSION: u32 = 1;
const CONTENT_KEY_BYTES: usize = 32;
const ALL_KINDS_KEY: u32 = 0;
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource settings for one bounded serving-facts build.
#[derive(Clone, Debug)]
pub struct ServingFactsConfig {
    /// Dedicated immutable work root.
    pub work_root: PathBuf,
    /// Maximum compressed bytes and physical rows per DuckDB scan.
    pub batch_limits: BatchLimits,
    /// Maximum sorted runs opened by one merge.
    pub merge_fan_in: usize,
    /// Free bytes that must remain after conservative preflight.
    pub disk_reserve_bytes: u64,
}

/// Explicit bounded-state evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServingFactsMemoryEvidence {
    /// Greatest compressed input bytes in one source batch.
    pub max_batch_bytes: u64,
    /// Greatest physical rows in one source batch.
    pub max_batch_rows: u64,
    /// Greatest fixed record bytes retained by a streaming merge.
    pub max_merge_buffered_bytes: usize,
    /// Sparse hourly keys retained during finalization.
    pub hourly_keys: usize,
    /// Fixed per-kind counter slots.
    pub kind_counter_slots: usize,
}

/// Canonical completion evidence for the serving-completeness product.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServingFactsEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner identity.
    pub runner_version: String,
    /// Completion status.
    pub status: String,
    /// Frozen canonical catalog identity.
    pub snapshot_id: String,
    /// Fixed analytics boundary.
    pub as_of_epoch: u64,
    /// Exclusive complete-hour boundary.
    pub complete_through_epoch: u64,
    /// Catalog object count.
    pub object_count: u64,
    /// Catalog physical row count.
    pub physical_rows: u64,
    /// Exact unique canonical event count.
    pub logical_events: u64,
    /// Physical duplicates suppressed by ID.
    pub duplicate_rows: u64,
    /// Immutable source event-facts evidence SHA-256.
    pub event_facts_evidence_sha256: String,
    /// Immutable source event-fact artifact identity.
    pub event_facts_artifact: ArtifactIdentity,
    /// Immutable source fixed-activity evidence SHA-256.
    pub activity_evidence_sha256: String,
    /// Immutable source activity artifact identity.
    pub activity_artifact: ArtifactIdentity,
    /// Canonical content fact artifact.
    pub content_artifact: ArtifactIdentity,
    /// Sparse exact hourly count artifact.
    pub hourly_artifact: ArtifactIdentity,
    /// Exact all-time kind summary artifact.
    pub kind_artifact: ArtifactIdentity,
    /// Sum of all-kind hourly rows.
    pub complete_hour_events: u64,
    /// Sum of represented kind event counts.
    pub eligible_kind_events: u64,
    /// Sum of represented exact content bytes.
    pub eligible_content_bytes: u64,
    /// Bounded source batches.
    pub batch_count: u64,
    /// Streaming merges.
    pub merge_count: u64,
    /// Conservative run byte estimate.
    pub estimated_run_bytes: u64,
    /// Configured disk reserve.
    pub disk_reserve_bytes: u64,
    /// Measured bounded state.
    pub memory: ServingFactsMemoryEvidence,
    /// Immutable batch checkpoints.
    pub batch_checkpoints: Vec<String>,
    /// Immutable merge checkpoints.
    pub merge_checkpoints: Vec<String>,
}

/// Fully validated serving-facts product.
pub struct BoundedServingFacts {
    /// Canonical evidence.
    pub evidence: ServingFactsEvidence,
    /// Evidence SHA-256.
    pub evidence_sha256: String,
}

/// One exact sparse hourly event-count row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServingHourlyRow {
    /// UTC hour number since the Unix epoch.
    pub hour_epoch: u32,
    /// Exact event kind, or all kinds.
    pub kind: Option<u16>,
    /// Canonical deduplicated event count.
    pub event_count: u64,
}

/// One exact all-time kind summary row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServingKindRow {
    /// Nostr event kind.
    pub kind: u16,
    /// Canonical eligible event count.
    pub event_count: u64,
    /// Exact eligible distinct publisher count.
    pub unique_pubkeys: u64,
    /// Earliest eligible timestamp.
    pub first_seen: u32,
    /// Latest eligible timestamp.
    pub last_seen: u32,
    /// Exact sum of UTF-8 content bytes.
    pub content_bytes: u64,
    /// Number of rows represented by the content-byte sum.
    pub content_rows: u64,
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

#[derive(Clone, Copy, Default)]
struct KindAccumulator {
    event_count: u64,
    first_seen: u32,
    last_seen: u32,
    content_bytes: u64,
    content_rows: u64,
}

struct DerivedCompact {
    hourly: BTreeMap<(u32, u32), u64>,
    kinds: Vec<KindAccumulator>,
    rows: u64,
}

/// Build exact content, hourly, and kind products from one frozen snapshot.
pub fn build_bounded_serving_facts(
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: ServingFactsConfig,
    event_facts_evidence_path: impl AsRef<Path>,
    activity_evidence_path: impl AsRef<Path>,
) -> Result<BoundedServingFacts> {
    validate_config(&snapshot, &build, &config)?;
    let (event_evidence, event_evidence_sha) =
        load_event_facts_evidence(event_facts_evidence_path)?;
    let activity = load_bounded_fixed_activity(activity_evidence_path)?;
    if event_evidence.snapshot_id != snapshot.catalog.snapshot_id
        || event_evidence.as_of_epoch != build.as_of_epoch
        || activity.evidence.snapshot_id != snapshot.catalog.snapshot_id
        || activity.evidence.as_of_epoch != build.as_of_epoch
    {
        return invalid("serving-facts source products do not share one frozen identity");
    }
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
            output_bytes: estimated_run_bytes,
            temporary_bytes: 0,
            retained_bytes: 0,
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
    let mut offset = 0_usize;
    let mut batch_duplicates = 0_u64;
    let mut max_batch_bytes = 0_u64;
    let mut max_batch_rows = 0_u64;
    let mut batch_checkpoints = Vec::with_capacity(batches.len());
    for batch in &batches {
        let end = offset
            .checked_add(batch.inputs.len())
            .ok_or_else(|| invalid_error("serving-facts batch offset overflowed"))?;
        let locations = snapshot
            .locations
            .get(offset..end)
            .ok_or_else(|| invalid_error("serving-facts batch locations are incomplete"))?;
        let run = build_batch(
            &connection,
            &snapshot,
            &build,
            batch,
            locations,
            &batch_root,
        )?;
        batch_duplicates = checked_add(
            batch_duplicates,
            batch
                .row_count
                .checked_sub(run.checkpoint.artifact.row_count)
                .ok_or_else(|| invalid_error("content batch output exceeds physical rows"))?,
            "content batch duplicates",
        )?;
        max_batch_bytes = max_batch_bytes.max(batch.byte_size);
        max_batch_rows = max_batch_rows.max(batch.row_count);
        batch_checkpoints.push(run.checkpoint_path.to_string_lossy().into_owned());
        runs.push(run);
        offset = end;
    }
    if offset != snapshot.locations.len() {
        return invalid("serving-facts batches did not consume all snapshot locations");
    }
    if runs.is_empty() {
        runs.push(build_empty(&snapshot, &build, &config.work_root)?);
    }
    let merge_root = config.work_root.join("merges");
    fs::create_dir_all(&merge_root)?;
    let merged = merge_to_single(runs, &snapshot, &build, config.merge_fan_in, &merge_root)?;
    let logical_events = merged.final_run.checkpoint.artifact.row_count;
    let duplicate_rows = snapshot
        .catalog
        .totals()
        .physical_rows
        .checked_sub(logical_events)
        .ok_or_else(|| invalid_error("content facts exceed physical rows"))?;
    if logical_events != event_evidence.logical_events
        || checked_add(
            batch_duplicates,
            merged.duplicate_rows,
            "content duplicates",
        )? != duplicate_rows
    {
        return invalid("content facts do not reconcile to canonical event facts");
    }

    let unique_by_kind = exact_kind_unique_pubkeys(&activity)?;
    let compact_root = config.work_root.join("compact");
    fs::create_dir_all(&compact_root)?;
    let finalized = finalize(
        &event_evidence,
        &merged.final_run.path,
        &unique_by_kind,
        build.as_of_epoch,
        &compact_root,
        &snapshot,
        &build,
    )?;
    let evidence = ServingFactsEvidence {
        schema_version: SCHEMA_VERSION,
        runner_version: SERVING_FACTS_RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build.as_of_epoch,
        complete_through_epoch: floor_hour(build.as_of_epoch),
        object_count: to_u64(snapshot.catalog.objects().len(), "object count")?,
        physical_rows: snapshot.catalog.totals().physical_rows,
        logical_events,
        duplicate_rows,
        event_facts_evidence_sha256: event_evidence_sha,
        event_facts_artifact: event_evidence.final_artifact,
        activity_evidence_sha256: activity.evidence_sha256,
        activity_artifact: activity.evidence.activity_artifact,
        content_artifact: merged.final_run.checkpoint.artifact,
        hourly_artifact: finalized.hourly,
        kind_artifact: finalized.kinds,
        complete_hour_events: finalized.complete_hour_events,
        eligible_kind_events: finalized.eligible_kind_events,
        eligible_content_bytes: finalized.eligible_content_bytes,
        batch_count: to_u64(batches.len(), "batch count")?,
        merge_count: merged.merge_count,
        estimated_run_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
        memory: ServingFactsMemoryEvidence {
            max_batch_bytes,
            max_batch_rows,
            max_merge_buffered_bytes: merged.max_buffered_bytes,
            hourly_keys: finalized.hourly_keys,
            kind_counter_slots: 65_536,
        },
        batch_checkpoints,
        merge_checkpoints: merged.checkpoints,
    };
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    let product = BoundedServingFacts {
        evidence,
        evidence_sha256: pensieve_lake::sha256_file(evidence_path.as_ref())?,
    };
    validate_bounded_serving_facts(&product)?;
    Ok(product)
}

struct Finalized {
    hourly: ArtifactIdentity,
    kinds: ArtifactIdentity,
    complete_hour_events: u64,
    eligible_kind_events: u64,
    eligible_content_bytes: u64,
    hourly_keys: usize,
}

fn finalize(
    event_evidence: &EventFactsEvidence,
    content_path: &Path,
    unique_by_kind: &[u64],
    as_of_epoch: u64,
    root: &Path,
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
) -> Result<Finalized> {
    if unique_by_kind.len() != 65_536 {
        return invalid("kind unique-pubkey vector has the wrong fixed domain");
    }
    let derived = derive_compact_rows(
        Path::new(&event_evidence.final_artifact.path),
        event_evidence.logical_events,
        content_path,
        as_of_epoch,
    )?;
    let content_sha = pensieve_lake::sha256_file(content_path)?;
    let compact_input = InputIdentity {
        identity: format!("sha256:{content_sha}"),
        byte_size: content_path.metadata()?.len(),
        row_count: derived.rows,
        sha256: content_sha,
    };
    let hourly_path = root.join("hourly-counts.run");
    let hourly_keys = derived.hourly.len();
    let hourly = publish_hourly(
        &hourly_path,
        derived.hourly,
        snapshot,
        build,
        compact_input.clone(),
    )?;
    let kind_path = root.join("kind-summaries.run");
    let kinds = publish_kinds(
        &kind_path,
        &derived.kinds,
        unique_by_kind,
        snapshot,
        build,
        compact_input,
    )?;
    let complete_hour_events = read_hourly_sum(&hourly_path, hourly.row_count, true)?;
    let mut eligible_kind_events = 0_u64;
    let mut eligible_content_bytes = 0_u64;
    visit_kind_artifact(&kind_path, kinds.row_count, |row| {
        eligible_kind_events = checked_add(eligible_kind_events, row.event_count, "kind sum")?;
        eligible_content_bytes =
            checked_add(eligible_content_bytes, row.content_bytes, "content sum")?;
        Ok(())
    })?;
    Ok(Finalized {
        hourly_keys,
        hourly,
        kinds,
        complete_hour_events,
        eligible_kind_events,
        eligible_content_bytes,
    })
}

fn derive_compact_rows(
    event_path: &Path,
    expected_rows: u64,
    content_path: &Path,
    as_of_epoch: u64,
) -> Result<DerivedCompact> {
    let mut events = EventFactReader::open(event_path)?;
    let mut content = ContentFactReader::open(content_path)?;
    let mut hourly = BTreeMap::<(u32, u32), u64>::new();
    let mut kinds = vec![KindAccumulator::default(); 65_536];
    let complete_through = floor_hour(as_of_epoch);
    let mut rows = 0_u64;
    loop {
        match (events.read_next()?, content.next()?) {
            (Some(event), Some(content)) if event.id == content.id => {
                rows = checked_add(rows, 1, "serving finalization rows")?;
                if event.created_at < u64::from(NOSTR_GENESIS_TIMESTAMP)
                    || event.created_at > as_of_epoch
                    || event.created_at > API_TIMESTAMP_MAX
                {
                    continue;
                }
                let created_at = u32::try_from(event.created_at)
                    .map_err(|_| invalid_error("eligible timestamp exceeds u32"))?;
                let accumulator = &mut kinds[usize::from(event.kind)];
                accumulator.event_count =
                    checked_add(accumulator.event_count, 1, "kind event count")?;
                accumulator.content_bytes = checked_add(
                    accumulator.content_bytes,
                    content.content_bytes,
                    "kind content bytes",
                )?;
                accumulator.content_rows =
                    checked_add(accumulator.content_rows, 1, "kind content rows")?;
                if accumulator.event_count == 1 {
                    accumulator.first_seen = created_at;
                    accumulator.last_seen = created_at;
                } else {
                    accumulator.first_seen = accumulator.first_seen.min(created_at);
                    accumulator.last_seen = accumulator.last_seen.max(created_at);
                }
                if event.created_at < complete_through {
                    let hour = u32::try_from(event.created_at / 3_600)
                        .map_err(|_| invalid_error("hour key exceeds u32"))?;
                    increment_map(&mut hourly, (hour, ALL_KINDS_KEY), "all-kind hourly")?;
                    increment_map(
                        &mut hourly,
                        (hour, u32::from(event.kind) + 1),
                        "per-kind hourly",
                    )?;
                }
            }
            (None, None) => break,
            (Some(event), Some(content)) => {
                return Err(BoundedExecutionError::Invalid(format!(
                    "event/content fact identity mismatch: {} != {}",
                    hex::encode(event.id),
                    hex::encode(content.id)
                ))
                .into());
            }
            _ => return invalid("event/content fact streams have different lengths"),
        }
    }
    if rows != expected_rows {
        return invalid("serving finalization did not consume every canonical event");
    }
    Ok(DerivedCompact {
        hourly,
        kinds,
        rows,
    })
}

#[derive(Clone, Copy)]
struct ContentFact {
    id: [u8; 32],
    content_bytes: u64,
}

impl ContentFact {
    fn encode(self) -> [u8; CONTENT_FACT_BYTES] {
        let mut bytes = [0_u8; CONTENT_FACT_BYTES];
        bytes[..32].copy_from_slice(&self.id);
        bytes[32..].copy_from_slice(&self.content_bytes.to_be_bytes());
        bytes
    }
}

struct ContentFactReader {
    reader: BufReader<File>,
    previous: Option<[u8; 32]>,
}

impl ContentFactReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<ContentFact>> {
        let mut bytes = [0_u8; CONTENT_FACT_BYTES];
        if !read_exact_or_eof(&mut self.reader, &mut bytes)? {
            return Ok(None);
        }
        let id: [u8; 32] = bytes[..32].try_into().expect("content ID");
        if self.previous.is_some_and(|previous| previous >= id) {
            return invalid("content facts are not strictly sorted and unique");
        }
        self.previous = Some(id);
        Ok(Some(ContentFact {
            id,
            content_bytes: u64::from_be_bytes(bytes[32..].try_into().expect("content bytes")),
        }))
    }
}

struct ContentScanStats {
    rows: u64,
    min_id: Option<String>,
    max_id: Option<String>,
}

fn scan_content_facts(
    connection: &Connection,
    locations: &[ObjectLocation],
    writer: &mut impl Write,
) -> Result<ContentScanStats> {
    if locations.is_empty() {
        return invalid("a content-fact batch must contain an object");
    }
    let paths = locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, octet_length(encode(content))::UBIGINT AS content_bytes \
         FROM read_parquet([{paths}], union_by_name=false) \
         ORDER BY id, content_bytes"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut previous: Option<ContentFact> = None;
    let mut output = 0_u64;
    let mut min_id = None;
    let mut max_id = None;
    while let Some(row) = rows.next()? {
        let id: Vec<u8> = row.get(0)?;
        let fact = ContentFact {
            id: id.try_into().map_err(|id: Vec<u8>| {
                invalid_error(&format!("content ID has {} bytes", id.len()))
            })?,
            content_bytes: row.get(1)?,
        };
        match previous {
            Some(value) if value.id == fact.id => {
                if value.content_bytes != fact.content_bytes {
                    return invalid("duplicate event ID has conflicting content length");
                }
            }
            Some(value) if value.id > fact.id => {
                return invalid("DuckDB content facts are not sorted by event ID");
            }
            Some(value) => {
                writer.write_all(&value.encode())?;
                output = checked_add(output, 1, "content output rows")?;
                min_id.get_or_insert_with(|| hex::encode(value.id));
                max_id = Some(hex::encode(value.id));
                previous = Some(fact);
            }
            None => previous = Some(fact),
        }
    }
    if let Some(value) = previous {
        writer.write_all(&value.encode())?;
        output = checked_add(output, 1, "content output rows")?;
        min_id.get_or_insert_with(|| hex::encode(value.id));
        max_id = Some(hex::encode(value.id));
    }
    Ok(ContentScanStats {
        rows: output,
        min_id,
        max_id,
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
    let path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let identity = run_identity(snapshot, build, "batch");
    if let Some(checkpoint) =
        load_reusable_checkpoint(&checkpoint_path, &path, &identity, &batch.inputs)?
    {
        return Ok(CompletedRun {
            path,
            checkpoint_path,
            checkpoint,
        });
    }
    verify_local_batch_inputs(&batch.inputs, locations)?;
    let partial = unique_partial(&path)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    let stats = scan_content_facts(connection, locations, &mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let checkpoint = publish_run_checkpoint(
        &partial,
        &path,
        &checkpoint_path,
        identity,
        batch.inputs.clone(),
        stats.rows,
        stats.min_id,
        stats.max_id,
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
    build: &BuildConfig,
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
            let output = merge_group(group, snapshot, build, round, group_index, fan_in, root)?;
            let input_rows = group.iter().try_fold(0_u64, |sum, run| {
                checked_add(sum, run.checkpoint.artifact.row_count, "merge input rows")
            })?;
            duplicate_rows = checked_add(
                duplicate_rows,
                input_rows
                    .checked_sub(output.checkpoint.artifact.row_count)
                    .ok_or_else(|| invalid_error("content merge output exceeds inputs"))?,
                "content merge duplicates",
            )?;
            max_buffered_bytes = max_buffered_bytes.max(
                group
                    .len()
                    .checked_add(1)
                    .and_then(|count| count.checked_mul(CONTENT_FACT_BYTES))
                    .ok_or_else(|| invalid_error("content merge memory overflowed"))?,
            );
            merge_count = checked_add(merge_count, 1, "content merge count")?;
            checkpoints.push(output.checkpoint_path.to_string_lossy().into_owned());
            next.push(output);
        }
        runs = next;
        round = round
            .checked_add(1)
            .ok_or_else(|| invalid_error("content merge round overflowed"))?;
    }
    Ok(MergeOutcome {
        final_run: runs.pop().expect("at least one content run"),
        merge_count,
        duplicate_rows,
        max_buffered_bytes,
        checkpoints,
    })
}

fn merge_group(
    inputs: &[CompletedRun],
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    round: u32,
    group_index: usize,
    fan_in: usize,
    root: &Path,
) -> Result<CompletedRun> {
    let input_identities = inputs.iter().map(run_input).collect::<Vec<_>>();
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&input_identities).map_err(BoundedExecutionError::from)?,
    ));
    let stem = format!("merge-{round:04}-{group_index:08}-{}", &digest[..16]);
    let path = root.join(format!("{stem}.run"));
    let checkpoint_path = root.join(format!("{stem}.json"));
    let identity = run_identity(snapshot, build, "merge");
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
            record_bytes: CONTENT_FACT_BYTES,
            key_bytes: CONTENT_KEY_BYTES,
        },
        fan_in,
    )?;
    let expected = inputs.iter().try_fold(0_u64, |sum, run| {
        checked_add(sum, run.checkpoint.artifact.row_count, "content merge rows")
    })?;
    if stats.input_records != expected
        || checked_add(
            stats.output_records,
            stats.duplicate_records,
            "content merge accounting",
        )? != expected
    {
        return invalid("content merge accounting mismatch");
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

fn build_empty(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    root: &Path,
) -> Result<CompletedRun> {
    let path = root.join("empty.run");
    let checkpoint_path = root.join("empty.json");
    let identity = run_identity(snapshot, build, "empty");
    let inputs = vec![InputIdentity {
        identity: format!("catalog:{}", snapshot.catalog.snapshot_id),
        byte_size: 0,
        row_count: 0,
        sha256: snapshot
            .catalog
            .snapshot_id
            .strip_prefix("sha256:")
            .ok_or_else(|| invalid_error("snapshot is not a SHA-256 ID"))?
            .to_owned(),
    }];
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

fn publish_hourly(
    path: &Path,
    rows: BTreeMap<(u32, u32), u64>,
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    input: InputIdentity,
) -> Result<ArtifactIdentity> {
    let checkpoint_path = path.with_extension("json");
    let identity = compact_identity(snapshot, build, "hourly", "hour-u32-kind-key-u32-count-u64");
    if let Some(checkpoint) = load_reusable_checkpoint(
        &checkpoint_path,
        path,
        &identity,
        std::slice::from_ref(&input),
    )? {
        return Ok(checkpoint.artifact);
    }
    let partial = unique_partial(path)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    for ((hour, kind_key), count) in &rows {
        writer.write_all(&hour.to_be_bytes())?;
        writer.write_all(&kind_key.to_be_bytes())?;
        writer.write_all(&count.to_be_bytes())?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    Ok(publish_run_checkpoint(
        &partial,
        path,
        &checkpoint_path,
        identity,
        vec![input],
        to_u64(rows.len(), "hourly rows")?,
        rows.first_key_value()
            .map(|((hour, kind), _)| format!("{hour:08x}{kind:08x}")),
        rows.last_key_value()
            .map(|((hour, kind), _)| format!("{hour:08x}{kind:08x}")),
    )?
    .artifact)
}

fn publish_kinds(
    path: &Path,
    rows: &[KindAccumulator],
    unique: &[u64],
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    input: InputIdentity,
) -> Result<ArtifactIdentity> {
    let checkpoint_path = path.with_extension("json");
    let identity = compact_identity(snapshot, build, "kinds", "kind-u16-summary-v1");
    if let Some(checkpoint) = load_reusable_checkpoint(
        &checkpoint_path,
        path,
        &identity,
        std::slice::from_ref(&input),
    )? {
        return Ok(checkpoint.artifact);
    }
    let partial = unique_partial(path)?;
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    let mut count = 0_u64;
    let mut first = None;
    let mut last = None;
    for (kind, row) in rows.iter().enumerate() {
        if row.event_count == 0 {
            if unique[kind] != 0 {
                return invalid("kind activity exists without an eligible canonical event");
            }
            continue;
        }
        let kind = u16::try_from(kind).expect("fixed kind domain");
        writer.write_all(&kind.to_be_bytes())?;
        writer.write_all(&row.event_count.to_be_bytes())?;
        writer.write_all(&unique[usize::from(kind)].to_be_bytes())?;
        writer.write_all(&row.first_seen.to_be_bytes())?;
        writer.write_all(&row.last_seen.to_be_bytes())?;
        writer.write_all(&row.content_bytes.to_be_bytes())?;
        writer.write_all(&row.content_rows.to_be_bytes())?;
        count = checked_add(count, 1, "kind summary rows")?;
        first.get_or_insert(kind);
        last = Some(kind);
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    Ok(publish_run_checkpoint(
        &partial,
        path,
        &checkpoint_path,
        identity,
        vec![input],
        count,
        first.map(|kind| format!("{kind:04x}")),
        last.map(|kind| format!("{kind:04x}")),
    )?
    .artifact)
}

/// Visit exact kind rows without buffering the artifact.
pub fn visit_serving_kind_rows(
    product: &BoundedServingFacts,
    visitor: impl FnMut(ServingKindRow) -> Result<()>,
) -> Result<()> {
    visit_kind_artifact(
        Path::new(&product.evidence.kind_artifact.path),
        product.evidence.kind_artifact.row_count,
        visitor,
    )
}

fn visit_kind_artifact(
    path: &Path,
    expected_rows: u64,
    mut visitor: impl FnMut(ServingKindRow) -> Result<()>,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut rows = 0_u64;
    let mut previous = None;
    loop {
        let mut bytes = [0_u8; KIND_SUMMARY_BYTES];
        if !read_exact_or_eof(&mut reader, &mut bytes)? {
            break;
        }
        let kind = u16::from_be_bytes(bytes[..2].try_into().expect("kind"));
        if previous.is_some_and(|value| value >= kind) {
            return invalid("kind summary artifact is not strictly sorted");
        }
        previous = Some(kind);
        let row = ServingKindRow {
            kind,
            event_count: u64::from_be_bytes(bytes[2..10].try_into().expect("event count")),
            unique_pubkeys: u64::from_be_bytes(bytes[10..18].try_into().expect("unique pubkeys")),
            first_seen: u32::from_be_bytes(bytes[18..22].try_into().expect("first seen")),
            last_seen: u32::from_be_bytes(bytes[22..26].try_into().expect("last seen")),
            content_bytes: u64::from_be_bytes(bytes[26..34].try_into().expect("content bytes")),
            content_rows: u64::from_be_bytes(bytes[34..42].try_into().expect("content rows")),
        };
        if row.event_count == 0
            || row.unique_pubkeys == 0
            || row.unique_pubkeys > row.event_count
            || row.first_seen > row.last_seen
            || row.content_rows != row.event_count
        {
            return invalid("kind summary artifact contains invalid metrics");
        }
        rows = checked_add(rows, 1, "kind summary rows")?;
        visitor(row)?;
    }
    if rows != expected_rows {
        return invalid("kind summary artifact row count changed");
    }
    Ok(())
}

/// Visit exact sparse hourly rows without buffering the artifact.
pub fn visit_serving_hourly_rows(
    product: &BoundedServingFacts,
    visitor: impl FnMut(ServingHourlyRow) -> Result<()>,
) -> Result<()> {
    visit_hourly_artifact(
        Path::new(&product.evidence.hourly_artifact.path),
        product.evidence.hourly_artifact.row_count,
        visitor,
    )
}

fn visit_hourly_artifact(
    path: &Path,
    expected_rows: u64,
    mut visitor: impl FnMut(ServingHourlyRow) -> Result<()>,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut rows = 0_u64;
    let mut previous = None;
    loop {
        let mut bytes = [0_u8; HOURLY_COUNT_BYTES];
        if !read_exact_or_eof(&mut reader, &mut bytes)? {
            break;
        }
        let key: [u8; 8] = bytes[..8].try_into().expect("hourly key");
        if previous.is_some_and(|value| value >= key) {
            return invalid("hourly artifact is not strictly sorted");
        }
        previous = Some(key);
        let hour_epoch = u32::from_be_bytes(bytes[..4].try_into().expect("hour"));
        let kind_key = u32::from_be_bytes(bytes[4..8].try_into().expect("kind key"));
        let kind =
            if kind_key == ALL_KINDS_KEY {
                None
            } else {
                Some(u16::try_from(kind_key - 1).map_err(|_| {
                    invalid_error("hourly artifact contains an out-of-domain kind key")
                })?)
            };
        let event_count = u64::from_be_bytes(bytes[8..].try_into().expect("hourly count"));
        if event_count == 0 {
            return invalid("hourly artifact contains a zero count");
        }
        rows = checked_add(rows, 1, "hourly rows")?;
        visitor(ServingHourlyRow {
            hour_epoch,
            kind,
            event_count,
        })?;
    }
    if rows != expected_rows {
        return invalid("hourly artifact row count changed");
    }
    Ok(())
}

fn read_hourly_sum(path: &Path, expected_rows: u64, all_kinds: bool) -> Result<u64> {
    let mut sum = 0_u64;
    visit_hourly_artifact(path, expected_rows, |row| {
        if row.kind.is_none() == all_kinds {
            sum = checked_add(sum, row.event_count, "hourly count sum")?;
        }
        Ok(())
    })?;
    Ok(sum)
}

/// Load and fully revalidate completed serving-facts evidence.
pub fn load_bounded_serving_facts(path: impl AsRef<Path>) -> Result<BoundedServingFacts> {
    let path = path.as_ref();
    let product = BoundedServingFacts {
        evidence: serde_json::from_slice(&fs::read(path)?).map_err(BoundedExecutionError::from)?,
        evidence_sha256: pensieve_lake::sha256_file(path)?,
    };
    validate_bounded_serving_facts(&product)?;
    Ok(product)
}

fn validate_bounded_serving_facts(product: &BoundedServingFacts) -> Result<()> {
    let evidence = &product.evidence;
    if evidence.schema_version != SCHEMA_VERSION
        || evidence.runner_version != SERVING_FACTS_RUNNER_VERSION
        || evidence.status != "completed"
        || evidence.complete_through_epoch != floor_hour(evidence.as_of_epoch)
        || evidence.content_artifact.row_count != evidence.logical_events
        || checked_add(
            evidence.logical_events,
            evidence.duplicate_rows,
            "loaded content rows",
        )? != evidence.physical_rows
    {
        return invalid("serving-facts evidence identity or accounting is invalid");
    }
    validate_artifact(&evidence.content_artifact, CONTENT_FACT_BYTES)?;
    validate_artifact(&evidence.hourly_artifact, HOURLY_COUNT_BYTES)?;
    validate_artifact(&evidence.kind_artifact, KIND_SUMMARY_BYTES)?;
    validate_artifact(&evidence.event_facts_artifact, EVENT_FACT_BYTES)?;
    validate_artifact(&evidence.activity_artifact, FIXED_ACTIVITY_RECORD_BYTES)?;
    if evidence.event_facts_artifact.row_count != evidence.logical_events {
        return invalid("serving-facts event source row count is invalid");
    }
    let derived = derive_compact_rows(
        Path::new(&evidence.event_facts_artifact.path),
        evidence.logical_events,
        Path::new(&evidence.content_artifact.path),
        evidence.as_of_epoch,
    )?;
    if derived.rows != evidence.logical_events {
        return invalid("serving-facts source artifacts do not reconcile");
    }
    let mut expected_hourly = derived.hourly.into_iter();
    let mut all_kind_sum = 0_u64;
    visit_hourly_artifact(
        Path::new(&evidence.hourly_artifact.path),
        evidence.hourly_artifact.row_count,
        |actual| {
            let Some(((hour, kind_key), count)) = expected_hourly.next() else {
                return invalid("hourly artifact has an unexpected row");
            };
            let kind = if kind_key == ALL_KINDS_KEY {
                None
            } else {
                Some(u16::try_from(kind_key - 1).expect("derived kind key"))
            };
            if actual
                != (ServingHourlyRow {
                    hour_epoch: hour,
                    kind,
                    event_count: count,
                })
            {
                return invalid("hourly artifact differs from canonical sources");
            }
            if actual.kind.is_none() {
                all_kind_sum = checked_add(all_kind_sum, actual.event_count, "all-kind sum")?;
            }
            Ok(())
        },
    )?;
    if expected_hourly.next().is_some()
        || all_kind_sum != evidence.complete_hour_events
        || to_u64(evidence.memory.hourly_keys, "hourly key count")?
            != evidence.hourly_artifact.row_count
    {
        return invalid("serving-facts hourly artifact does not reconcile");
    }
    let unique_by_kind = exact_kind_unique_pubkeys_from_artifact(
        Path::new(&evidence.activity_artifact.path),
        evidence.activity_artifact.row_count,
        evidence.as_of_epoch,
    )?;
    let mut kind_events = 0_u64;
    let mut content_bytes = 0_u64;
    let mut kind_rows = 0_u64;
    let mut expected_kind = derived
        .kinds
        .iter()
        .enumerate()
        .filter(|(_, row)| row.event_count != 0);
    visit_kind_artifact(
        Path::new(&evidence.kind_artifact.path),
        evidence.kind_artifact.row_count,
        |row| {
            let Some((kind, expected)) = expected_kind.next() else {
                return invalid("kind artifact has an unexpected row");
            };
            if row
                != (ServingKindRow {
                    kind: u16::try_from(kind).expect("fixed kind domain"),
                    event_count: expected.event_count,
                    unique_pubkeys: unique_by_kind[kind],
                    first_seen: expected.first_seen,
                    last_seen: expected.last_seen,
                    content_bytes: expected.content_bytes,
                    content_rows: expected.content_rows,
                })
            {
                return invalid("kind artifact differs from canonical sources");
            }
            kind_rows = checked_add(kind_rows, 1, "loaded kind rows")?;
            kind_events = checked_add(kind_events, row.event_count, "loaded kind events")?;
            content_bytes = checked_add(content_bytes, row.content_bytes, "loaded content bytes")?;
            Ok(())
        },
    )?;
    if expected_kind.next().is_some()
        || kind_events != evidence.eligible_kind_events
        || content_bytes != evidence.eligible_content_bytes
        || kind_rows != evidence.kind_artifact.row_count
    {
        return invalid("serving-facts kind artifact does not reconcile");
    }
    Ok(())
}

fn validate_artifact(artifact: &ArtifactIdentity, width: usize) -> Result<()> {
    let path = Path::new(&artifact.path);
    let expected = artifact
        .row_count
        .checked_mul(u64::try_from(width).expect("fixed width fits u64"))
        .ok_or_else(|| invalid_error("artifact byte accounting overflowed"))?;
    if artifact.byte_size != expected
        || path.metadata()?.len() != expected
        || pensieve_lake::sha256_file(path)? != artifact.sha256
    {
        return invalid("serving-facts artifact identity mismatch");
    }
    Ok(())
}

fn validate_config(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    config: &ServingFactsConfig,
) -> Result<()> {
    if build.as_of_epoch > API_TIMESTAMP_MAX
        || config.merge_fan_in < 2
        || snapshot.locations.len() != snapshot.catalog.objects().len()
    {
        return invalid("invalid serving-facts build configuration");
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
        product: format!("serving-content-{phase}"),
        product_version: SERVING_FACTS_VERSION.to_owned(),
        key_space: "event-id-32-content-bytes-u64-v1".to_owned(),
    }
}

fn compact_identity(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    phase: &str,
    key_space: &str,
) -> RunIdentity {
    RunIdentity {
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of: build.as_of_epoch,
        product: format!("serving-{phase}"),
        product_version: SERVING_FACTS_VERSION.to_owned(),
        key_space: key_space.to_owned(),
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

fn estimate_run_bytes(rows: u64, batches: usize, fan_in: usize) -> Result<u64> {
    let mut levels = 1_u64;
    let mut runs = batches.max(1);
    while runs > 1 {
        runs = runs.div_ceil(fan_in);
        levels = checked_add(levels, 1, "content merge levels")?;
    }
    rows.checked_mul(u64::try_from(CONTENT_FACT_BYTES).expect("record width fits u64"))
        .and_then(|bytes| bytes.checked_mul(levels))
        .ok_or_else(|| invalid_error("content run estimate overflowed").into())
}

fn increment_map<K: Ord>(map: &mut BTreeMap<K, u64>, key: K, label: &str) -> Result<()> {
    let value = map.entry(key).or_default();
    *value = checked_add(*value, 1, label)?;
    Ok(())
}

fn floor_hour(epoch: u64) -> u64 {
    epoch / 3_600 * 3_600
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| invalid_error(&format!("{label} overflowed")).into())
}

fn to_u64(value: usize, label: &str) -> Result<u64> {
    value
        .try_into()
        .map_err(|_| invalid_error(&format!("{label} exceeds u64")).into())
}

fn unique_partial(path: &Path) -> Result<PathBuf> {
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .ok_or_else(|| invalid_error("serving artifact path has no filename"))?
        .to_string_lossy();
    Ok(path.with_file_name(format!(
        ".{name}.{}.{}.partial",
        std::process::id(),
        sequence
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
            return invalid("fixed-width artifact ends with a truncated record");
        }
        offset += read;
    }
    Ok(true)
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(invalid_error(message).into())
}

fn invalid_error(message: &str) -> BoundedExecutionError {
    BoundedExecutionError::Invalid(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_reader_rejects_truncation_and_order_regression() {
        let directory = tempfile::tempdir().unwrap();
        let truncated = directory.path().join("truncated.run");
        fs::write(&truncated, [0_u8; CONTENT_FACT_BYTES - 1]).unwrap();
        assert!(ContentFactReader::open(&truncated).unwrap().next().is_err());

        let unsorted = directory.path().join("unsorted.run");
        let mut bytes = Vec::new();
        bytes.extend(
            ContentFact {
                id: [2; 32],
                content_bytes: 1,
            }
            .encode(),
        );
        bytes.extend(
            ContentFact {
                id: [1; 32],
                content_bytes: 1,
            }
            .encode(),
        );
        fs::write(&unsorted, bytes).unwrap();
        let mut reader = ContentFactReader::open(&unsorted).unwrap();
        assert!(reader.next().unwrap().is_some());
        assert!(reader.next().is_err());
    }

    #[test]
    fn utf8_content_scan_deduplicates_and_counts_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let parquet = directory.path().join("events.parquet");
        let connection = Connection::open_in_memory().unwrap();
        let id_one = format!("{:0>64}", "1");
        let id_two = format!("{:0>64}", "2");
        connection
            .execute_batch(&format!(
                "CREATE TABLE events(id BLOB, content VARCHAR); \
                 INSERT INTO events VALUES \
                   (from_hex('{}'), 'é'), (from_hex('{}'), 'é'), (from_hex('{}'), 'abc'); \
                 COPY events TO '{}' (FORMAT parquet)",
                id_one,
                id_one,
                id_two,
                parquet.display()
            ))
            .unwrap();
        let mut output = Vec::new();
        let stats = scan_content_facts(&connection, &[ObjectLocation::Local(parquet)], &mut output)
            .unwrap();
        assert_eq!(stats.rows, 2);
        let expected_min_id = format!("{:0>64}", "1");
        let expected_max_id = format!("{:0>64}", "2");
        assert_eq!(stats.min_id.as_deref(), Some(expected_min_id.as_str()));
        assert_eq!(stats.max_id.as_deref(), Some(expected_max_id.as_str()));
        assert_eq!(output.len(), 2 * CONTENT_FACT_BYTES);
        assert_eq!(u64::from_be_bytes(output[32..40].try_into().unwrap()), 2);
        assert_eq!(
            u64::from_be_bytes(output[CONTENT_FACT_BYTES + 32..].try_into().unwrap()),
            3
        );
    }

    #[test]
    fn sparse_hourly_encoding_reserves_zero_for_all_kinds() {
        assert_eq!(ALL_KINDS_KEY, 0);
        assert_eq!(u32::from(u16::MAX) + 1, 65_536);
        assert_eq!(floor_hour(7_199), 3_600);
    }

    #[test]
    fn compact_rows_exclude_incomplete_hours_but_keep_all_time_kind_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let event_path = directory.path().join("events.run");
        let content_path = directory.path().join("content.run");
        let as_of = u64::from(NOSTR_GENESIS_TIMESTAMP) + 10_000;
        let complete_through = floor_hour(as_of);
        let events = [
            crate::EventFact {
                id: [1; 32],
                created_at: u64::from(NOSTR_GENESIS_TIMESTAMP) - 1,
                kind: 7,
            },
            crate::EventFact {
                id: [2; 32],
                created_at: complete_through - 1,
                kind: 1,
            },
            crate::EventFact {
                id: [3; 32],
                created_at: complete_through,
                kind: 1,
            },
        ];
        let mut event_bytes = Vec::new();
        let mut content_bytes = Vec::new();
        for (index, event) in events.iter().enumerate() {
            event_bytes.extend_from_slice(&event.encode());
            content_bytes.extend_from_slice(
                &ContentFact {
                    id: event.id,
                    content_bytes: u64::try_from(index + 1).unwrap(),
                }
                .encode(),
            );
        }
        fs::write(&event_path, event_bytes).unwrap();
        fs::write(&content_path, content_bytes).unwrap();

        let derived = derive_compact_rows(&event_path, 3, &content_path, as_of).unwrap();
        assert_eq!(derived.rows, 3);
        assert_eq!(derived.hourly.len(), 2);
        assert_eq!(derived.hourly.values().copied().sum::<u64>(), 2);
        assert_eq!(
            derived
                .hourly
                .get(&(u32::try_from(complete_through / 3_600).unwrap() - 1, 0)),
            Some(&1)
        );
        assert_eq!(derived.kinds[1].event_count, 2);
        assert_eq!(derived.kinds[1].content_bytes, 5);
        assert_eq!(derived.kinds[1].content_rows, 2);
        assert_eq!(
            derived.kinds[1].first_seen,
            u32::try_from(complete_through - 1).unwrap()
        );
        assert_eq!(
            derived.kinds[1].last_seen,
            u32::try_from(complete_through).unwrap()
        );
        assert_eq!(derived.kinds[7].event_count, 0);
    }
}
