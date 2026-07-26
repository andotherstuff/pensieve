//! Resumable conversion and publication of one sealed notepack work unit.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use pensieve_parquet::{
    DEFAULT_MAX_EVENT_BYTES, RejectedFrame, partition_prepared_rows, prepare_canonical_events,
    scan_segment, validate_file, write_prepared, write_rejected_segment,
};
use sha2::{Digest, Sha256};

use crate::{
    Error, Inventory, ObjectKind, ObjectRecord, ObjectState, Publisher, Result, WorkState,
    WorkUnitRegistration,
};

/// Default target represented bytes for one raw Parquet object.
pub const DEFAULT_TARGET_UNCOMPRESSED_BYTES: usize = 512 * 1024 * 1024;

/// Operational configuration shared by historical and live notepack work units.
#[derive(Clone, Debug)]
pub struct CampaignConfig {
    /// Durable local staging root.
    pub staging_dir: PathBuf,
    /// Prefix prepended to immutable object keys.
    pub object_prefix: String,
    /// Target represented bytes per Parquet part.
    pub target_uncompressed_bytes: usize,
    /// Safety limit for one notepack frame.
    pub max_event_bytes: usize,
}

impl CampaignConfig {
    /// Construct defaults rooted at `staging_dir`.
    pub fn new(staging_dir: impl Into<PathBuf>) -> Self {
        Self {
            staging_dir: staging_dir.into(),
            object_prefix: "nostr/v1".to_owned(),
            target_uncompressed_bytes: DEFAULT_TARGET_UNCOMPRESSED_BYTES,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

/// Final or resumed state of one work-unit execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CampaignSummary {
    /// Content-derived work-unit identifier.
    pub work_unit_id: String,
    /// Durable state reached by this run.
    pub state: WorkState,
    /// Input notepack frames.
    pub input_events: u64,
    /// Canonical rows across all generated parts.
    pub output_rows: u64,
    /// Invalid frames retained in quarantine.
    pub rejected_events: u64,
    /// Canonical Parquet objects.
    pub parquet_objects: usize,
    /// Whether the work unit existed before this invocation.
    pub resumed: bool,
}

/// Compute lowercase SHA-256 over the exact bytes of a local file.
pub fn sha256_file(path: impl AsRef<Path>) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Convert, validate, publish, and atomically activate one sealed notepack input.
///
/// The exact source-object checksum is the stable work-unit identity. Repeating
/// this call resumes from durable inventory state and never activates a partial
/// object set.
pub fn run_notepack_work_unit(
    inventory: &mut Inventory,
    publisher: &dyn Publisher,
    input: impl AsRef<Path>,
    config: &CampaignConfig,
) -> Result<CampaignSummary> {
    let input = input.as_ref();
    let source_bytes = input.metadata()?.len();
    let source_sha256 = sha256_file(input)?;
    let work_unit_id = format!("notepack-sha256-{source_sha256}");
    let target =
        u64::try_from(config.target_uncompressed_bytes).map_err(|_| Error::NumericOutOfRange {
            field: "target_uncompressed_bytes",
        })?;
    let max_event_bytes =
        u64::try_from(config.max_event_bytes).map_err(|_| Error::NumericOutOfRange {
            field: "max_event_bytes",
        })?;
    let resumed = inventory.work_unit(&work_unit_id)?.is_some();
    let record = inventory.ensure_work_unit(&WorkUnitRegistration {
        id: &work_unit_id,
        source_path: input,
        source_bytes,
        source_sha256: &source_sha256,
        target_uncompressed_bytes: target,
        max_event_bytes,
        object_prefix: &config.object_prefix,
        writer_version: pensieve_parquet::IMPLEMENTATION_VERSION,
    })?;

    if matches!(
        record.state,
        WorkState::Published | WorkState::SourceCommitted
    ) {
        return summary(inventory, &work_unit_id, resumed);
    }

    let result = run_registered_work_unit(
        inventory,
        publisher,
        input,
        config,
        &work_unit_id,
        record.state,
    );
    if let Err(error) = &result
        && let Some(current) = inventory.work_unit(&work_unit_id)?
        && matches!(
            current.state,
            WorkState::Writing | WorkState::Validated | WorkState::Uploading | WorkState::Uploaded
        )
    {
        let _ =
            inventory.transition_work(&work_unit_id, WorkState::Failed, Some(&error.to_string()));
    }
    result?;
    summary(inventory, &work_unit_id, resumed)
}

fn run_registered_work_unit(
    inventory: &mut Inventory,
    publisher: &dyn Publisher,
    input: &Path,
    config: &CampaignConfig,
    work_unit_id: &str,
    initial_state: WorkState,
) -> Result<()> {
    let mut state = initial_state;
    if matches!(state, WorkState::Pending | WorkState::Failed) {
        inventory.transition_work(work_unit_id, WorkState::Writing, None)?;
        state = WorkState::Writing;
    }

    if state == WorkState::Writing {
        generate_and_record(inventory, input, config, work_unit_id)?;
        state = WorkState::Validated;
    }

    if state == WorkState::Validated {
        inventory.transition_work(work_unit_id, WorkState::Uploading, None)?;
        state = WorkState::Uploading;
    }

    if state == WorkState::Uploading {
        let objects = inventory.objects_for_work(work_unit_id)?;
        for object in objects {
            verify_local_object(&object)?;
            publisher.publish(
                &object.object_key,
                &object.local_path,
                object.byte_size,
                &object.sha256,
            )?;
            inventory.mark_object_uploaded(&object.object_key)?;
        }
        inventory.transition_work(work_unit_id, WorkState::Uploaded, None)?;
        state = WorkState::Uploaded;
    }

    if state == WorkState::Uploaded {
        inventory.activate_work_unit(work_unit_id)?;
    }
    Ok(())
}

fn generate_and_record(
    inventory: &mut Inventory,
    input: &Path,
    config: &CampaignConfig,
    work_unit_id: &str,
) -> Result<()> {
    let scan = scan_segment(input, config.max_event_bytes)?;
    let input_events = scan.events.len() + scan.rejected.len();
    let rows = prepare_canonical_events(scan.events);
    let work_dir = config.staging_dir.join(work_unit_id);
    fs::create_dir_all(&work_dir)?;

    let ranges = if rows.is_empty() {
        Vec::new()
    } else {
        partition_prepared_rows(&rows, config.target_uncompressed_bytes)?
    };
    let mut objects = Vec::with_capacity(ranges.len() + usize::from(!scan.rejected.is_empty()));
    for (part_number, range) in ranges.into_iter().enumerate() {
        let part_number = u32::try_from(part_number).map_err(|_| Error::NumericOutOfRange {
            field: "part_number",
        })?;
        let filename = format!("part-{part_number:05}.parquet");
        let local_path = work_dir.join(&filename);
        ensure_parquet_part(&local_path, &rows[range])?;
        let report = validate_file(&local_path)?;
        let sha256 = sha256_file(&local_path)?;
        let object_filename = format!("part-{part_number:05}-{sha256}.parquet");
        objects.push(ObjectRecord {
            object_key: object_key(&config.object_prefix, "raw", work_unit_id, &object_filename),
            work_unit_id: work_unit_id.to_owned(),
            part_number,
            kind: ObjectKind::Parquet,
            state: ObjectState::Validated,
            local_path: local_path.clone(),
            byte_size: local_path.metadata()?.len(),
            sha256,
            writer_version: pensieve_parquet::IMPLEMENTATION_VERSION.to_owned(),
            row_count: to_u64(report.rows, "row_count")?,
            min_created_at: report.min_created_at,
            max_created_at: report.max_created_at,
        });
    }

    if !scan.rejected.is_empty() {
        let filename = "rejects.notepack.gz";
        let local_path = work_dir.join(filename);
        ensure_reject_part(&local_path, &scan.rejected)?;
        let sha256 = sha256_file(&local_path)?;
        let object_filename = format!("rejects-{sha256}.notepack.gz");
        objects.push(ObjectRecord {
            object_key: object_key(
                &config.object_prefix,
                "quarantine",
                work_unit_id,
                &object_filename,
            ),
            work_unit_id: work_unit_id.to_owned(),
            part_number: 0,
            kind: ObjectKind::Reject,
            state: ObjectState::Validated,
            local_path: local_path.clone(),
            byte_size: local_path.metadata()?.len(),
            sha256,
            writer_version: pensieve_parquet::IMPLEMENTATION_VERSION.to_owned(),
            row_count: 0,
            min_created_at: None,
            max_created_at: None,
        });
    }

    inventory.record_validated_objects(
        work_unit_id,
        to_u64(input_events, "input_events")?,
        to_u64(rows.len(), "output_rows")?,
        to_u64(scan.rejected.len(), "rejected_events")?,
        &objects,
    )
}

fn ensure_parquet_part(path: &Path, rows: &[pensieve_parquet::CanonicalEvent]) -> Result<()> {
    let parent = path.parent().expect("part path has a parent");
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    write_prepared(temporary.as_file_mut(), rows)?;
    temporary.as_file_mut().sync_all()?;
    validate_file(temporary.path())?;
    reconcile_generated_file(temporary, path)
}

fn ensure_reject_part(path: &Path, rejected: &[RejectedFrame]) -> Result<()> {
    let parent = path.parent().expect("reject path has a parent");
    let candidate_dir = tempfile::Builder::new()
        .prefix(".reject-candidate-")
        .tempdir_in(parent)?;
    let candidate = candidate_dir.path().join("rejects.notepack.gz");
    write_rejected_segment(&candidate, rejected)?;
    let candidate_bytes = candidate.metadata()?.len();
    let candidate_sha256 = sha256_file(&candidate)?;
    if path.exists() {
        if path.metadata()?.len() == candidate_bytes && sha256_file(path)? == candidate_sha256 {
            return Ok(());
        }
        fs::remove_file(path)?;
    }
    fs::rename(&candidate, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn reconcile_generated_file(temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    let temporary_bytes = temporary.as_file().metadata()?.len();
    let temporary_sha256 = sha256_file(temporary.path())?;
    if path.exists() {
        if path.metadata()?.len() == temporary_bytes && sha256_file(path)? == temporary_sha256 {
            return Ok(());
        }
        fs::remove_file(path)?;
    }
    temporary
        .persist_noclobber(path)
        .map_err(|error| Error::Io(error.error))?;
    File::open(path.parent().expect("generated path has parent"))?.sync_all()?;
    Ok(())
}

fn verify_local_object(object: &ObjectRecord) -> Result<()> {
    if object.local_path.metadata()?.len() != object.byte_size
        || sha256_file(&object.local_path)? != object.sha256
    {
        return Err(Error::ArtifactMismatch {
            path: object.local_path.clone(),
        });
    }
    if object.kind == ObjectKind::Parquet {
        let report = validate_file(&object.local_path)?;
        if to_u64(report.rows, "row_count")? != object.row_count
            || report.min_created_at != object.min_created_at
            || report.max_created_at != object.max_created_at
        {
            return Err(Error::ArtifactMismatch {
                path: object.local_path.clone(),
            });
        }
    }
    Ok(())
}

fn object_key(prefix: &str, class: &str, work_unit_id: &str, filename: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{class}/{work_unit_id}/{filename}")
    } else {
        format!("{prefix}/{class}/{work_unit_id}/{filename}")
    }
}

fn summary(inventory: &Inventory, work_unit_id: &str, resumed: bool) -> Result<CampaignSummary> {
    let work = inventory
        .work_unit(work_unit_id)?
        .expect("registered work unit exists");
    let parquet_objects = inventory
        .objects_for_work(work_unit_id)?
        .into_iter()
        .filter(|object| object.kind == ObjectKind::Parquet)
        .count();
    Ok(CampaignSummary {
        work_unit_id: work_unit_id.to_owned(),
        state: work.state,
        input_events: work.input_events,
        output_rows: work.output_rows,
        rejected_events: work.rejected_events,
        parquet_objects,
        resumed,
    })
}

fn to_u64(value: usize, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::NumericOutOfRange { field })
}
