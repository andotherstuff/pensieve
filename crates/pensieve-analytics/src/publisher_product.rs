//! Disk-bounded exact publisher rankings for predefined rolling windows.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactIdentity, BoundedExecutionError, BoundedFixedActivity, DiskBudget,
    FIXED_ACTIVITY_RECORD_BYTES, Result, preflight_disk, publish_canonical_json,
};

/// Stable exact predefined-window ranking semantics.
pub const PUBLISHER_RANKING_VERSION: &str = "publisher-ranking-v1";
/// Fixed bytes in one canonical published ranking row.
pub const PUBLISHER_RANKING_RECORD_BYTES: usize = 64;

const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-publisher-ranking-v1";
const ALL_KINDS: i64 = -1;

/// Bounded disk and query settings for exact publisher rankings.
#[derive(Clone, Debug)]
pub struct PublisherRankingConfig {
    /// Durable resumable SQLite ledger.
    pub state_database: PathBuf,
    /// Canonical fixed-width ranking artifact.
    pub artifact_path: PathBuf,
    /// Exact supported rolling windows.
    pub windows_days: Vec<u32>,
    /// Maximum served publishers per window/filter.
    pub top_limit: usize,
    /// Pubkeys committed in one SQLite transaction.
    pub publisher_batch_size: usize,
    /// Hard SQLite file ceiling.
    pub max_state_bytes: u64,
    /// Fixed SQLite page cache.
    pub sqlite_cache_bytes: u64,
    /// Free work-filesystem bytes left untouched.
    pub disk_reserve_bytes: u64,
}

/// One exact materialized publisher ranking row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublisherRankingRow {
    /// Exact rolling window in days.
    pub days: u32,
    /// Exact kind filter, or all kinds.
    pub kind: Option<u16>,
    /// Raw publisher key.
    pub pubkey: [u8; 32],
    /// Exact event count.
    pub event_count: u64,
    /// Exact distinct kind count.
    pub kinds_count: u64,
    /// Earliest included timestamp.
    pub first_event: u32,
    /// Latest included timestamp.
    pub last_event: u32,
}

/// Immutable evidence for one exact bounded ranking product.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublisherRankingEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner identity.
    pub runner_version: String,
    /// Completion status.
    pub status: String,
    /// Stable product semantics.
    pub product_version: String,
    /// Frozen source snapshot.
    pub snapshot_id: String,
    /// Fixed analytics boundary.
    pub as_of_epoch: u64,
    /// Fully validated predecessor evidence used to enable the recurring lane.
    pub baseline_evidence_sha256: Option<String>,
    /// Validated fixed-activity evidence SHA-256.
    pub activity_evidence_sha256: String,
    /// Exact source activity artifact SHA-256.
    pub activity_artifact_sha256: String,
    /// Exact source records scanned.
    pub source_records: u64,
    /// Supported exact windows.
    pub windows_days: Vec<u32>,
    /// Maximum served rows per group.
    pub top_limit: usize,
    /// Exact `(window,kind,pubkey)` ledger rows including all-kind rows.
    pub ledger_rows: u64,
    /// Distinct `(window,kind)` groups represented by the ledger.
    pub ranking_groups: u64,
    /// Canonical published row artifact.
    pub ranking_artifact: ArtifactIdentity,
    /// Maximum kinds buffered for one publisher.
    pub max_publisher_kinds_buffered: usize,
    /// Maximum configured SQLite bytes.
    pub max_state_bytes: u64,
    /// Maximum configured SQLite cache bytes.
    pub sqlite_cache_bytes: u64,
    /// Free-space reserve.
    pub disk_reserve_bytes: u64,
}

/// Fully validated exact publisher ranking product.
pub struct BoundedPublisherRanking {
    /// Canonical evidence.
    pub evidence: PublisherRankingEvidence,
    /// Evidence SHA-256.
    pub evidence_sha256: String,
}

#[derive(Clone, Copy)]
struct ActivityRecord {
    pubkey: [u8; 32],
    created_at: u32,
    kind: u16,
}

#[derive(Clone, Copy, Default)]
struct KindAccumulator {
    count: u64,
    first: u32,
    last: u32,
}

