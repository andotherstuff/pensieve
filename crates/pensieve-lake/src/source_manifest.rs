//! Frozen historical-source manifests and completion accounting.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pensieve_parquet::SalvageReport;

use crate::{ActiveRawFragment, CatalogWorkUnit, Error, Result, WorkState, WorkUnitRecord};

/// Format identifier for the historical notepack source manifest.
pub const HISTORICAL_SOURCE_MANIFEST_FORMAT: &str = "pensieve.historical-source-manifest.v1";
/// Format identifier for explicit historical-source repair exceptions.
pub const HISTORICAL_SOURCE_EXCEPTIONS_FORMAT: &str = "pensieve.historical-source-exceptions.v1";

/// One exact source object selected for the bounded historical campaign.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct HistoricalSourceEntry {
    /// Logical segment number parsed from the canonical filename.
    pub segment_number: u64,
    /// Exact source object name, without a host-local path.
    pub source_name: String,
    /// Exact remote object bytes observed when the manifest was frozen.
    pub source_bytes: u64,
}

/// Aggregate source-manifest totals.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoricalSourceTotals {
    /// Number of selected logical source segments.
    pub sources: u64,
    /// Sum of selected source-object bytes.
    pub source_bytes: u64,
}

/// Durable publication evidence binding one manifest filename to content-addressed work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalSourceReceipt {
    /// Exact source filename represented by the receipt filename.
    pub source_name: String,
    /// Published content-addressed inventory work-unit ID.
    pub work_unit_id: String,
    /// SHA-256 of the source bytes.
    pub source_sha256: String,
    /// Source events observed by the converter.
    pub input_events: Option<u64>,
    /// Canonical rows written by the converter.
    pub output_rows: Option<u64>,
    /// Invalid frames quarantined by the converter.
    pub rejected_events: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct HistoricalSourcePayload {
    format: String,
    max_segment_number: u64,
    selection_high_water_gzip: u64,
    entries: Vec<HistoricalSourceEntry>,
    totals: HistoricalSourceTotals,
}

/// Content-addressed, immutable input universe for one historical campaign.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoricalSourceManifest {
    /// SHA-256 identity of the compact canonical payload.
    pub manifest_id: String,
    #[serde(flatten)]
    payload: HistoricalSourcePayload,
}

/// One damaged manifest source resolved through an immutable repair work unit.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct HistoricalSourceException {
    /// Exact original source name from the frozen manifest.
    pub source_name: String,
    /// Exact original source bytes from the frozen manifest and inventory.
    pub source_bytes: u64,
    /// SHA-256 observed when the original source failed conversion.
    pub source_sha256: String,
    /// Evidence report describing terminal truncation and retained bytes.
    pub salvage_report_id: String,
    /// SHA-256 of the complete framed prefix.
    pub salvaged_source_sha256: String,
    /// Published repair work unit created from that prefix.
    pub repair_work_unit_id: String,
    /// Complete original frames retained in the repair source.
    pub complete_frames: u64,
    /// Complete frames expected to enter quarantine.
    pub rejected_events: u64,
    /// Zero-based terminal truncated-frame index.
    pub truncated_frame_index: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct HistoricalSourceExceptionsPayload {
    format: String,
    manifest_id: String,
    entries: Vec<HistoricalSourceException>,
}

/// Content-addressed exception ledger for a frozen historical manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoricalSourceExceptions {
    /// SHA-256 identity of the compact canonical ledger payload.
    pub exceptions_id: String,
    #[serde(flatten)]
    payload: HistoricalSourceExceptionsPayload,
}

impl HistoricalSourceExceptions {
    /// Validated exceptions in source-name order.
    pub fn entries(&self) -> &[HistoricalSourceException] {
        &self.payload.entries
    }

    /// Frozen source-manifest identity covered by this ledger.
    pub fn manifest_id(&self) -> &str {
        &self.payload.manifest_id
    }

