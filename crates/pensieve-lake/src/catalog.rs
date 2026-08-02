//! Portable, deterministic snapshots of active raw lake objects.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Inventory, ObjectRecord, Result, WorkUnitRecord};

/// Format identifier for V1 active-raw fragments and snapshots.
pub const ACTIVE_RAW_CATALOG_FORMAT: &str = "pensieve.active-raw-catalog.v1";

/// Published source-work coverage recorded in a catalog.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CatalogWorkUnit {
    /// Content-derived work-unit identifier.
    pub work_unit_id: String,
    /// Source filename for operator traceability, without its host-local path.
    pub source_name: String,
    /// Exact source bytes.
    pub source_bytes: u64,
    /// Lowercase SHA-256 of the source.
    pub source_sha256: String,
    /// Target represented bytes used by the writer.
    pub target_uncompressed_bytes: u64,
    /// Maximum accepted source frame bytes.
    pub max_event_bytes: u64,
    /// Object-key namespace used for this work.
    pub object_prefix: String,
    /// Writer implementation identity.
    pub writer_version: String,
    /// Source frames observed.
    pub input_events: u64,
    /// Canonical rows emitted.
    pub output_rows: u64,
    /// Source frames quarantined.
    pub rejected_events: u64,
}

/// One immutable active raw Parquet object.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CatalogObject {
    /// Immutable key in the configured object store.
    pub object_key: String,
    /// Owning work unit.
    pub work_unit_id: String,
    /// Deterministic zero-based part number.
    pub part_number: u32,
    /// Exact object bytes.
    pub byte_size: u64,
    /// Lowercase SHA-256 of the object.
    pub sha256: String,
    /// Writer implementation identity.
    pub writer_version: String,
    /// Physical rows in this object.
    pub row_count: u64,
    /// Unsigned minimum event timestamp as decimal text.
    pub min_created_at: Option<String>,
    /// Unsigned maximum event timestamp as decimal text.
    pub max_created_at: Option<String>,
}

/// Aggregate physical totals for a fragment or snapshot.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogTotals {
    /// Published source work units, including zero-output inputs.
    pub work_units: u64,
    /// Active raw Parquet objects.
    pub objects: u64,
    /// Sum of immutable object bytes.
    pub object_bytes: u64,
    /// Sum of physical rows; this is not a unique-event count.
    pub physical_rows: u64,
    /// Sum of quarantined source frames across covered work units.
    pub rejected_events: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FragmentPayload {
    format: String,
    store_id: String,
    inventory_id: String,
    object_class: String,
    deduplicated_by_event_id: bool,
    work_units: Vec<CatalogWorkUnit>,
    objects: Vec<CatalogObject>,
    totals: CatalogTotals,
}

/// Portable export from one writer's local inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveRawFragment {
    /// SHA-256 content identity of the canonical fragment payload.
    pub fragment_id: String,
    #[serde(flatten)]
    payload: FragmentPayload,
}

impl ActiveRawFragment {
    /// Export the current published/active view from one inventory.
    pub fn export(
        inventory: &mut Inventory,
        inventory_id: impl Into<String>,
        store_id: impl Into<String>,
    ) -> Result<Self> {
        let (work_units, objects) = inventory.active_raw_catalog_records()?;
        let work_units = work_units.into_iter().map(CatalogWorkUnit::from).collect();
        let objects = objects.into_iter().map(CatalogObject::from).collect();
        Self::from_records(inventory_id.into(), store_id.into(), work_units, objects)
    }

    fn from_records(
        inventory_id: String,
        store_id: String,
        mut work_units: Vec<CatalogWorkUnit>,
        mut objects: Vec<CatalogObject>,
    ) -> Result<Self> {
        validate_label("inventory_id", &inventory_id)?;
        validate_label("store_id", &store_id)?;
        work_units.sort();
        objects.sort_by(|left, right| left.object_key.cmp(&right.object_key));
        let totals = catalog_totals(&work_units, &objects)?;
        let payload = FragmentPayload {
            format: ACTIVE_RAW_CATALOG_FORMAT.to_owned(),
            store_id,
            inventory_id,
            object_class: "active_raw".to_owned(),
            deduplicated_by_event_id: false,
            work_units,
            objects,
            totals,
        };
        validate_fragment_payload(&payload)?;
        let fragment_id = content_id(&payload)?;
        Ok(Self {
            fragment_id,
            payload,
        })
    }