#[derive(Default)]
struct AllAccumulator {
    count: u64,
    first: u32,
    last: u32,
    kinds: BTreeSet<u16>,
}

/// Build or resume exact predefined-window publisher rankings.
pub fn build_bounded_publisher_ranking(
    evidence_path: impl AsRef<Path>,
    activity: &BoundedFixedActivity,
    config: PublisherRankingConfig,
) -> Result<BoundedPublisherRanking> {
    build_publisher_ranking(evidence_path, activity, config, None)
}

/// Rebuild a target ranking after validating explicit predecessor lineage.
pub fn advance_bounded_publisher_ranking(
    evidence_path: impl AsRef<Path>,
    baseline: &BoundedPublisherRanking,
    activity: &BoundedFixedActivity,
    config: PublisherRankingConfig,
) -> Result<BoundedPublisherRanking> {
    if activity.evidence.as_of_epoch < baseline.evidence.as_of_epoch
        || config.windows_days != baseline.evidence.windows_days
        || config.top_limit != baseline.evidence.top_limit
    {
        return Err(BoundedExecutionError::Invalid(
            "publisher successor changes its predecessor window contract".to_owned(),
        )
        .into());
    }
    build_publisher_ranking(
        evidence_path,
        activity,
        config,
        Some(baseline.evidence_sha256.clone()),
    )
}