    /// Verify ordering, hashes, references, and content identity.
    pub fn validate(&self) -> Result<()> {
        if self.payload.format != HISTORICAL_SOURCE_EXCEPTIONS_FORMAT {
            return Err(Error::InvalidSourceManifest(format!(
                "unsupported exception-ledger format {}",
                self.payload.format
            )));
        }
        if self.payload.entries.is_empty() {
            return Err(Error::InvalidSourceManifest(
                "exception ledger has no entries".to_owned(),
            ));
        }
        let mut previous = None;
        for entry in &self.payload.entries {
            parse_source_name(&entry.source_name)?;
            if previous.is_some_and(|previous: &str| previous >= entry.source_name.as_str()) {
                return Err(Error::InvalidSourceManifest(
                    "exception entries are not strictly ordered by source name".to_owned(),
                ));
            }
            previous = Some(entry.source_name.as_str());
            for (field, value) in [
                ("source_sha256", entry.source_sha256.as_str()),
                (
                    "salvaged_source_sha256",
                    entry.salvaged_source_sha256.as_str(),
                ),
            ] {
                validate_sha256(field, value)?;
            }
            validate_sha256_id("salvage_report_id", &entry.salvage_report_id)?;
            let expected_work_id = format!("notepack-sha256-{}", entry.salvaged_source_sha256);
            if entry.repair_work_unit_id != expected_work_id {
                return Err(Error::InvalidSourceManifest(format!(
                    "repair work-unit identity mismatch for {}",
                    entry.source_name
                )));
            }
            if entry.complete_frames != entry.truncated_frame_index {
                return Err(Error::InvalidSourceManifest(format!(
                    "truncated frame does not follow the complete prefix for {}",
                    entry.source_name
                )));
            }
        }
        let expected = content_id(&self.payload)?;
        if self.exceptions_id != expected {
            return Err(Error::InvalidSourceManifest(format!(
                "exception-ledger identity mismatch: expected {expected}, found {}",
                self.exceptions_id
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RcloneLsJsonEntry {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "IsDir", default)]
    is_dir: bool,
}

#[derive(Default)]
struct Representations {
    plain: Option<HistoricalSourceEntry>,
    gzip: Option<HistoricalSourceEntry>,
}

impl HistoricalSourceManifest {
    /// Select a deterministic, bounded manifest from `rclone lsjson` output.
    pub fn from_rclone_lsjson(bytes: &[u8], max_segment_number: u64) -> Result<Self> {
        let remote_entries: Vec<RcloneLsJsonEntry> = serde_json::from_slice(bytes)?;
        let mut representations = BTreeMap::<u64, Representations>::new();
        let mut selection_high_water_gzip = None;

        for remote in remote_entries {
            if remote.is_dir {
                continue;
            }
            let (segment_number, compressed) = parse_source_name(&remote.path)?;
            let source_bytes = u64::try_from(remote.size).map_err(|_| {
                Error::InvalidSourceManifest(format!(
                    "source {} has a negative byte size",
                    remote.path
                ))
            })?;
            let entry = HistoricalSourceEntry {
                segment_number,
                source_name: remote.path,
                source_bytes,
            };
            let representation = representations.entry(segment_number).or_default();
            let slot = if compressed {
                selection_high_water_gzip = Some(
                    selection_high_water_gzip
                        .map_or(segment_number, |current: u64| current.max(segment_number)),
                );
                &mut representation.gzip
            } else {
                &mut representation.plain
            };
            match slot {
                Some(existing) if existing != &entry => {
                    return Err(Error::InvalidSourceManifest(format!(
                        "source representation conflicts with a duplicate listing: {}",
                        entry.source_name
                    )));
                }
                Some(_) => {}
                None => *slot = Some(entry),
            }
        }

        let selection_high_water_gzip = selection_high_water_gzip.ok_or_else(|| {
            Error::InvalidSourceManifest(
                "no sealed gzip segment exists to establish a source high-water mark".to_owned(),
            )
        })?;
        if max_segment_number > selection_high_water_gzip {
            return Err(Error::InvalidSourceManifest(format!(
                "historical boundary {max_segment_number} is above the visible gzip high-water \
                 mark {selection_high_water_gzip}"
            )));
        }
        let mut entries = Vec::new();
        for (segment_number, representation) in representations {
            if segment_number > max_segment_number {
                continue;
            }
            if let Some(gzip) = representation.gzip {
                entries.push(gzip);
            } else if segment_number < selection_high_water_gzip
                && let Some(plain) = representation.plain
            {
                entries.push(plain);
            }
        }
        if entries.is_empty() {
            return Err(Error::InvalidSourceManifest(
                "bounded source selection is empty".to_owned(),
            ));
        }

        let payload = HistoricalSourcePayload {
            format: HISTORICAL_SOURCE_MANIFEST_FORMAT.to_owned(),
            max_segment_number,
            selection_high_water_gzip,
            totals: source_totals(&entries)?,
            entries,
        };
        validate_payload(&payload)?;
        Ok(Self {
            manifest_id: content_id(&payload)?,
            payload,
        })
    }

    /// Inclusive historical segment boundary.
    pub fn max_segment_number(&self) -> u64 {
        self.payload.max_segment_number
    }

    /// Highest gzip segment visible when the manifest was frozen.
    pub fn selection_high_water_gzip(&self) -> u64 {
        self.payload.selection_high_water_gzip
    }

    /// Selected source objects in ascending segment-number order.
    pub fn entries(&self) -> &[HistoricalSourceEntry] {
        &self.payload.entries
    }

    /// Aggregate source totals.
    pub fn totals(&self) -> &HistoricalSourceTotals {
        &self.payload.totals
    }

    /// Verify structure, ordering, totals, boundary, and content identity.
    pub fn validate(&self) -> Result<()> {
        validate_payload(&self.payload)?;
        let expected = content_id(&self.payload)?;
        if self.manifest_id != expected {
            return Err(Error::InvalidSourceManifest(format!(
                "manifest identity mismatch: expected {expected}, found {}",
                self.manifest_id
            )));
        }
        Ok(())
    }
}

/// One completion-accounting defect.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CompletionProblem {
    /// Source filename or inventory identity involved.
    pub source_name: String,
    /// Operator-readable reason the completion gate is not satisfied.
    pub reason: String,
}

/// Aggregate campaign completion accounting.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionTotals {
    /// Frozen source-manifest entries.
    pub manifest_sources: u64,
    /// Manifest entries with durably published work.
    pub published_sources: u64,
    /// Published canonical rows.
    pub output_rows: u64,
    /// Invalid source frames recorded by published work.
    pub rejected_events: u64,
    /// Valid duplicate source events inferred from input accounting.
    pub duplicate_events: u64,
    /// Active raw Parquet objects selected by the inventory.
    pub active_raw_objects: u64,
    /// Active raw Parquet physical rows.
    pub active_raw_rows: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CompletionPayload {
    format: String,
    manifest_id: String,
    complete: bool,
    totals: CompletionTotals,
    problems: Vec<CompletionProblem>,
}

/// Deterministic completion report for one manifest and inventory snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoricalCompletionAudit {
    /// SHA-256 identity of the compact canonical report payload.
    pub audit_id: String,
    #[serde(flatten)]
    payload: CompletionPayload,
}

impl HistoricalCompletionAudit {
    /// Whether the frozen manifest is completely and consistently published.
    pub fn is_complete(&self) -> bool {
        self.payload.complete
    }

    /// Aggregate campaign accounting.
    pub fn totals(&self) -> &CompletionTotals {
        &self.payload.totals
    }

    /// Defects preventing a complete result.
    pub fn problems(&self) -> &[CompletionProblem] {
        &self.payload.problems
    }
}

/// Optional immutable evidence used to resolve non-standard source coverage.
#[derive(Clone, Copy, Debug, Default)]
pub struct HistoricalCompletionEvidence<'a> {
    /// Explicit terminal-truncation exception ledger.
    pub exceptions: Option<&'a HistoricalSourceExceptions>,
    /// Active catalog fragments containing repair work referenced by exceptions.
    pub repair_fragments: &'a [ActiveRawFragment],
    /// Durable per-filename receipts for content-addressed publication aliases.
    pub source_receipts: &'a [HistoricalSourceReceipt],
}

/// Compare a frozen source universe with the complete inventory and active catalog view.
pub fn audit_historical_completion(
    manifest: &HistoricalSourceManifest,
    work_units: &[WorkUnitRecord],
    active_work_unit_ids: &BTreeSet<String>,
    active_raw_objects: u64,
    active_raw_rows: u64,
    evidence: HistoricalCompletionEvidence<'_>,
) -> Result<HistoricalCompletionAudit> {
    let HistoricalCompletionEvidence {
        exceptions,
        repair_fragments,
        source_receipts,
    } = evidence;
    manifest.validate()?;
    if let Some(exceptions) = exceptions {
        exceptions.validate()?;
        if exceptions.manifest_id() != manifest.manifest_id {
            return Err(Error::InvalidSourceManifest(
                "exception ledger references a different source manifest".to_owned(),
            ));
        }
    } else if !repair_fragments.is_empty() {
        return Err(Error::InvalidSourceManifest(
            "repair fragments require an exception ledger".to_owned(),
        ));
    }
    let exception_by_source: BTreeMap<_, _> = exceptions
        .into_iter()
        .flat_map(HistoricalSourceExceptions::entries)
        .map(|entry| (entry.source_name.as_str(), entry))
        .collect();
    let mut repair_by_id = BTreeMap::<String, (&CatalogWorkUnit, u64)>::new();
    for fragment in repair_fragments {
        fragment.validate()?;
        for work in fragment.work_units() {
            let object_count = u64::try_from(
                fragment
                    .objects()
                    .iter()
                    .filter(|object| object.work_unit_id == work.work_unit_id)
                    .count(),
            )
            .map_err(|_| {
                Error::InvalidSourceManifest("repair object count exceeds u64".to_owned())
            })?;
            if let Some((existing, existing_objects)) =
                repair_by_id.insert(work.work_unit_id.clone(), (work, object_count))
                && (existing != work || existing_objects != object_count)
            {
                return Err(Error::InvalidSourceManifest(format!(
                    "conflicting repair coverage for {}",
                    work.work_unit_id
                )));
            }
        }
    }
    let manifest_names: BTreeSet<_> = manifest
        .entries()
        .iter()
        .map(|entry| entry.source_name.as_str())
        .collect();
    let mut by_source = BTreeMap::<String, Vec<&WorkUnitRecord>>::new();
    let by_id: BTreeMap<_, _> = work_units
        .iter()
        .map(|work| (work.id.as_str(), work))
        .collect();
    let mut problems = Vec::new();
    for work in work_units {
        let Some(source_name) = work
            .source_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            problems.push(CompletionProblem {
                source_name: work.id.clone(),
                reason: "inventory source path has no UTF-8 filename".to_owned(),
            });
            continue;
        };
        by_source.entry(source_name).or_default().push(work);
    }