    /// Stable operator-defined identity for the exporting inventory.
    pub fn inventory_id(&self) -> &str {
        &self.payload.inventory_id
    }

    /// Stable non-secret identity for the shared object store.
    pub fn store_id(&self) -> &str {
        &self.payload.store_id
    }

    /// Published source work included in this fragment.
    pub fn work_units(&self) -> &[CatalogWorkUnit] {
        &self.payload.work_units
    }

    /// Active raw objects included in this fragment.
    pub fn objects(&self) -> &[CatalogObject] {
        &self.payload.objects
    }

    /// Aggregate physical totals.
    pub fn totals(&self) -> &CatalogTotals {
        &self.payload.totals
    }

    /// Verify structure, ordering, references, totals, and content identity.
    pub fn validate(&self) -> Result<()> {
        validate_fragment_payload(&self.payload)?;
        validate_content_id("fragment", &self.fragment_id, &self.payload)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct SnapshotSource {
    inventory_id: String,
    fragment_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SnapshotPayload {
    format: String,
    store_id: String,
    object_class: String,
    deduplicated_by_event_id: bool,
    sources: Vec<SnapshotSource>,
    work_units: Vec<CatalogWorkUnit>,
    objects: Vec<CatalogObject>,
    totals: CatalogTotals,
}

/// Deterministic union of one or more active-raw inventory fragments.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveRawSnapshot {
    /// SHA-256 content identity of the canonical snapshot payload.
    pub snapshot_id: String,
    #[serde(flatten)]
    payload: SnapshotPayload,
}

impl ActiveRawSnapshot {
    /// Stable non-secret identity for the shared object store.
    pub fn store_id(&self) -> &str {
        &self.payload.store_id
    }

    /// Active raw objects included in this snapshot.
    pub fn objects(&self) -> &[CatalogObject] {
        &self.payload.objects
    }

    /// Published work-unit coverage included in this snapshot.
    pub fn work_units(&self) -> &[CatalogWorkUnit] {
        &self.payload.work_units
    }

    /// Aggregate physical totals.
    pub fn totals(&self) -> &CatalogTotals {
        &self.payload.totals
    }

    /// Verify structure, ordering, references, totals, and content identity.
    pub fn validate(&self) -> Result<()> {
        validate_snapshot_payload(&self.payload)?;
        validate_content_id("snapshot", &self.snapshot_id, &self.payload)
    }
}

/// Merge independently exported inventory fragments into one active-file snapshot.
pub fn merge_active_raw_fragments(
    fragments: impl IntoIterator<Item = ActiveRawFragment>,
) -> Result<ActiveRawSnapshot> {
    let mut fragments: Vec<_> = fragments.into_iter().collect();
    if fragments.is_empty() {
        return Err(Error::InvalidCatalog(
            "at least one inventory fragment is required".to_owned(),
        ));
    }
    for fragment in &fragments {
        fragment.validate()?;
    }
    fragments.sort_by(|left, right| {
        left.inventory_id()
            .cmp(right.inventory_id())
            .then(left.fragment_id.cmp(&right.fragment_id))
    });

    let store_id = fragments[0].store_id().to_owned();
    let mut sources = BTreeMap::<String, String>::new();
    let mut work_units = BTreeMap::<String, CatalogWorkUnit>::new();
    let mut work_object_sets = BTreeMap::<String, BTreeSet<String>>::new();
    let mut objects = BTreeMap::<String, CatalogObject>::new();
    let mut parts = BTreeMap::<(String, u32), String>::new();

    for fragment in fragments {
        if fragment.store_id() != store_id {
            return Err(Error::InvalidCatalog(format!(
                "fragment {} uses store {}, expected {}",
                fragment.fragment_id,
                fragment.store_id(),
                store_id
            )));
        }
        match sources.get(fragment.inventory_id()) {
            Some(existing) if existing != &fragment.fragment_id => {
                return Err(Error::InvalidCatalog(format!(
                    "inventory {} appears with conflicting fragment identities",
                    fragment.inventory_id()
                )));
            }
            _ => {
                sources.insert(
                    fragment.inventory_id().to_owned(),
                    fragment.fragment_id.clone(),
                );
            }
        }

        let fragment_object_sets = object_sets(fragment.objects());
        for work_unit in fragment.work_units() {
            if let Some(existing) = work_units.get(&work_unit.work_unit_id) {
                if work_object_sets
                    .get(&work_unit.work_unit_id)
                    .cloned()
                    .unwrap_or_default()
                    != fragment_object_sets
                        .get(&work_unit.work_unit_id)
                        .cloned()
                        .unwrap_or_default()
                    || !same_content_work(existing, work_unit)
                {
                    return Err(Error::InvalidCatalog(format!(
                        "work unit {} has conflicting records across fragments",
                        work_unit.work_unit_id
                    )));
                }
                if work_unit.source_name < existing.source_name {
                    work_units.insert(work_unit.work_unit_id.clone(), work_unit.clone());
                }
            } else {
                work_units.insert(work_unit.work_unit_id.clone(), work_unit.clone());
                work_object_sets.insert(
                    work_unit.work_unit_id.clone(),
                    fragment_object_sets
                        .get(&work_unit.work_unit_id)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        }
        for object in fragment.objects() {
            let part = (object.work_unit_id.clone(), object.part_number);
            if let Some(existing_key) = parts.get(&part) {
                if existing_key != &object.object_key {
                    return Err(Error::InvalidCatalog(format!(
                        "work unit {} part {} maps to conflicting object keys",
                        object.work_unit_id, object.part_number
                    )));
                }
            } else {
                parts.insert(part, object.object_key.clone());
            }
            if let Some(existing) = objects.get(&object.object_key) {
                if existing != object {
                    return Err(Error::InvalidCatalog(format!(
                        "object key {} has conflicting metadata",
                        object.object_key
                    )));
                }
            } else {
                objects.insert(object.object_key.clone(), object.clone());
            }
        }
    }

    let work_units: Vec<_> = work_units.into_values().collect();
    let objects: Vec<_> = objects.into_values().collect();
    let payload = SnapshotPayload {
        format: ACTIVE_RAW_CATALOG_FORMAT.to_owned(),
        store_id,
        object_class: "active_raw".to_owned(),
        deduplicated_by_event_id: false,
        sources: sources
            .into_iter()
            .map(|(inventory_id, fragment_id)| SnapshotSource {
                inventory_id,
                fragment_id,
            })
            .collect(),
        totals: catalog_totals(&work_units, &objects)?,
        work_units,
        objects,
    };
    validate_snapshot_payload(&payload)?;
    let snapshot_id = content_id(&payload)?;
    Ok(ActiveRawSnapshot {
        snapshot_id,
        payload,
    })
}

fn same_content_work(left: &CatalogWorkUnit, right: &CatalogWorkUnit) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.source_name.clear();
    right.source_name.clear();
    left == right
}

/// Read and validate one inventory fragment.
pub fn read_catalog_fragment(path: impl AsRef<Path>) -> Result<ActiveRawFragment> {
    let bytes = fs::read(path)?;
    let fragment = serde_json::from_slice(&bytes)?;
    ActiveRawFragment::validate(&fragment)?;
    validate_canonical_json(&bytes, &fragment)?;
    Ok(fragment)
}

/// Read and validate one merged active-file snapshot.
pub fn read_catalog_snapshot(path: impl AsRef<Path>) -> Result<ActiveRawSnapshot> {
    let bytes = fs::read(path)?;
    let snapshot = serde_json::from_slice(&bytes)?;
    ActiveRawSnapshot::validate(&snapshot)?;
    validate_canonical_json(&bytes, &snapshot)?;
    Ok(snapshot)
}

/// Atomically replace a catalog JSON file with canonical pretty-printed bytes.
pub fn write_catalog_atomically<T: Serialize>(path: impl AsRef<Path>, value: &T) -> Result<()> {
    let path = path.as_ref();
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&canonical_json(value)?)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn canonical_json(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn validate_canonical_json(bytes: &[u8], value: &impl Serialize) -> Result<()> {
    if bytes != canonical_json(value)? {
        return Err(Error::InvalidCatalog(
            "catalog JSON is valid but not canonically encoded".to_owned(),
        ));
    }
    Ok(())
}

fn validate_fragment_payload(payload: &FragmentPayload) -> Result<()> {
    validate_common(
        &payload.format,
        &payload.store_id,
        &payload.object_class,
        payload.deduplicated_by_event_id,
        &payload.work_units,
        &payload.objects,
        &payload.totals,
    )?;
    validate_label("inventory_id", &payload.inventory_id)
}

fn validate_snapshot_payload(payload: &SnapshotPayload) -> Result<()> {
    validate_common(
        &payload.format,
        &payload.store_id,
        &payload.object_class,
        payload.deduplicated_by_event_id,
        &payload.work_units,
        &payload.objects,
        &payload.totals,
    )?;
    if payload.sources.is_empty() {
        return Err(Error::InvalidCatalog(
            "snapshot has no source fragments".to_owned(),
        ));
    }
    ensure_sorted_unique(&payload.sources, "snapshot sources")?;
    let inventory_ids: BTreeSet<_> = payload
        .sources
        .iter()
        .map(|source| source.inventory_id.as_str())
        .collect();
    if inventory_ids.len() != payload.sources.len() {
        return Err(Error::InvalidCatalog(
            "snapshot source inventory identifiers are not unique".to_owned(),
        ));
    }
    for source in &payload.sources {
        validate_label("source inventory_id", &source.inventory_id)?;
        validate_sha256_id("source fragment_id", &source.fragment_id)?;
    }
    Ok(())
}

fn validate_common(
    format: &str,
    store_id: &str,
    object_class: &str,
    deduplicated_by_event_id: bool,
    work_units: &[CatalogWorkUnit],
    objects: &[CatalogObject],
    totals: &CatalogTotals,
) -> Result<()> {
    if format != ACTIVE_RAW_CATALOG_FORMAT {
        return Err(Error::InvalidCatalog(format!(
            "unsupported format {format}"
        )));
    }
    validate_label("store_id", store_id)?;
    if object_class != "active_raw" {
        return Err(Error::InvalidCatalog(format!(
            "unsupported object class {object_class}"
        )));
    }
    if deduplicated_by_event_id {
        return Err(Error::InvalidCatalog(
            "V1 active-raw catalogs cannot claim event-ID deduplication".to_owned(),
        ));
    }
    ensure_sorted_unique(work_units, "work units")?;
    ensure_sorted_object_keys(objects)?;
    let work_by_id: BTreeMap<_, _> = work_units
        .iter()
        .map(|work| (work.work_unit_id.as_str(), work))
        .collect();
    if work_by_id.len() != work_units.len() {
        return Err(Error::InvalidCatalog(
            "work-unit identifiers are not unique".to_owned(),
        ));
    }
    let mut parts = BTreeSet::new();
    let mut work_parts = BTreeMap::<&str, BTreeSet<u32>>::new();
    let mut work_rows = BTreeMap::<&str, u64>::new();
    for work in work_units {
        validate_label("work_unit_id", &work.work_unit_id)?;
        validate_label("source_name", &work.source_name)?;
        validate_label("object_prefix", &work.object_prefix)?;
        validate_label("work-unit writer_version", &work.writer_version)?;
        validate_sha256_hex("source_sha256", &work.source_sha256)?;
        if work.source_name.contains('/') || work.source_name.contains('\\') {
            return Err(Error::InvalidCatalog(format!(
                "source_name contains a path separator: {}",
                work.source_name
            )));
        }
        if work
            .output_rows
            .checked_add(work.rejected_events)
            .is_none_or(|accounted| accounted > work.input_events)
        {
            return Err(Error::InvalidCatalog(format!(
                "work unit {} accounts for more rows and rejects than its inputs",
                work.work_unit_id
            )));
        }
    }
    for object in objects {
        validate_label("object_key", &object.object_key)?;
        validate_label("object writer_version", &object.writer_version)?;
        validate_sha256_hex("object sha256", &object.sha256)?;
        let Some(work) = work_by_id.get(object.work_unit_id.as_str()) else {
            return Err(Error::InvalidCatalog(format!(
                "object {} references absent work unit {}",
                object.object_key, object.work_unit_id
            )));
        };
        if object.writer_version != work.writer_version {
            return Err(Error::InvalidCatalog(format!(
                "object {} has a different writer identity than work unit {}",
                object.object_key, object.work_unit_id
            )));
        }
        let expected_prefix = format!("{}/", work.object_prefix.trim_end_matches('/'));
        if !object.object_key.starts_with(&expected_prefix) {
            return Err(Error::InvalidCatalog(format!(
                "object {} is outside work unit {} prefix {}",
                object.object_key, object.work_unit_id, work.object_prefix
            )));
        }
        if !parts.insert((&object.work_unit_id, object.part_number)) {
            return Err(Error::InvalidCatalog(format!(
                "work unit {} has duplicate part {}",
                object.work_unit_id, object.part_number
            )));
        }
        work_parts
            .entry(&object.work_unit_id)
            .or_default()
            .insert(object.part_number);
        let rows = work_rows.entry(&object.work_unit_id).or_default();
        *rows = rows.checked_add(object.row_count).ok_or_else(|| {
            Error::InvalidCatalog(format!(
                "work unit {} object row count overflows u64",
                object.work_unit_id
            ))
        })?;
        validate_range(object)?;
    }
    for work in work_units {
        let object_rows = work_rows
            .get(work.work_unit_id.as_str())
            .copied()
            .unwrap_or_default();
        if object_rows != work.output_rows {
            return Err(Error::InvalidCatalog(format!(
                "work unit {} reports {} output rows but its objects contain {}",
                work.work_unit_id, work.output_rows, object_rows
            )));
        }
        if let Some(part_numbers) = work_parts.get(work.work_unit_id.as_str()) {
            for (expected, actual) in (0_u32..).zip(part_numbers) {
                if expected != *actual {
                    return Err(Error::InvalidCatalog(format!(
                        "work unit {} object parts are not contiguous from zero",
                        work.work_unit_id
                    )));
                }
            }
        }
    }
    if &catalog_totals(work_units, objects)? != totals {
        return Err(Error::InvalidCatalog(
            "catalog totals do not match its records".to_owned(),
        ));
    }
    Ok(())
}

fn validate_range(object: &CatalogObject) -> Result<()> {
    let min = object
        .min_created_at
        .as_deref()
        .map(parse_canonical_u64)
        .transpose()?;
    let max = object
        .max_created_at
        .as_deref()
        .map(parse_canonical_u64)
        .transpose()?;
    if object.row_count == 0 && (min.is_some() || max.is_some()) {
        return Err(Error::InvalidCatalog(format!(
            "empty object {} has an event-time range",
            object.object_key
        )));
    }
    if object.row_count > 0 && (min.is_none() || max.is_none()) {
        return Err(Error::InvalidCatalog(format!(
            "non-empty object {} lacks a complete event-time range",
            object.object_key
        )));
    }
    if min.zip(max).is_some_and(|(min, max)| min > max) {
        return Err(Error::InvalidCatalog(format!(
            "object {} has an inverted event-time range",
            object.object_key
        )));
    }
    Ok(())
}

fn parse_canonical_u64(value: &str) -> Result<u64> {
    let parsed = value.parse::<u64>().map_err(|_| {
        Error::InvalidCatalog(format!("invalid unsigned decimal timestamp {value}"))
    })?;
    if parsed.to_string() != value {
        return Err(Error::InvalidCatalog(format!(
            "non-canonical unsigned decimal timestamp {value}"
        )));
    }
    Ok(parsed)
}

fn catalog_totals(
    work_units: &[CatalogWorkUnit],
    objects: &[CatalogObject],
) -> Result<CatalogTotals> {
    Ok(CatalogTotals {
        work_units: usize_to_u64(work_units.len(), "work-unit count")?,
        objects: usize_to_u64(objects.len(), "object count")?,
        object_bytes: checked_sum(
            objects.iter().map(|object| object.byte_size),
            "object byte total",
        )?,
        physical_rows: checked_sum(
            objects.iter().map(|object| object.row_count),
            "physical row total",
        )?,
        rejected_events: checked_sum(
            work_units.iter().map(|work| work.rejected_events),
            "rejected event total",
        )?,
    })
}

fn checked_sum(mut values: impl Iterator<Item = u64>, field: &str) -> Result<u64> {
    values.try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| Error::InvalidCatalog(format!("{field} overflows u64")))
    })
}

fn usize_to_u64(value: usize, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::InvalidCatalog(format!("{field} cannot be represented as u64")))
}