fn build_publisher_ranking(
    evidence_path: impl AsRef<Path>,
    activity: &BoundedFixedActivity,
    config: PublisherRankingConfig,
    baseline_evidence_sha256: Option<String>,
) -> Result<BoundedPublisherRanking> {
    validate_config(activity, &config)?;
    if evidence_path.as_ref().is_file() {
        let completed = load_bounded_publisher_ranking(evidence_path, &config.state_database)?;
        if completed.evidence.baseline_evidence_sha256 != baseline_evidence_sha256 {
            return Err(BoundedExecutionError::Invalid(
                "publisher completed evidence has different predecessor lineage".to_owned(),
            )
            .into());
        }
        return Ok(completed);
    }
    let state_parent = config.state_database.parent().ok_or_else(|| {
        BoundedExecutionError::Invalid("publisher state database has no parent".to_owned())
    })?;
    std::fs::create_dir_all(state_parent)?;
    if let Some(parent) = config.artifact_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    preflight_disk(
        state_parent,
        DiskBudget {
            output_bytes: config.max_state_bytes,
            temporary_bytes: 0,
            retained_bytes: config
                .state_database
                .metadata()
                .map_or(0, |metadata| metadata.len()),
            reserve_bytes: config.disk_reserve_bytes,
        },
    )?;
    let mut state = Connection::open(&config.state_database)?;
    configure_state(&state, &config)?;
    initialize_identity(&state, activity, &config)?;
    let (mut source_records, mut max_kinds) = state_progress(&state)?;
    prepare_rank_index_for_build(
        &state,
        source_records,
        activity.evidence.activity_artifact.row_count,
    )?;
    let source_path = Path::new(&activity.evidence.activity_artifact.path);
    let mut reader = BufReader::new(File::open(source_path)?);
    reader.seek(SeekFrom::Start(
        source_records
            .checked_mul(FIXED_ACTIVITY_RECORD_BYTES as u64)
            .ok_or_else(|| {
                BoundedExecutionError::Invalid("publisher source offset overflowed".to_owned())
            })?,
    ))?;
    let starts = config
        .windows_days
        .iter()
        .map(|days| {
            activity
                .evidence
                .as_of_epoch
                .checked_sub(u64::from(*days) * 86_400)
                .ok_or_else(|| {
                    BoundedExecutionError::Invalid("publisher window underflowed".to_owned()).into()
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut pending = read_activity(&mut reader)?;
    while pending.is_some() {
        let transaction = state.transaction()?;
        let mut completed_publishers = 0_usize;
        while completed_publishers < config.publisher_batch_size {
            let Some(first) = pending else {
                break;
            };
            let pubkey = first.pubkey;
            let mut all = (0..starts.len())
                .map(|_| AllAccumulator::default())
                .collect::<Vec<_>>();
            let mut kinds: BTreeMap<u16, Vec<KindAccumulator>> = BTreeMap::new();
            while let Some(record) = pending {
                if record.pubkey != pubkey {
                    break;
                }
                source_records = checked_add(source_records, 1, "publisher source records")?;
                if u64::from(record.created_at) <= activity.evidence.as_of_epoch {
                    for (index, start) in starts.iter().enumerate() {
                        if u64::from(record.created_at) < *start {
                            continue;
                        }
                        observe_all(&mut all[index], record)?;
                        let windows = kinds
                            .entry(record.kind)
                            .or_insert_with(|| vec![KindAccumulator::default(); starts.len()]);
                        observe_kind(&mut windows[index], record)?;
                    }
                }
                pending = read_activity(&mut reader)?;
            }
            max_kinds = max_kinds.max(kinds.len());
            insert_publisher(&transaction, &config.windows_days, pubkey, &all, &kinds)?;
            completed_publishers += 1;
        }
        transaction.execute(
            "UPDATE metadata SET value=?1 WHERE key='source_records'",
            [source_records.to_string()],
        )?;
        transaction.execute(
            "UPDATE metadata SET value=?1 WHERE key='max_publisher_kinds_buffered'",
            [max_kinds.to_string()],
        )?;
        transaction.commit()?;
        enforce_state_size(&state, config.max_state_bytes)?;
    }
    if source_records != activity.evidence.activity_artifact.row_count {
        return Err(BoundedExecutionError::Invalid(
            "publisher ranking did not scan the exact activity artifact".to_owned(),
        )
        .into());
    }
    ensure_rank_index(&state)?;
    let ledger_rows = count(&state, "SELECT count(*) FROM publisher_windows")?;
    let ranking_groups = count(
        &state,
        "SELECT count(*) FROM (SELECT 1 FROM publisher_windows GROUP BY days,kind)",
    )?;
    let ranking_artifact = materialize_artifact(&state, &config)?;
    let evidence = PublisherRankingEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        product_version: PUBLISHER_RANKING_VERSION.to_owned(),
        snapshot_id: activity.evidence.snapshot_id.clone(),
        as_of_epoch: activity.evidence.as_of_epoch,
        baseline_evidence_sha256,
        activity_evidence_sha256: activity.evidence_sha256.clone(),
        activity_artifact_sha256: activity.evidence.activity_artifact.sha256.clone(),
        source_records,
        windows_days: config.windows_days,
        top_limit: config.top_limit,
        ledger_rows,
        ranking_groups,
        ranking_artifact,
        max_publisher_kinds_buffered: max_kinds,
        max_state_bytes: config.max_state_bytes,
        sqlite_cache_bytes: config.sqlite_cache_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
    };
    validate_evidence(&evidence, &state)?;
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    Ok(BoundedPublisherRanking {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

/// Load and fully validate a completed publisher ranking product.
pub fn load_bounded_publisher_ranking(
    evidence_path: impl AsRef<Path>,
    state_database: impl AsRef<Path>,
) -> Result<BoundedPublisherRanking> {
    let evidence_path = evidence_path.as_ref();
    let evidence: PublisherRankingEvidence = serde_json::from_slice(&std::fs::read(evidence_path)?)
        .map_err(BoundedExecutionError::from)?;
    let state =
        Connection::open_with_flags(state_database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_evidence(&evidence, &state)?;
    Ok(BoundedPublisherRanking {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

/// Visit canonical ranking rows without buffering the artifact.
pub fn visit_publisher_ranking_rows(
    product: &BoundedPublisherRanking,
    mut visitor: impl FnMut(PublisherRankingRow) -> Result<()>,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(&product.evidence.ranking_artifact.path)?);
    let mut previous: Option<PublisherRankingRow> = None;
    let mut rows = 0_u64;
    loop {
        let mut bytes = [0_u8; PUBLISHER_RANKING_RECORD_BYTES];
        if !read_exact_or_eof(&mut reader, &mut bytes)? {
            break;
        }
        let row = decode_ranking(bytes)?;
        if row.event_count == 0 || row.kinds_count == 0 || row.first_event > row.last_event {
            return Err(BoundedExecutionError::Invalid(
                "publisher ranking artifact contains invalid metrics".to_owned(),
            )
            .into());
        }
        if let Some(previous) = previous.as_ref()
            && !ranking_precedes(previous, &row)
        {
            return Err(BoundedExecutionError::Invalid(
                "publisher ranking artifact is not canonically ordered".to_owned(),
            )
            .into());
        }
        rows = checked_add(rows, 1, "publisher ranking rows")?;
        previous = Some(row.clone());
        visitor(row)?;
    }
    if rows != product.evidence.ranking_artifact.row_count {
        return Err(BoundedExecutionError::Invalid(
            "publisher ranking artifact row count changed".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn configure_state(state: &Connection, config: &PublisherRankingConfig) -> Result<()> {
    state.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS publisher_windows(
             days INTEGER NOT NULL,kind INTEGER NOT NULL,pubkey BLOB NOT NULL,
             event_count INTEGER NOT NULL,kinds_count INTEGER NOT NULL,
             first_event INTEGER NOT NULL,last_event INTEGER NOT NULL,
             PRIMARY KEY(days,kind,pubkey)
         ) WITHOUT ROWID;",
    )?;
    let page_size: u64 = state.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let max_pages = config.max_state_bytes / page_size;
    if max_pages == 0 {
        return Err(BoundedExecutionError::Invalid(
            "publisher max state is below one SQLite page".to_owned(),
        )
        .into());
    }
    state.pragma_update(None, "max_page_count", max_pages)?;
    let cache_kib = config.sqlite_cache_bytes.div_ceil(1024);
    let cache = -i64::try_from(cache_kib)
        .map_err(|_| BoundedExecutionError::Invalid("publisher cache exceeds i64".to_owned()))?;
    state.pragma_update(None, "cache_size", cache)?;
    Ok(())
}

fn prepare_rank_index_for_build(
    state: &Connection,
    source_records: u64,
    expected_source_records: u64,
) -> Result<()> {
    if source_records > expected_source_records {
        return Err(BoundedExecutionError::Invalid(
            "publisher source progress exceeds its immutable activity artifact".to_owned(),
        )
        .into());
    }
    if source_records < expected_source_records {
        state.execute_batch("DROP INDEX IF EXISTS publisher_window_rank")?;
    } else {
        ensure_rank_index(state)?;
    }
    Ok(())
}

fn ensure_rank_index(state: &Connection) -> Result<()> {
    state.execute_batch(
        "CREATE INDEX IF NOT EXISTS publisher_window_rank
             ON publisher_windows(days,kind,event_count DESC,pubkey ASC)",
    )?;
    Ok(())
}

fn initialize_identity(
    state: &Connection,
    activity: &BoundedFixedActivity,
    config: &PublisherRankingConfig,
) -> Result<()> {
    let identity = serde_json::to_string(&(
        PUBLISHER_RANKING_VERSION,
        &activity.evidence.snapshot_id,
        activity.evidence.as_of_epoch,
        &activity.evidence_sha256,
        &activity.evidence.activity_artifact.sha256,
        &config.windows_days,
        config.top_limit,
    ))
    .map_err(BoundedExecutionError::from)?;
    let existing: Option<String> = state
        .query_row(
            "SELECT value FROM metadata WHERE key='identity'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing != identity {
            return Err(BoundedExecutionError::Invalid(
                "publisher state belongs to a different immutable run".to_owned(),
            )
            .into());
        }
    } else {
        state.execute("INSERT INTO metadata VALUES('identity',?1)", [&identity])?;
        state.execute("INSERT INTO metadata VALUES('source_records','0')", [])?;
        state.execute(
            "INSERT INTO metadata VALUES('max_publisher_kinds_buffered','0')",
            [],
        )?;
    }
    Ok(())
}

fn state_progress(state: &Connection) -> Result<(u64, usize)> {
    let records: String = state.query_row(
        "SELECT value FROM metadata WHERE key='source_records'",
        [],
        |row| row.get(0),
    )?;
    let kinds: String = state.query_row(
        "SELECT value FROM metadata WHERE key='max_publisher_kinds_buffered'",
        [],
        |row| row.get(0),
    )?;
    Ok((
        records.parse().map_err(|_| {
            BoundedExecutionError::Invalid("publisher source progress is invalid".to_owned())
        })?,
        kinds.parse().map_err(|_| {
            BoundedExecutionError::Invalid("publisher kind progress is invalid".to_owned())
        })?,
    ))
}

fn insert_publisher(
    transaction: &Transaction<'_>,
    windows: &[u32],
    pubkey: [u8; 32],
    all: &[AllAccumulator],
    kinds: &BTreeMap<u16, Vec<KindAccumulator>>,
) -> Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO publisher_windows
         (days,kind,pubkey,event_count,kinds_count,first_event,last_event)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
    )?;
    for (index, accumulator) in all.iter().enumerate() {
        if accumulator.count != 0 {
            insert_accumulator(
                &mut statement,
                windows[index],
                ALL_KINDS,
                &pubkey,
                accumulator.count,
                accumulator.kinds.len() as u64,
                accumulator.first,
                accumulator.last,
            )?;
        }
    }
    for (kind, values) in kinds {
        for (index, accumulator) in values.iter().enumerate() {
            if accumulator.count != 0 {
                insert_accumulator(
                    &mut statement,
                    windows[index],
                    i64::from(*kind),
                    &pubkey,
                    accumulator.count,
                    1,
                    accumulator.first,
                    accumulator.last,
                )?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_accumulator(
    statement: &mut rusqlite::CachedStatement<'_>,
    days: u32,
    kind: i64,
    pubkey: &[u8; 32],
    count: u64,
    kinds: u64,
    first: u32,
    last: u32,
) -> Result<()> {
    statement.execute(params![
        i64::from(days),
        kind,
        &pubkey[..],
        to_i64("publisher count", count)?,
        to_i64("publisher kinds", kinds)?,
        i64::from(first),
        i64::from(last),
    ])?;
    Ok(())
}

fn materialize_artifact(
    state: &Connection,
    config: &PublisherRankingConfig,
) -> Result<ArtifactIdentity> {
    let partial = config
        .artifact_path
        .with_extension(format!("partial.{}", std::process::id()));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let mut writer = BufWriter::new(output);
    let mut row_count = 0_u64;
    let mut min_key = None;
    let mut max_key = None;
    visit_state_rankings(state, config.top_limit, |ranking| {
        let bytes = encode_ranking(&ranking);
        let key = hex::encode(&bytes[..40]);
        if min_key.as_ref().is_none_or(|minimum| &key < minimum) {
            min_key = Some(key.clone());
        }
        if max_key.as_ref().is_none_or(|maximum| &key > maximum) {
            max_key = Some(key);
        }
        writer.write_all(&bytes)?;
        row_count = checked_add(row_count, 1, "publisher materialized rows")?;
        Ok(())
    })?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if config.artifact_path.exists() {
        let existing = pensieve_lake::sha256_file(&config.artifact_path)?;
        let generated = pensieve_lake::sha256_file(&partial)?;
        if existing != generated {
            return Err(BoundedExecutionError::Invalid(
                "publisher completed artifact conflicts with retry output".to_owned(),
            )
            .into());
        }
        std::fs::remove_file(&partial)?;
    } else {
        std::fs::rename(&partial, &config.artifact_path)?;
    }
    Ok(ArtifactIdentity {
        path: config.artifact_path.to_string_lossy().into_owned(),
        byte_size: config.artifact_path.metadata()?.len(),
        row_count,
        min_key,
        max_key,
        sha256: pensieve_lake::sha256_file(&config.artifact_path)?,
    })
}

fn validate_evidence(evidence: &PublisherRankingEvidence, state: &Connection) -> Result<()> {
    let ledger_rows = count(state, "SELECT count(*) FROM publisher_windows")?;
    let rank_indexes = count(
        state,
        "SELECT count(*) FROM sqlite_master
          WHERE type='index' AND name='publisher_window_rank'",
    )?;
    let groups = count(
        state,
        "SELECT count(*) FROM (SELECT 1 FROM publisher_windows GROUP BY days,kind)",
    )?;
    let metadata = std::fs::metadata(&evidence.ranking_artifact.path)?;
    let expected_bytes = evidence
        .ranking_artifact
        .row_count
        .checked_mul(PUBLISHER_RANKING_RECORD_BYTES as u64);
    let expected_identity = serde_json::to_string(&(
        PUBLISHER_RANKING_VERSION,
        &evidence.snapshot_id,
        evidence.as_of_epoch,
        &evidence.activity_evidence_sha256,
        &evidence.activity_artifact_sha256,
        &evidence.windows_days,
        evidence.top_limit,
    ))
    .map_err(BoundedExecutionError::from)?;
    let state_identity: String = state.query_row(
        "SELECT value FROM metadata WHERE key='identity'",
        [],
        |row| row.get(0),
    )?;
    let (source_records, max_kinds) = state_progress(state)?;
    if evidence.schema_version != EVIDENCE_SCHEMA_VERSION
        || evidence.runner_version != RUNNER_VERSION
        || evidence.status != "completed"
        || evidence.product_version != PUBLISHER_RANKING_VERSION
        || evidence.windows_days.is_empty()
        || evidence
            .windows_days
            .windows(2)
            .any(|days| days[0] >= days[1])
        || evidence.top_limit == 0
        || evidence.top_limit > 1_000
        || state_identity != expected_identity
        || evidence.source_records != source_records
        || evidence.max_publisher_kinds_buffered != max_kinds
        || rank_indexes != 1
        || evidence.ledger_rows != ledger_rows
        || evidence.ranking_groups != groups
        || evidence.ranking_artifact.row_count > groups.saturating_mul(evidence.top_limit as u64)
        || expected_bytes != Some(evidence.ranking_artifact.byte_size)
        || metadata.len() != evidence.ranking_artifact.byte_size
        || pensieve_lake::sha256_file(&evidence.ranking_artifact.path)?
            != evidence.ranking_artifact.sha256
    {
        return Err(BoundedExecutionError::Invalid(
            "publisher ranking evidence does not reconcile".to_owned(),
        )
        .into());
    }
    let product = BoundedPublisherRanking {
        evidence: evidence.clone(),
        evidence_sha256: String::new(),
    };
    visit_publisher_ranking_rows(&product, |_| Ok(()))?;
    let mut artifact = BufReader::new(File::open(&evidence.ranking_artifact.path)?);
    let mut compared = 0_u64;
    visit_state_rankings(state, evidence.top_limit, |expected| {
        let mut bytes = [0_u8; PUBLISHER_RANKING_RECORD_BYTES];
        if !read_exact_or_eof(&mut artifact, &mut bytes)? || decode_ranking(bytes)? != expected {
            return Err(BoundedExecutionError::Invalid(
                "publisher ranking artifact differs from exact ledger query".to_owned(),
            )
            .into());
        }
        compared = checked_add(compared, 1, "publisher reconciled rows")?;
        Ok(())
    })?;
    if read_exact_or_eof(&mut artifact, &mut [0_u8; PUBLISHER_RANKING_RECORD_BYTES])?
        || compared != evidence.ranking_artifact.row_count
    {
        return Err(BoundedExecutionError::Invalid(
            "publisher ranking artifact has unreconciled rows".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn visit_state_rankings(
    state: &Connection,
    top_limit: usize,
    mut visitor: impl FnMut(PublisherRankingRow) -> Result<()>,
) -> Result<()> {
    let mut statement = state.prepare(
        "SELECT days,kind,pubkey,event_count,kinds_count,first_event,last_event
           FROM (
             SELECT *,row_number() OVER (
               PARTITION BY days,kind ORDER BY event_count DESC,pubkey ASC
             ) AS rank
             FROM publisher_windows
           ) WHERE rank <= ?1
          ORDER BY days ASC,kind ASC,event_count DESC,pubkey ASC",
    )?;
    let mut query = statement.query([to_i64("publisher top limit", top_limit as u64)?])?;
    while let Some(row) = query.next()? {
        visitor(PublisherRankingRow {
            days: u32::try_from(row.get::<_, i64>(0)?).map_err(|_| {
                BoundedExecutionError::Invalid("publisher days are invalid".to_owned())
            })?,
            kind: decode_kind(row.get(1)?)?,
            pubkey: fixed_32(row.get(2)?, "publisher pubkey")?,
            event_count: from_i64("publisher event count", row.get(3)?)?,
            kinds_count: from_i64("publisher kind count", row.get(4)?)?,
            first_event: u32::try_from(row.get::<_, i64>(5)?).map_err(|_| {
                BoundedExecutionError::Invalid("publisher first event is invalid".to_owned())
            })?,
            last_event: u32::try_from(row.get::<_, i64>(6)?).map_err(|_| {
                BoundedExecutionError::Invalid("publisher last event is invalid".to_owned())
            })?,
        })?;
    }
    Ok(())
}

fn validate_config(activity: &BoundedFixedActivity, config: &PublisherRankingConfig) -> Result<()> {
    activity.validate_for_publication(
        &activity.evidence.snapshot_id,
        activity.evidence.as_of_epoch,
    )?;
    if config.windows_days.is_empty()
        || config
            .windows_days
            .windows(2)
            .any(|days| days[0] >= days[1])
        || config.windows_days.contains(&0)
        || config.top_limit == 0
        || config.top_limit > 1_000
        || config.publisher_batch_size == 0
        || config.max_state_bytes == 0
        || config.sqlite_cache_bytes == 0
    {
        return Err(BoundedExecutionError::Invalid(
            "publisher ranking bounded configuration is invalid".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn observe_all(accumulator: &mut AllAccumulator, record: ActivityRecord) -> Result<()> {
    accumulator.count = checked_add(accumulator.count, 1, "publisher event count")?;
    if accumulator.count == 1 {
        accumulator.first = record.created_at;
    }
    accumulator.last = record.created_at;
    accumulator.kinds.insert(record.kind);
    Ok(())
}

fn observe_kind(accumulator: &mut KindAccumulator, record: ActivityRecord) -> Result<()> {
    accumulator.count = checked_add(accumulator.count, 1, "publisher kind event count")?;
    if accumulator.count == 1 {
        accumulator.first = record.created_at;
    }
    accumulator.last = record.created_at;
    Ok(())
}

fn read_activity(reader: &mut impl Read) -> Result<Option<ActivityRecord>> {
    let mut bytes = [0_u8; FIXED_ACTIVITY_RECORD_BYTES];
    if !read_exact_or_eof(reader, &mut bytes)? {
        return Ok(None);
    }
    Ok(Some(ActivityRecord {
        pubkey: bytes[..32].try_into().expect("fixed pubkey"),
        created_at: u32::from_be_bytes(bytes[32..36].try_into().expect("fixed timestamp")),
        kind: u16::from_be_bytes(bytes[36..38].try_into().expect("fixed kind")),
    }))
}

fn encode_ranking(row: &PublisherRankingRow) -> [u8; PUBLISHER_RANKING_RECORD_BYTES] {
    let mut bytes = [0_u8; PUBLISHER_RANKING_RECORD_BYTES];
    bytes[..4].copy_from_slice(&row.days.to_be_bytes());
    bytes[4..8].copy_from_slice(&row.kind.map_or(65_536_u32, u32::from).to_be_bytes());
    bytes[8..40].copy_from_slice(&row.pubkey);
    bytes[40..48].copy_from_slice(&row.event_count.to_be_bytes());
    bytes[48..56].copy_from_slice(&row.kinds_count.to_be_bytes());
    bytes[56..60].copy_from_slice(&row.first_event.to_be_bytes());
    bytes[60..64].copy_from_slice(&row.last_event.to_be_bytes());
    bytes
}

fn decode_ranking(bytes: [u8; PUBLISHER_RANKING_RECORD_BYTES]) -> Result<PublisherRankingRow> {
    let days = u32::from_be_bytes(bytes[..4].try_into().expect("ranking days"));
    let kind = u32::from_be_bytes(bytes[4..8].try_into().expect("ranking kind"));
    Ok(PublisherRankingRow {
        days,
        kind: if kind == 65_536 {
            None
        } else {
            Some(u16::try_from(kind).map_err(|_| {
                BoundedExecutionError::Invalid("publisher artifact kind is invalid".to_owned())
            })?)
        },
        pubkey: bytes[8..40].try_into().expect("ranking pubkey"),
        event_count: u64::from_be_bytes(bytes[40..48].try_into().expect("ranking count")),
        kinds_count: u64::from_be_bytes(bytes[48..56].try_into().expect("ranking kinds")),
        first_event: u32::from_be_bytes(bytes[56..60].try_into().expect("ranking first")),
        last_event: u32::from_be_bytes(bytes[60..64].try_into().expect("ranking last")),
    })
}

fn ranking_precedes(left: &PublisherRankingRow, right: &PublisherRankingRow) -> bool {
    (left.days, left.kind) < (right.days, right.kind)
        || ((left.days, left.kind) == (right.days, right.kind)
            && (left.event_count > right.event_count
                || (left.event_count == right.event_count && left.pubkey < right.pubkey)))
}

fn decode_kind(value: i64) -> Result<Option<u16>> {
    if value == ALL_KINDS {
        Ok(None)
    } else {
        Ok(Some(u16::try_from(value).map_err(|_| {
            BoundedExecutionError::Invalid("publisher kind is invalid".to_owned())
        })?))
    }
}

fn read_exact_or_eof(reader: &mut impl Read, bytes: &mut [u8]) -> Result<bool> {
    let mut offset = 0;
    while offset < bytes.len() {
        match reader.read(&mut bytes[offset..])? {
            0 if offset == 0 => return Ok(false),
            0 => {
                return Err(BoundedExecutionError::Invalid(
                    "publisher fixed-width artifact is truncated".to_owned(),
                )
                .into());
            }
            read => offset += read,
        }
    }
    Ok(true)
}

fn enforce_state_size(state: &Connection, maximum: u64) -> Result<()> {
    let pages: u64 = state.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: u64 = state.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    if pages.saturating_mul(page_size) > maximum {
        return Err(BoundedExecutionError::Invalid(
            "publisher SQLite state exceeded its configured ceiling".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn count(state: &Connection, sql: &str) -> Result<u64> {
    from_i64(
        "publisher state count",
        state.query_row(sql, [], |row| row.get(0))?,
    )
}

fn fixed_32(value: Vec<u8>, label: &str) -> Result<[u8; 32]> {
    value.try_into().map_err(|value: Vec<u8>| {
        BoundedExecutionError::Invalid(format!("{label} has {} bytes instead of 32", value.len()))
            .into()
    })
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed")).into())
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| crate::Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| crate::Error::NegativeLedgerValue { field, value })
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{Result, prepare_rank_index_for_build};

    #[test]
    fn ranking_index_exists_only_after_the_source_is_complete() -> Result<()> {
        let state = Connection::open_in_memory()?;
        state.execute_batch(
            "CREATE TABLE publisher_windows(
                 days INTEGER NOT NULL,kind INTEGER NOT NULL,pubkey BLOB NOT NULL,
                 event_count INTEGER NOT NULL,kinds_count INTEGER NOT NULL,
                 first_event INTEGER NOT NULL,last_event INTEGER NOT NULL,
                 PRIMARY KEY(days,kind,pubkey)
             ) WITHOUT ROWID;
             CREATE INDEX publisher_window_rank
                 ON publisher_windows(days,kind,event_count DESC,pubkey ASC);",
        )?;

        prepare_rank_index_for_build(&state, 1, 2)?;
        assert_eq!(index_count(&state)?, 0);

        prepare_rank_index_for_build(&state, 2, 2)?;
        assert_eq!(index_count(&state)?, 1);
        assert!(prepare_rank_index_for_build(&state, 3, 2).is_err());
        Ok(())
    }

    fn index_count(state: &Connection) -> Result<u64> {
        Ok(state.query_row(
            "SELECT count(*) FROM sqlite_master
              WHERE type='index' AND name='publisher_window_rank'",
            [],
            |row| row.get(0),
        )?)
    }
}