    let mut receipt_by_source = BTreeMap::new();
    for receipt in source_receipts {
        if receipt.work_unit_id != format!("notepack-sha256-{}", receipt.source_sha256) {
            return Err(Error::InvalidSourceManifest(format!(
                "publication receipt for {} has inconsistent identity",
                receipt.source_name
            )));
        }
        let Some(work) = by_id.get(receipt.work_unit_id.as_str()) else {
            return Err(Error::InvalidSourceManifest(format!(
                "publication receipt for {} references absent work {}",
                receipt.source_name, receipt.work_unit_id
            )));
        };
        if work.source_sha256 != receipt.source_sha256
            || receipt
                .input_events
                .is_some_and(|count| work.input_events != count)
            || receipt
                .output_rows
                .is_some_and(|count| work.output_rows != count)
            || receipt
                .rejected_events
                .is_some_and(|count| work.rejected_events != count)
        {
            return Err(Error::InvalidSourceManifest(format!(
                "publication receipt for {} differs from inventory work {}",
                receipt.source_name, receipt.work_unit_id
            )));
        }
        if !matches!(
            work.state,
            WorkState::Published | WorkState::SourceCommitted
        ) {
            return Err(Error::InvalidSourceManifest(format!(
                "publication receipt for {} references {} work",
                receipt.source_name, work.state
            )));
        }
        if receipt_by_source
            .insert(receipt.source_name.as_str(), *work)
            .is_some()
        {
            return Err(Error::InvalidSourceManifest(format!(
                "multiple publication receipts cover {}",
                receipt.source_name
            )));
        }
    }

