//! Frozen historical-source manifests and completion accounting.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result, WorkState, WorkUnitRecord};

/// Format identifier for the historical notepack source manifest.
pub const HISTORICAL_SOURCE_MANIFEST_FORMAT: &str = "pensieve.historical-source-manifest.v1";

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

/// Compare a frozen source universe with the complete inventory and active catalog view.
pub fn audit_historical_completion(
    manifest: &HistoricalSourceManifest,
    work_units: &[WorkUnitRecord],
    active_work_unit_ids: &BTreeSet<String>,
    active_raw_objects: u64,
    active_raw_rows: u64,
) -> Result<HistoricalCompletionAudit> {
    manifest.validate()?;
    let manifest_names: BTreeSet<_> = manifest
        .entries()
        .iter()
        .map(|entry| entry.source_name.as_str())
        .collect();
    let mut by_source = BTreeMap::<String, Vec<&WorkUnitRecord>>::new();
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

    let mut totals = CompletionTotals {
        manifest_sources: manifest.totals().sources,
        active_raw_objects,
        active_raw_rows,
        ..CompletionTotals::default()
    };
    for entry in manifest.entries() {
        let Some(records) = by_source.get(&entry.source_name) else {
            problems.push(CompletionProblem {
                source_name: entry.source_name.clone(),
                reason: "source has no inventory work unit".to_owned(),
            });
            continue;
        };
        if records.len() != 1 {
            problems.push(CompletionProblem {
                source_name: entry.source_name.clone(),
                reason: format!("source has {} inventory work units", records.len()),
            });
            continue;
        }
        let work = records[0];
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
        totals.published_sources = totals.published_sources.checked_add(1).ok_or_else(|| {
            Error::InvalidSourceManifest("published source count overflows u64".to_owned())
        })?;
        totals.output_rows = totals
            .output_rows
            .checked_add(work.output_rows)
            .ok_or_else(|| {
                Error::InvalidSourceManifest("output row count overflows u64".to_owned())
            })?;
        totals.rejected_events = totals
            .rejected_events
            .checked_add(work.rejected_events)
            .ok_or_else(|| {
                Error::InvalidSourceManifest("rejected event count overflows u64".to_owned())
            })?;
        totals.duplicate_events = totals
            .duplicate_events
            .checked_add(duplicate_events)
            .ok_or_else(|| {
                Error::InvalidSourceManifest("duplicate event count overflows u64".to_owned())
            })?;
    }

    for source_name in by_source.keys() {
        if !manifest_names.contains(source_name.as_str()) {
            problems.push(CompletionProblem {
                source_name: source_name.clone(),
                reason: "inventory work is outside the frozen historical manifest".to_owned(),
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