fn object_sets(objects: &[CatalogObject]) -> BTreeMap<String, BTreeSet<String>> {
    let mut result = BTreeMap::<String, BTreeSet<String>>::new();
    for object in objects {
        result
            .entry(object.work_unit_id.clone())
            .or_default()
            .insert(object.object_key.clone());
    }
    result
}

fn ensure_sorted_unique<T: Ord>(values: &[T], label: &str) -> Result<()> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::InvalidCatalog(format!(
            "{label} are not strictly sorted and unique"
        )));
    }
    Ok(())
}

fn ensure_sorted_object_keys(objects: &[CatalogObject]) -> Result<()> {
    if objects
        .windows(2)
        .any(|pair| pair[0].object_key >= pair[1].object_key)
    {
        return Err(Error::InvalidCatalog(
            "objects are not strictly sorted by key".to_owned(),
        ));
    }
    Ok(())
}

fn validate_label(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(Error::InvalidCatalog(format!(
            "{field} must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}

fn validate_sha256_id(field: &str, value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(Error::InvalidCatalog(format!(
            "{field} must start with sha256:"
        )));
    };
    validate_sha256_hex(field, digest)
}

fn validate_sha256_hex(field: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidCatalog(format!(
            "{field} must be 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn content_id(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn validate_content_id(label: &str, actual: &str, payload: &impl Serialize) -> Result<()> {
    validate_sha256_id(&format!("{label}_id"), actual)?;
    let expected = content_id(payload)?;
    if actual != expected {
        return Err(Error::InvalidCatalog(format!(
            "{label} identity mismatch: expected {expected}, found {actual}"
        )));
    }
    Ok(())
}

impl From<WorkUnitRecord> for CatalogWorkUnit {
    fn from(record: WorkUnitRecord) -> Self {
        let source_name = record
            .source_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| record.source_path.to_string_lossy().into_owned());
        Self {
            work_unit_id: record.id,
            source_name,
            source_bytes: record.source_bytes,
            source_sha256: record.source_sha256,
            target_uncompressed_bytes: record.target_uncompressed_bytes,
            max_event_bytes: record.max_event_bytes,
            object_prefix: record.object_prefix,
            writer_version: record.writer_version,
            input_events: record.input_events,
            output_rows: record.output_rows,
            rejected_events: record.rejected_events,
        }
    }
}

impl From<ObjectRecord> for CatalogObject {
    fn from(record: ObjectRecord) -> Self {
        Self {
            object_key: record.object_key,
            work_unit_id: record.work_unit_id,
            part_number: record.part_number,
            byte_size: record.byte_size,
            sha256: record.sha256,
            writer_version: record.writer_version,
            row_count: record.row_count,
            min_created_at: record.min_created_at.map(|value| value.to_string()),
            max_created_at: record.max_created_at.map(|value| value.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectKind, ObjectState, WorkState, WorkUnitRegistration};

    fn work(id: &str) -> CatalogWorkUnit {
        CatalogWorkUnit {
            work_unit_id: id.to_owned(),
            source_name: format!("{id}.notepack.gz"),
            source_bytes: 100,
            source_sha256: "11".repeat(32),
            target_uncompressed_bytes: 1_000,
            max_event_bytes: 2_000,
            object_prefix: "nostr/v1".to_owned(),
            writer_version: "test-writer".to_owned(),
            input_events: 2,
            output_rows: 2,
            rejected_events: 0,
        }
    }

    fn object(work_unit_id: &str, key: &str, part_number: u32) -> CatalogObject {
        CatalogObject {
            object_key: format!("nostr/v1/{key}"),
            work_unit_id: work_unit_id.to_owned(),
            part_number,
            byte_size: 50,
            sha256: "22".repeat(32),
            writer_version: "test-writer".to_owned(),
            row_count: 2,
            min_created_at: Some("1".to_owned()),
            max_created_at: Some(u64::MAX.to_string()),
        }
    }

    fn fragment(inventory_id: &str, work_id: &str, key: &str) -> ActiveRawFragment {
        ActiveRawFragment::from_records(
            inventory_id.to_owned(),
            "s3://test".to_owned(),
            vec![work(work_id)],
            vec![object(work_id, key, 0)],
        )
        .expect("valid fragment")
    }

    #[test]
    fn merge_is_deterministic_across_fragment_order() {
        let first = fragment("history", "work-a", "raw/a.parquet");
        let second = fragment("live", "work-b", "raw/b.parquet");
        let forward =
            merge_active_raw_fragments([first.clone(), second.clone()]).expect("forward merge");
        let reverse = merge_active_raw_fragments([second, first]).expect("reverse merge");

        assert_eq!(forward, reverse);
        assert_eq!(forward.totals().work_units, 2);
        assert_eq!(forward.totals().objects, 2);
        assert_eq!(forward.totals().physical_rows, 4);
        forward.validate().expect("valid snapshot");
    }

    #[test]
    fn identical_fragments_are_idempotent() {
        let fragment = fragment("history", "work-a", "raw/a.parquet");
        let once = merge_active_raw_fragments([fragment.clone()]).expect("single merge");
        let twice =
            merge_active_raw_fragments([fragment.clone(), fragment]).expect("duplicate merge");
        assert_eq!(once, twice);
    }

    #[test]
    fn content_identical_source_aliases_merge_deterministically() {
        let first = fragment("history", "work-a", "raw/a.parquet");
        let mut second = fragment("live", "work-a", "raw/a.parquet");
        second.payload.work_units[0].source_name = "another-source.notepack.gz".to_owned();
        second.fragment_id = content_id(&second.payload).expect("new identity");

        let forward =
            merge_active_raw_fragments([first.clone(), second.clone()]).expect("forward merge");
        let reverse = merge_active_raw_fragments([second, first]).expect("reverse merge");

        assert_eq!(forward, reverse);
        assert_eq!(forward.totals().work_units, 1);
        assert_eq!(
            forward.work_units()[0].source_name,
            "another-source.notepack.gz"
        );
    }

    #[test]
    fn content_identical_empty_source_aliases_merge_deterministically() {
        let mut first_work = work("empty-work");
        first_work.source_name = "segment-000006633.notepack.gz".to_owned();
        first_work.input_events = 0;
        first_work.output_rows = 0;
        let first = ActiveRawFragment::from_records(
            "history".to_owned(),
            "s3://test".to_owned(),
            vec![first_work.clone()],
            vec![],
        )
        .expect("first fragment");
        first_work.source_name = "segment-000007703.notepack.gz".to_owned();
        let second = ActiveRawFragment::from_records(
            "live".to_owned(),
            "s3://test".to_owned(),
            vec![first_work],
            vec![],
        )
        .expect("second fragment");

        let forward =
            merge_active_raw_fragments([first.clone(), second.clone()]).expect("forward merge");
        let reverse = merge_active_raw_fragments([second, first]).expect("reverse merge");

        assert_eq!(forward, reverse);
        assert_eq!(forward.totals().work_units, 1);
        assert_eq!(forward.totals().objects, 0);
        assert_eq!(
            forward.work_units()[0].source_name,
            "segment-000006633.notepack.gz"
        );
    }

    #[test]
    fn conflicting_object_key_is_rejected() {
        let first = fragment("history", "work-a", "raw/shared.parquet");
        let mut second = fragment("live", "work-b", "raw/shared.parquet");
        second.payload.objects[0].sha256 = "33".repeat(32);
        second.fragment_id = content_id(&second.payload).expect("new identity");

        let error = merge_active_raw_fragments([first, second]).expect_err("must conflict");
        assert!(error.to_string().contains("conflicting metadata"));
    }

    #[test]
    fn noncanonical_unsigned_timestamp_is_rejected() {
        let mut fragment = fragment("history", "work-a", "raw/a.parquet");
        fragment.payload.objects[0].min_created_at = Some("01".to_owned());
        fragment.fragment_id = content_id(&fragment.payload).expect("new identity");

        let error = fragment.validate().expect_err("must reject");
        assert!(error.to_string().contains("non-canonical"));
    }

    #[test]
    fn content_identity_detects_tampering() {
        let mut fragment = fragment("history", "work-a", "raw/a.parquet");
        fragment.payload.objects[0].byte_size += 1;

        let error = fragment.validate().expect_err("must reject");
        assert!(error.to_string().contains("totals do not match"));
    }

    #[test]
    fn work_unit_rows_must_match_its_active_objects() {
        let mut fragment = fragment("history", "work-a", "raw/a.parquet");
        fragment.payload.work_units[0].output_rows = 1;
        fragment.fragment_id = content_id(&fragment.payload).expect("new identity");

        let error = fragment.validate().expect_err("must reject");
        assert!(error.to_string().contains("reports 1 output rows"));
    }

    #[test]
    fn atomic_json_round_trip_preserves_snapshot() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("snapshot.json");
        let snapshot = merge_active_raw_fragments([fragment("history", "work-a", "raw/a.parquet")])
            .expect("snapshot");

        write_catalog_atomically(&path, &snapshot).expect("write");
        assert_eq!(read_catalog_snapshot(&path).expect("read"), snapshot);
    }

    #[test]
    fn reader_rejects_noncanonical_json_bytes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("snapshot.json");
        let snapshot = merge_active_raw_fragments([fragment("history", "work-a", "raw/a.parquet")])
            .expect("snapshot");
        fs::write(&path, serde_json::to_vec(&snapshot).expect("compact JSON")).expect("write");

        let error = read_catalog_snapshot(&path).expect_err("must reject");
        assert!(error.to_string().contains("not canonically encoded"));
    }

    #[test]
    fn inventory_export_includes_only_published_coverage_and_active_raw_objects() {
        let mut inventory = Inventory::open_in_memory().expect("inventory");
        publish_inventory_work(&mut inventory, "published", true);
        publish_inventory_work(&mut inventory, "empty", false);
        let failed_sha = "44".repeat(32);
        inventory
            .ensure_work_unit(&WorkUnitRegistration {
                id: "failed",
                source_path: Path::new("/source/failed.notepack.gz"),
                source_bytes: 20,
                source_sha256: &failed_sha,
                target_uncompressed_bytes: 1_000,
                max_event_bytes: 2_000,
                object_prefix: "nostr/v1",
                writer_version: "test-writer",
            })
            .expect("register failed work");
        inventory
            .transition_work("failed", WorkState::Writing, None)
            .expect("start failed work");
        inventory
            .transition_work("failed", WorkState::Failed, Some("test failure"))
            .expect("fail work");

        let fragment = ActiveRawFragment::export(&mut inventory, "test-inventory", "s3://test")
            .expect("export");

        assert_eq!(fragment.totals().work_units, 2);
        assert_eq!(fragment.totals().objects, 1);
        assert_eq!(fragment.work_units()[0].work_unit_id, "empty");
        assert_eq!(fragment.work_units()[1].work_unit_id, "published");
        assert_eq!(fragment.objects()[0].work_unit_id, "published");
        fragment.validate().expect("valid fragment");
    }

    fn publish_inventory_work(inventory: &mut Inventory, id: &str, with_object: bool) {
        let source_sha256 = "55".repeat(32);
        inventory
            .ensure_work_unit(&WorkUnitRegistration {
                id,
                source_path: &Path::new("/source").join(format!("{id}.notepack.gz")),
                source_bytes: 100,
                source_sha256: &source_sha256,
                target_uncompressed_bytes: 1_000,
                max_event_bytes: 2_000,
                object_prefix: "nostr/v1",
                writer_version: "test-writer",
            })
            .expect("register work");
        inventory
            .transition_work(id, WorkState::Writing, None)
            .expect("start work");
        let objects = with_object.then(|| ObjectRecord {
            object_key: format!("nostr/v1/raw/{id}/part-00000.parquet"),
            work_unit_id: id.to_owned(),
            part_number: 0,
            kind: ObjectKind::Parquet,
            state: ObjectState::Validated,
            local_path: Path::new("/staging").join(format!("{id}.parquet")),
            byte_size: 50,
            sha256: "66".repeat(32),
            writer_version: "test-writer".to_owned(),
            row_count: 2,
            min_created_at: Some(1),
            max_created_at: Some(u64::MAX),
        });
        inventory
            .record_validated_objects(
                id,
                u64::from(with_object) * 2,
                u64::from(with_object) * 2,
                0,
                &objects.into_iter().collect::<Vec<_>>(),
            )
            .expect("validate work");
        inventory
            .transition_work(id, WorkState::Uploading, None)
            .expect("start upload");
        if with_object {
            inventory
                .mark_object_uploaded(&format!("nostr/v1/raw/{id}/part-00000.parquet"))
                .expect("upload object");
        }
        inventory
            .transition_work(id, WorkState::Uploaded, None)
            .expect("finish upload");
        inventory.activate_work_unit(id).expect("activate work");
    }
}