    let mut totals = CompletionTotals {
        manifest_sources: manifest.totals().sources,
        active_raw_objects,
        active_raw_rows,
        ..CompletionTotals::default()
    };
    let mut accounted_work_ids = BTreeSet::new();
    for entry in manifest.entries() {
        let work = match by_source.get(&entry.source_name) {
            Some(records) if records.len() == 1 => records[0],
            Some(records) => {
                problems.push(CompletionProblem {
                    source_name: entry.source_name.clone(),
                    reason: format!("source has {} inventory work units", records.len()),
                });
                continue;
            }
            None => {
                let Some(work) = receipt_by_source.get(entry.source_name.as_str()) else {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: "source has no inventory work unit or publication receipt"
                            .to_owned(),
                    });
                    continue;
                };
                *work
            }
        };
        if work.source_bytes != entry.source_bytes {
            problems.push(CompletionProblem {
                source_name: entry.source_name.clone(),
                reason: format!(
                    "source byte size differs: manifest={} inventory={}",
                    entry.source_bytes, work.source_bytes
                ),
            });
        }
        if !matches!(
            work.state,
            WorkState::Published | WorkState::SourceCommitted
        ) {
            if let Some(exception) = exception_by_source.get(entry.source_name.as_str()) {
                if exception.source_bytes != entry.source_bytes {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: "exception source bytes differ from frozen manifest".to_owned(),
                    });
                    continue;
                }
                if work.state != WorkState::Failed {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: format!(
                            "source exception requires failed original work, found {}",
                            work.state
                        ),
                    });
                    continue;
                }
                if work.source_sha256 != exception.source_sha256 {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: "failed source checksum differs from exception evidence".to_owned(),
                    });
                    continue;
                }
                let Some((repair, repair_objects)) =
                    repair_by_id.get(&exception.repair_work_unit_id)
                else {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: format!(
                            "repair work {} is absent from active repair fragments",
                            exception.repair_work_unit_id
                        ),
                    });
                    continue;
                };
                if repair.source_sha256 != exception.salvaged_source_sha256
                    || repair.input_events != exception.complete_frames
                    || repair.rejected_events != exception.rejected_events
                {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: "active repair work differs from exception evidence".to_owned(),
                    });
                    continue;
                }
                let Some(accounted) = repair.output_rows.checked_add(repair.rejected_events) else {
                    return Err(Error::InvalidSourceManifest(
                        "repair event accounting overflows".to_owned(),
                    ));
                };
                let Some(duplicate_events) = repair.input_events.checked_sub(accounted) else {
                    problems.push(CompletionProblem {
                        source_name: entry.source_name.clone(),
                        reason: "repair output plus rejects exceeds input".to_owned(),
                    });
                    continue;
                };
                add_published_source(&mut totals)?;
                if accounted_work_ids.insert(repair.work_unit_id.as_str()) {
                    add_work_totals(
                        &mut totals,
                        repair.output_rows,
                        repair.rejected_events,
                        duplicate_events,
                    )?;
                    totals.active_raw_rows = totals
                        .active_raw_rows
                        .checked_add(repair.output_rows)
                        .ok_or_else(|| {
                            Error::InvalidSourceManifest(
                                "active repair row count overflows".to_owned(),
                            )
                        })?;
                    totals.active_raw_objects = totals
                        .active_raw_objects
                        .checked_add(*repair_objects)
                        .ok_or_else(|| {
                            Error::InvalidSourceManifest(
                                "active repair object count overflows".to_owned(),
                            )
                        })?;
                }
                continue;
            }
            problems.push(CompletionProblem {
                source_name: entry.source_name.clone(),
                reason: format!("work unit is {}", work.state),
            });
            continue;
        }
        if !active_work_unit_ids.contains(&work.id) {
            problems.push(CompletionProblem {
                source_name: entry.source_name.clone(),
                reason: "published work is absent from the active-raw catalog view".to_owned(),
            });
            continue;
        }
        let Some(accounted) = work.output_rows.checked_add(work.rejected_events) else {
            return Err(Error::InvalidSourceManifest(format!(
                "event accounting overflows for {}",
                entry.source_name
            )));
        };
        let Some(duplicate_events) = work.input_events.checked_sub(accounted) else {
            problems.push(CompletionProblem {
                source_name: entry.source_name.clone(),
                reason: "output plus rejected events exceeds input events".to_owned(),
            });
            continue;
        };
        add_published_source(&mut totals)?;
        if accounted_work_ids.insert(work.id.as_str()) {
            add_work_totals(
                &mut totals,
                work.output_rows,
                work.rejected_events,
                duplicate_events,
            )?;
        }
    }

    for source_name in by_source.keys() {
        if !manifest_names.contains(source_name.as_str()) {
            problems.push(CompletionProblem {
                source_name: source_name.clone(),
                reason: "inventory work is outside the frozen historical manifest".to_owned(),
            });
        }
    }
    for source_name in exception_by_source.keys() {
        if !manifest_names.contains(source_name) {
            problems.push(CompletionProblem {
                source_name: (*source_name).to_owned(),
                reason: "exception source is outside the frozen historical manifest".to_owned(),
            });
        }
    }
    if totals.active_raw_rows != totals.output_rows {
        problems.push(CompletionProblem {
            source_name: "active-raw-catalog".to_owned(),
            reason: format!(
                "active raw row total differs from published output accounting: catalog={} \
                 inventory={}",
                totals.active_raw_rows, totals.output_rows
            ),
        });
    }
    problems.sort();
    let complete = problems.is_empty() && totals.published_sources == manifest.totals().sources;
    let payload = CompletionPayload {
        format: "pensieve.historical-completion-audit.v1".to_owned(),
        manifest_id: manifest.manifest_id.clone(),
        complete,
        totals,
        problems,
    };
    Ok(HistoricalCompletionAudit {
        audit_id: content_id(&payload)?,
        payload,
    })
}

fn add_published_source(totals: &mut CompletionTotals) -> Result<()> {
    totals.published_sources = totals.published_sources.checked_add(1).ok_or_else(|| {
        Error::InvalidSourceManifest("published source count overflows u64".to_owned())
    })?;
    Ok(())
}

fn add_work_totals(
    totals: &mut CompletionTotals,
    output_rows: u64,
    rejected_events: u64,
    duplicate_events: u64,
) -> Result<()> {
    totals.output_rows = totals
        .output_rows
        .checked_add(output_rows)
        .ok_or_else(|| Error::InvalidSourceManifest("output row count overflows u64".to_owned()))?;
    totals.rejected_events = totals
        .rejected_events
        .checked_add(rejected_events)
        .ok_or_else(|| {
            Error::InvalidSourceManifest("rejected event count overflows u64".to_owned())
        })?;
    totals.duplicate_events = totals
        .duplicate_events
        .checked_add(duplicate_events)
        .ok_or_else(|| {
            Error::InvalidSourceManifest("duplicate event count overflows u64".to_owned())
        })?;
    Ok(())
}

/// Read publication receipts from a campaign receipt directory in filename order.
pub fn read_historical_source_receipts(
    directory: impl AsRef<Path>,
) -> Result<Vec<HistoricalSourceReceipt>> {
    let mut paths = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    let mut receipts = Vec::new();
    for path in paths {
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            return Err(Error::InvalidSourceManifest(
                "publication receipt filename is not UTF-8".to_owned(),
            ));
        };
        let Some(source_name) = filename.strip_suffix(".published") else {
            continue;
        };
        let text = fs::read_to_string(&path)?;
        let fields: Vec<_> = text.split_whitespace().collect();
        if fields.len() != 2 && fields.len() != 5 {
            return Err(Error::InvalidSourceManifest(format!(
                "publication receipt {filename} must contain exactly two or five fields"
            )));
        }
        let parse_count = |field: &str, label: &str| {
            field.parse::<u64>().map_err(|_| {
                Error::InvalidSourceManifest(format!(
                    "publication receipt {filename} has invalid {label}"
                ))
            })
        };
        validate_sha256("publication receipt source checksum", fields[1])?;
        if fields[0] != format!("notepack-sha256-{}", fields[1]) {
            return Err(Error::InvalidSourceManifest(format!(
                "publication receipt {filename} has inconsistent identity"
            )));
        }
        receipts.push(HistoricalSourceReceipt {
            source_name: source_name.to_owned(),
            work_unit_id: fields[0].to_owned(),
            source_sha256: fields[1].to_owned(),
            input_events: fields
                .get(2)
                .map(|field| parse_count(field, "input event count"))
                .transpose()?,
            output_rows: fields
                .get(3)
                .map(|field| parse_count(field, "output row count"))
                .transpose()?,
            rejected_events: fields
                .get(4)
                .map(|field| parse_count(field, "rejected event count"))
                .transpose()?,
        });
    }
    Ok(receipts)
}

/// Read and fully validate a canonical source manifest.
pub fn read_historical_source_manifest(path: impl AsRef<Path>) -> Result<HistoricalSourceManifest> {
    let bytes = fs::read(path)?;
    let manifest: HistoricalSourceManifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    if bytes != canonical_json(&manifest)? {
        return Err(Error::InvalidSourceManifest(
            "source manifest JSON is not canonically encoded".to_owned(),
        ));
    }
    Ok(manifest)
}

/// Create a canonical manifest file without replacing an existing freeze.
pub fn write_historical_source_manifest_noclobber(
    path: impl AsRef<Path>,
    manifest: &HistoricalSourceManifest,
) -> Result<()> {
    manifest.validate()?;
    let path = path.as_ref();
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&canonical_json(manifest)?)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::InvalidSourceManifest(format!(
                "refusing to replace frozen source manifest {}",
                path.display()
            ))
        } else {
            Error::Io(error.error)
        }
    })?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Build an exception ledger from validated salvage and repair evidence.
pub fn historical_source_exceptions_from_salvage(
    manifest: &HistoricalSourceManifest,
    reports: &[SalvageReport],
    repair_fragment: &ActiveRawFragment,
) -> Result<HistoricalSourceExceptions> {
    manifest.validate()?;
    repair_fragment.validate()?;
    if reports.is_empty() {
        return Err(Error::InvalidSourceManifest(
            "at least one salvage report is required".to_owned(),
        ));
    }
    let mut entries = Vec::with_capacity(reports.len());
    for report in reports {
        report.validate().map_err(|error| {
            Error::InvalidSourceManifest(format!("invalid salvage report: {error}"))
        })?;
        let source = manifest
            .entries()
            .iter()
            .find(|entry| entry.source_name == report.source_name())
            .ok_or_else(|| {
                Error::InvalidSourceManifest(format!(
                    "salvage source {} is absent from the frozen manifest",
                    report.source_name()
                ))
            })?;
        if source.source_bytes != report.source_bytes() {
            return Err(Error::InvalidSourceManifest(format!(
                "salvage source bytes differ from the manifest for {}",
                report.source_name()
            )));
        }
        let repair_work_unit_id = format!("notepack-sha256-{}", report.salvaged_segment_sha256());
        let repair = repair_fragment
            .work_units()
            .iter()
            .find(|work| work.work_unit_id == repair_work_unit_id)
            .ok_or_else(|| {
                Error::InvalidSourceManifest(format!(
                    "repair fragment does not contain {repair_work_unit_id}"
                ))
            })?;
        validate_repair_accounting(report, repair)?;
        entries.push(HistoricalSourceException {
            source_name: source.source_name.clone(),
            source_bytes: source.source_bytes,
            source_sha256: report.source_sha256().to_owned(),
            salvage_report_id: report.report_id.clone(),
            salvaged_source_sha256: report.salvaged_segment_sha256().to_owned(),
            repair_work_unit_id,
            complete_frames: report.complete_frames(),
            rejected_events: report.rejected_events(),
            truncated_frame_index: report.truncated_frame_index(),
        });
    }
    entries.sort();
    let payload = HistoricalSourceExceptionsPayload {
        format: HISTORICAL_SOURCE_EXCEPTIONS_FORMAT.to_owned(),
        manifest_id: manifest.manifest_id.clone(),
        entries,
    };
    let exceptions = HistoricalSourceExceptions {
        exceptions_id: content_id(&payload)?,
        payload,
    };
    exceptions.validate()?;
    Ok(exceptions)
}

/// Build a one-entry exception ledger from validated salvage and repair evidence.
pub fn historical_source_exception_from_salvage(
    manifest: &HistoricalSourceManifest,
    report: &SalvageReport,
    repair_fragment: &ActiveRawFragment,
) -> Result<HistoricalSourceExceptions> {
    historical_source_exceptions_from_salvage(
        manifest,
        std::slice::from_ref(report),
        repair_fragment,
    )
}

/// Read and fully validate a canonical historical exception ledger.
pub fn read_historical_source_exceptions(
    path: impl AsRef<Path>,
) -> Result<HistoricalSourceExceptions> {
    let bytes = fs::read(path)?;
    let exceptions: HistoricalSourceExceptions = serde_json::from_slice(&bytes)?;
    exceptions.validate()?;
    if bytes != canonical_json(&exceptions)? {
        return Err(Error::InvalidSourceManifest(
            "exception-ledger JSON is not canonically encoded".to_owned(),
        ));
    }
    Ok(exceptions)
}

/// Create a canonical exception ledger without replacing existing evidence.
pub fn write_historical_source_exceptions_noclobber(
    path: impl AsRef<Path>,
    exceptions: &HistoricalSourceExceptions,
) -> Result<()> {
    exceptions.validate()?;
    write_json_noclobber(path.as_ref(), &canonical_json(exceptions)?)
}

fn parse_source_name(name: &str) -> Result<(u64, bool)> {
    if name.contains('/') || name.contains('\\') {
        return Err(Error::InvalidSourceManifest(format!(
            "source name is not a single path component: {name}"
        )));
    }
    let Some(rest) = name.strip_prefix("segment-") else {
        return Err(Error::InvalidSourceManifest(format!(
            "unexpected source name: {name}"
        )));
    };
    let (digits, compressed) = if let Some(digits) = rest.strip_suffix(".notepack.gz") {
        (digits, true)
    } else if let Some(digits) = rest.strip_suffix(".notepack") {
        (digits, false)
    } else {
        return Err(Error::InvalidSourceManifest(format!(
            "unexpected source name: {name}"
        )));
    };
    if digits.len() != 9 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::InvalidSourceManifest(format!(
            "source name does not use a nine-digit segment number: {name}"
        )));
    }
    let segment_number = digits.parse::<u64>().map_err(|_| {
        Error::InvalidSourceManifest(format!("invalid segment number in source name: {name}"))
    })?;
    Ok((segment_number, compressed))
}

fn validate_payload(payload: &HistoricalSourcePayload) -> Result<()> {
    if payload.format != HISTORICAL_SOURCE_MANIFEST_FORMAT {
        return Err(Error::InvalidSourceManifest(format!(
            "unsupported source manifest format {}",
            payload.format
        )));
    }
    if payload.entries.is_empty() {
        return Err(Error::InvalidSourceManifest(
            "source manifest has no entries".to_owned(),
        ));
    }
    if payload.max_segment_number > payload.selection_high_water_gzip {
        return Err(Error::InvalidSourceManifest(format!(
            "historical boundary {} is above the visible gzip high-water mark {}",
            payload.max_segment_number, payload.selection_high_water_gzip
        )));
    }
    let mut previous = None;
    for entry in &payload.entries {
        let (number, compressed) = parse_source_name(&entry.source_name)?;
        if number != entry.segment_number {
            return Err(Error::InvalidSourceManifest(format!(
                "source {} has inconsistent segment number {}",
                entry.source_name, entry.segment_number
            )));
        }
        if number > payload.max_segment_number {
            return Err(Error::InvalidSourceManifest(format!(
                "source {} crosses historical boundary {}",
                entry.source_name, payload.max_segment_number
            )));
        }
        if !compressed && number >= payload.selection_high_water_gzip {
            return Err(Error::InvalidSourceManifest(format!(
                "plain source {} is not below the gzip high-water mark",
                entry.source_name
            )));
        }
        if previous.is_some_and(|previous| previous >= number) {
            return Err(Error::InvalidSourceManifest(
                "source entries are not strictly ordered by segment number".to_owned(),
            ));
        }
        previous = Some(number);
    }
    if source_totals(&payload.entries)? != payload.totals {
        return Err(Error::InvalidSourceManifest(
            "source manifest totals do not match its entries".to_owned(),
        ));
    }
    Ok(())
}

fn source_totals(entries: &[HistoricalSourceEntry]) -> Result<HistoricalSourceTotals> {
    let sources = u64::try_from(entries.len()).map_err(|_| {
        Error::InvalidSourceManifest("source count cannot be represented as u64".to_owned())
    })?;
    let source_bytes = entries.iter().try_fold(0_u64, |total, entry| {
        total.checked_add(entry.source_bytes).ok_or_else(|| {
            Error::InvalidSourceManifest("source byte total overflows u64".to_owned())
        })
    })?;
    Ok(HistoricalSourceTotals {
        sources,
        source_bytes,
    })
}

fn validate_repair_accounting(report: &SalvageReport, repair: &CatalogWorkUnit) -> Result<()> {
    if repair.source_sha256 != report.salvaged_segment_sha256() {
        return Err(Error::InvalidSourceManifest(format!(
            "repair source checksum differs from salvage report for {}",
            report.source_name()
        )));
    }
    if repair.input_events != report.complete_frames() {
        return Err(Error::InvalidSourceManifest(format!(
            "repair input count differs from salvage report for {}",
            report.source_name()
        )));
    }
    if repair.rejected_events != report.rejected_events() {
        return Err(Error::InvalidSourceManifest(format!(
            "repair reject count differs from salvage report for {}",
            report.source_name()
        )));
    }
    let accounted = repair
        .output_rows
        .checked_add(repair.rejected_events)
        .ok_or_else(|| Error::InvalidSourceManifest("repair accounting overflows".to_owned()))?;
    if accounted > repair.input_events {
        return Err(Error::InvalidSourceManifest(format!(
            "repair output plus rejects exceeds input for {}",
            report.source_name()
        )));
    }
    Ok(())
}

fn write_json_noclobber(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::InvalidSourceManifest(format!(
                "refusing to replace frozen evidence {}",
                path.display()
            ))
        } else {
            Error::Io(error.error)
        }
    })?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn validate_sha256(field: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidSourceManifest(format!(
            "{field} is not lowercase SHA-256"
        )));
    }
    Ok(())
}

fn validate_sha256_id(field: &str, value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(Error::InvalidSourceManifest(format!(
            "{field} is not a SHA-256 content ID"
        )));
    };
    validate_sha256(field, digest)
}

fn canonical_json(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn content_id(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{Inventory, ObjectKind, ObjectRecord, ObjectState, WorkUnitRegistration};

    use super::*;

    fn lsjson(entries: &[(&str, i64)]) -> Vec<u8> {
        let entries: Vec<_> = entries
            .iter()
            .map(|(path, size)| {
                serde_json::json!({
                    "Path": path,
                    "Size": size,
                    "IsDir": false
                })
            })
            .collect();
        serde_json::to_vec(&entries).expect("lsjson")
    }

    #[test]
    fn selection_is_bounded_and_prefers_gzip() {
        let manifest = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[
                ("segment-000000000.notepack", 100),
                ("segment-000000000.notepack.gz", 50),
                ("segment-000000001.notepack", 101),
                ("segment-000000002.notepack.gz", 52),
                ("segment-000000003.notepack.gz", 53),
            ]),
            2,
        )
        .expect("manifest");

        assert_eq!(manifest.max_segment_number(), 2);
        assert_eq!(manifest.selection_high_water_gzip(), 3);
        assert_eq!(
            manifest
                .entries()
                .iter()
                .map(|entry| entry.source_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "segment-000000000.notepack.gz",
                "segment-000000001.notepack",
                "segment-000000002.notepack.gz"
            ]
        );
        assert_eq!(manifest.totals().source_bytes, 203);
    }

    #[test]
    fn gzip_representation_wins_at_high_water() {
        let manifest = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[
                ("segment-000000000.notepack.gz", 50),
                ("segment-000000001.notepack", 101),
                ("segment-000000001.notepack.gz", 51),
            ]),
            1,
        )
        .expect("manifest");
        assert_eq!(manifest.entries().len(), 2);
        assert_eq!(
            manifest.entries()[1].source_name,
            "segment-000000001.notepack.gz"
        );
    }

    #[test]
    fn boundary_cannot_exceed_visible_gzip_high_water() {
        let error = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[("segment-000000000.notepack.gz", 50)]),
            1,
        )
        .expect_err("unsafe boundary");
        assert!(error.to_string().contains("above the visible gzip"));
    }

    #[test]
    fn manifest_round_trip_requires_canonical_bytes_and_no_clobber() {
        let manifest = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[("segment-000000000.notepack.gz", 50)]),
            0,
        )
        .expect("manifest");
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("manifest.json");
        write_historical_source_manifest_noclobber(&path, &manifest).expect("write");
        assert_eq!(
            read_historical_source_manifest(&path).expect("read"),
            manifest
        );
        let error =
            write_historical_source_manifest_noclobber(&path, &manifest).expect_err("no clobber");
        assert!(error.to_string().contains("refusing to replace"));
    }

    #[test]
    fn audit_reports_missing_failed_and_unexpected_work() {
        let manifest = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[
                ("segment-000000000.notepack.gz", 50),
                ("segment-000000001.notepack.gz", 51),
            ]),
            1,
        )
        .expect("manifest");
        let published = work(
            "work-published",
            "segment-000000000.notepack.gz",
            50,
            WorkState::Published,
        );
        let unexpected = work(
            "work-unexpected",
            "segment-000000002.notepack.gz",
            52,
            WorkState::Failed,
        );
        let audit = audit_historical_completion(
            &manifest,
            &[published, unexpected],
            &BTreeSet::from(["work-published".to_owned()]),
            1,
            2,
            HistoricalCompletionEvidence::default(),
        )
        .expect("audit");

        assert!(!audit.is_complete());
        assert_eq!(audit.totals().published_sources, 1);
        assert_eq!(audit.problems().len(), 2);
        assert!(
            audit
                .problems()
                .iter()
                .any(|problem| problem.reason.contains("no inventory"))
        );
        assert!(
            audit
                .problems()
                .iter()
                .any(|problem| problem.reason.contains("outside"))
        );
    }

    #[test]
    fn publication_receipt_covers_content_identical_source_without_double_counting_rows() {
        let manifest = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[
                ("segment-000000000.notepack.gz", 50),
                ("segment-000000001.notepack.gz", 50),
            ]),
            1,
        )
        .expect("manifest");
        let sha = "11".repeat(32);
        let id = format!("notepack-sha256-{sha}");
        let published = work(
            &id,
            "segment-000000000.notepack.gz",
            50,
            WorkState::Published,
        );
        let receipt = HistoricalSourceReceipt {
            source_name: "segment-000000001.notepack.gz".to_owned(),
            work_unit_id: id.clone(),
            source_sha256: sha,
            input_events: Some(2),
            output_rows: Some(2),
            rejected_events: Some(0),
        };

        let audit = audit_historical_completion(
            &manifest,
            &[published],
            &BTreeSet::from([id]),
            1,
            2,
            HistoricalCompletionEvidence {
                source_receipts: &[receipt],
                ..HistoricalCompletionEvidence::default()
            },
        )
        .expect("audit");

        assert!(audit.is_complete());
        assert_eq!(audit.totals().published_sources, 2);
        assert_eq!(audit.totals().output_rows, 2);
        assert_eq!(audit.totals().active_raw_rows, 2);
    }

    #[test]
    fn legacy_two_field_publication_receipt_retains_strict_identity() {
        let directory = tempfile::tempdir().expect("receipt directory");
        let sha = "ab".repeat(32);
        fs::write(
            directory
                .path()
                .join("segment-000000001.notepack.gz.published"),
            format!("notepack-sha256-{sha} {sha}\n"),
        )
        .expect("write receipt");

        let receipts = read_historical_source_receipts(directory.path()).expect("read receipt");

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].source_name, "segment-000000001.notepack.gz");
        assert_eq!(receipts[0].source_sha256, sha);
        assert_eq!(receipts[0].input_events, None);
        assert_eq!(receipts[0].output_rows, None);
        assert_eq!(receipts[0].rejected_events, None);
    }

    #[test]
    fn terminal_truncation_exception_requires_and_accounts_for_active_repair() {
        let manifest = HistoricalSourceManifest::from_rclone_lsjson(
            &lsjson(&[("segment-000000000.notepack.gz", 50)]),
            0,
        )
        .expect("manifest");
        let mut failed = work(
            "original-work",
            "segment-000000000.notepack.gz",
            50,
            WorkState::Failed,
        );
        failed.source_sha256 = "bb".repeat(32);
        let repair_sha = "aa".repeat(32);
        let repair_id = format!("notepack-sha256-{repair_sha}");
        let mut repair_inventory = Inventory::open_in_memory().expect("repair inventory");
        repair_inventory
            .ensure_work_unit(&WorkUnitRegistration {
                id: &repair_id,
                source_path: Path::new("/repair/salvaged.notepack"),
                source_bytes: 40,
                source_sha256: &repair_sha,
                target_uncompressed_bytes: 1_000,
                max_event_bytes: 2_000,
                object_prefix: "nostr/v1",
                writer_version: "test",
            })
            .expect("register repair");
        repair_inventory
            .transition_work(&repair_id, WorkState::Writing, None)
            .expect("write repair");
        let object_key = format!("nostr/v1/raw/{repair_id}/part-00000.parquet");
        repair_inventory
            .record_validated_objects(
                &repair_id,
                2,
                1,
                1,
                &[ObjectRecord {
                    object_key: object_key.clone(),
                    work_unit_id: repair_id.clone(),
                    part_number: 0,
                    kind: ObjectKind::Parquet,
                    state: ObjectState::Validated,
                    local_path: PathBuf::from("/staging/repair.parquet"),
                    byte_size: 20,
                    sha256: "cc".repeat(32),
                    writer_version: "test".to_owned(),
                    row_count: 1,
                    min_created_at: Some(1),
                    max_created_at: Some(1),
                }],
            )
            .expect("validate repair");
        repair_inventory
            .transition_work(&repair_id, WorkState::Uploading, None)
            .expect("upload repair");
        repair_inventory
            .mark_object_uploaded(&object_key)
            .expect("mark uploaded");
        repair_inventory
            .transition_work(&repair_id, WorkState::Uploaded, None)
            .expect("finish upload");
        repair_inventory
            .activate_work_unit(&repair_id)
            .expect("activate repair");
        let repair_fragment =
            ActiveRawFragment::export(&mut repair_inventory, "repair", "s3://test")
                .expect("repair fragment");
        let payload = HistoricalSourceExceptionsPayload {
            format: HISTORICAL_SOURCE_EXCEPTIONS_FORMAT.to_owned(),
            manifest_id: manifest.manifest_id.clone(),
            entries: vec![HistoricalSourceException {
                source_name: "segment-000000000.notepack.gz".to_owned(),
                source_bytes: 50,
                source_sha256: "bb".repeat(32),
                salvage_report_id: format!("sha256:{}", "dd".repeat(32)),
                salvaged_source_sha256: repair_sha,
                repair_work_unit_id: repair_id,
                complete_frames: 2,
                rejected_events: 1,
                truncated_frame_index: 2,
            }],
        };
        let exceptions = HistoricalSourceExceptions {
            exceptions_id: content_id(&payload).expect("exception ID"),
            payload,
        };

        let audit = audit_historical_completion(
            &manifest,
            &[failed],
            &BTreeSet::new(),
            0,
            0,
            HistoricalCompletionEvidence {
                exceptions: Some(&exceptions),
                repair_fragments: &[repair_fragment],
                source_receipts: &[],
            },
        )
        .expect("completion audit");
        assert!(audit.is_complete());
        assert_eq!(audit.totals().published_sources, 1);
        assert_eq!(audit.totals().output_rows, 1);
        assert_eq!(audit.totals().rejected_events, 1);
        assert_eq!(audit.totals().active_raw_rows, 1);
    }

    fn work(id: &str, source_name: &str, source_bytes: u64, state: WorkState) -> WorkUnitRecord {
        WorkUnitRecord {
            id: id.to_owned(),
            source_path: PathBuf::from("/input").join(source_name),
            source_bytes,
            source_sha256: "11".repeat(32),
            target_uncompressed_bytes: 1,
            max_event_bytes: 1,
            object_prefix: "nostr/v1".to_owned(),
            writer_version: "test".to_owned(),
            state,
            input_events: 2,
            output_rows: 2,
            rejected_events: 0,
            error: None,
        }
    }
}
