//! Resumable disk-bounded construction of current NIP-65 relay distribution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use duckdb::Connection as DuckConnection;
use rusqlite::{Connection as SqliteConnection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{configure_execution, configure_remote_access};
use crate::event_facts::verify_local_batch_inputs;
use crate::{
    BatchLimits, BoundedExecutionError, BuildConfig, DiskBudget, InputIdentity, ObjectLocation,
    RELAY_DISTRIBUTION_VERSION, RelayDistributionRow, ResolvedSnapshot, Result, plan_input_batches,
    preflight_disk, publish_canonical_json, relay_memberships,
};

const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-relay-distribution-v2";

/// Bounded workspace and serving thresholds for current relay distribution.
#[derive(Clone, Debug)]
pub struct RelayDistributionConfig {
    /// Durable resumable SQLite state database.
    pub state_database: PathBuf,
    /// Maximum catalog bytes and rows scanned by one DuckDB batch.
    pub batch_limits: BatchLimits,
    /// Maximum SQLite bytes before the build fails closed.
    pub max_state_bytes: u64,
    /// SQLite page-cache bound in bytes.
    pub sqlite_cache_bytes: u64,
    /// Minimum unique winning pubkeys required for a serving row.
    pub minimum_users: u64,
    /// Free work-filesystem bytes left untouched.
    pub disk_reserve_bytes: u64,
}

/// Immutable completion evidence for deterministic current relay distribution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RelayDistributionEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Completion state.
    pub status: String,
    /// Stable product semantics.
    pub product_version: String,
    /// Frozen canonical catalog snapshot.
    pub snapshot_id: String,
    /// Fixed analytics boundary.
    pub as_of_epoch: u64,
    /// SHA-256 of the fully validated predecessor evidence, when advanced.
    pub baseline_evidence_sha256: Option<String>,
    /// Exact immutable objects added after the predecessor snapshot.
    pub delta_object_count: u64,
    /// Active immutable catalog objects covered.
    pub object_count: u64,
    /// Exact active objects applied to resumable state.
    pub applied_objects: u64,
    /// Physical source rows scanned across every applied object.
    pub physical_rows_scanned: u64,
    /// Physical kind-10002 rows stored or deduplicated.
    pub physical_relay_events: u64,
    /// Unique canonical kind-10002 event IDs in state.
    pub candidate_events: u64,
    /// Candidate events at or before the fixed boundary.
    pub eligible_candidate_events: u64,
    /// Pubkeys with an eligible deterministic winning event.
    pub winning_pubkeys: u64,
    /// Normalized memberships attached to all unique candidate events.
    pub candidate_memberships: u64,
    /// Raw `r` tags encountered in physical kind-10002 rows.
    pub raw_relay_tags: u64,
    /// Raw tags rejected by canonical URL normalization.
    pub invalid_relay_tags: u64,
    /// Duplicate normalized memberships suppressed within physical events.
    pub duplicate_relay_tags: u64,
    /// Minimum users retained in the serving relation.
    pub minimum_users: u64,
    /// Final rows ordered by descending users and canonical URL tie-break.
    pub rows: Vec<RelayDistributionRow>,
    /// SHA-256 of canonical final row JSON.
    pub rows_sha256: String,
    /// Exact active object identities covered by the state.
    pub inputs: Vec<InputIdentity>,
    /// Maximum configured state bytes.
    pub max_state_bytes: u64,
    /// Maximum configured SQLite page-cache bytes.
    pub sqlite_cache_bytes: u64,
    /// Free-space reserve.
    pub disk_reserve_bytes: u64,
}

/// Fully validated current relay distribution product.
pub struct BoundedRelayDistribution {
    /// Canonical evidence.
    pub evidence: RelayDistributionEvidence,
    /// SHA-256 of completion evidence.
    pub evidence_sha256: String,
}

#[derive(Default)]
struct BatchStats {
    physical_rows: u64,
    physical_relay_events: u64,
    raw_relay_tags: u64,
    invalid_relay_tags: u64,
    duplicate_relay_tags: u64,
}

/// Build or advance exact current relay state from append-only Parquet objects.
pub fn build_bounded_relay_distribution(
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: RelayDistributionConfig,
) -> Result<BoundedRelayDistribution> {
    build_relay_distribution(evidence_path, snapshot, build, config, None)
}

/// Advance an exact relay ledger after validating its immutable predecessor.
pub fn advance_bounded_relay_distribution(
    evidence_path: impl AsRef<Path>,
    baseline: &BoundedRelayDistribution,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: RelayDistributionConfig,
) -> Result<BoundedRelayDistribution> {
    let target_inputs = catalog_inputs(&snapshot)?;
    let target_by_key = target_inputs
        .iter()
        .map(|input| (input.identity.as_str(), input))
        .collect::<BTreeMap<_, _>>();
    for input in &baseline.evidence.inputs {
        if target_by_key.get(input.identity.as_str()).copied() != Some(input) {
            return Err(BoundedExecutionError::Invalid(format!(
                "relay target does not retain immutable baseline object {}",
                input.identity
            ))
            .into());
        }
    }
    if build.as_of_epoch < baseline.evidence.as_of_epoch {
        return Err(BoundedExecutionError::Invalid(
            "relay successor as-of precedes its baseline".to_owned(),
        )
        .into());
    }
    let delta_object_count = u64::try_from(
        target_inputs
            .len()
            .checked_sub(baseline.evidence.inputs.len())
            .ok_or_else(|| {
                BoundedExecutionError::Invalid(
                    "relay target has fewer objects than its baseline".to_owned(),
                )
            })?,
    )
    .map_err(|_| BoundedExecutionError::Invalid("relay delta count exceeds u64".to_owned()))?;
    build_relay_distribution(
        evidence_path,
        snapshot,
        build,
        config,
        Some((baseline.evidence_sha256.clone(), delta_object_count)),
    )
}

fn build_relay_distribution(
    evidence_path: impl AsRef<Path>,
    snapshot: ResolvedSnapshot,
    build: BuildConfig,
    config: RelayDistributionConfig,
    lineage: Option<(String, u64)>,
) -> Result<BoundedRelayDistribution> {
    validate_config(&snapshot, &build, &config)?;
    let state_parent = config.state_database.parent().ok_or_else(|| {
        BoundedExecutionError::Invalid("relay state database has no parent".to_owned())
    })?;
    std::fs::create_dir_all(state_parent)?;
    preflight_disk(
        state_parent,
        DiskBudget {
            output_bytes: config.max_state_bytes,
            temporary_bytes: 0,
            retained_bytes: config
                .state_database
                .metadata()
                .map_or(0, |meta| meta.len()),
            reserve_bytes: config.disk_reserve_bytes,
        },
    )?;

    let inputs = catalog_inputs(&snapshot)?;
    let mut state = SqliteConnection::open(&config.state_database)?;
    configure_state(&state, &config)?;
    validate_applied_inputs(&state, &inputs)?;
    let applied = applied_input_keys(&state)?;
    let mut pending_inputs = Vec::new();
    let mut pending_locations = Vec::new();
    for (input, location) in inputs.iter().zip(&snapshot.locations) {
        if !applied.contains(&input.identity) {
            pending_inputs.push(input.clone());
            pending_locations.push(location.clone());
        }
    }
    let batches = plan_input_batches(&pending_inputs, config.batch_limits)?;
    let duck = DuckConnection::open_in_memory()?;
    configure_execution(&duck, &build)?;
    duck.execute_batch("SET TimeZone='UTC'; SET preserve_insertion_order=false")?;
    configure_remote_access(&duck, &snapshot, &build)?;

    let mut offset = 0_usize;
    for batch in &batches {
        let end = offset.checked_add(batch.inputs.len()).ok_or_else(|| {
            BoundedExecutionError::Invalid("relay batch location offset overflowed".to_owned())
        })?;
        let locations = pending_locations.get(offset..end).ok_or_else(|| {
            BoundedExecutionError::Invalid("relay batch locations are incomplete".to_owned())
        })?;
        verify_local_batch_inputs(&batch.inputs, locations)?;
        apply_batch(&duck, &mut state, &batch.inputs, locations)?;
        offset = end;
    }
    if offset != pending_locations.len() {
        return Err(BoundedExecutionError::Invalid(
            "relay batches did not consume all pending locations".to_owned(),
        )
        .into());
    }

    state.execute(
        "INSERT INTO metadata(key,value) VALUES('snapshot_id',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [&snapshot.catalog.snapshot_id],
    )?;
    enforce_state_size(&state, config.max_state_bytes)?;
    let rows = materialize_rows(&state, build.as_of_epoch, config.minimum_users)?;
    let rows_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&rows).map_err(BoundedExecutionError::from)?,
    ));
    let totals = state_totals(&state, build.as_of_epoch)?;
    let physical = physical_totals(&state)?;
    let evidence = RelayDistributionEvidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        product_version: RELAY_DISTRIBUTION_VERSION.to_owned(),
        snapshot_id: snapshot.catalog.snapshot_id.clone(),
        as_of_epoch: build.as_of_epoch,
        baseline_evidence_sha256: lineage.as_ref().map(|value| value.0.clone()),
        delta_object_count: lineage.map_or(
            u64::try_from(inputs.len()).map_err(|_| {
                BoundedExecutionError::Invalid("relay object count exceeds u64".to_owned())
            })?,
            |value| value.1,
        ),
        object_count: u64::try_from(inputs.len()).map_err(|_| {
            BoundedExecutionError::Invalid("relay object count exceeds u64".to_owned())
        })?,
        applied_objects: u64::try_from(inputs.len()).map_err(|_| {
            BoundedExecutionError::Invalid("applied relay object count exceeds u64".to_owned())
        })?,
        physical_rows_scanned: physical.0,
        physical_relay_events: physical.1,
        candidate_events: totals.0,
        eligible_candidate_events: totals.1,
        winning_pubkeys: totals.2,
        candidate_memberships: totals.3,
        raw_relay_tags: physical.2,
        invalid_relay_tags: physical.3,
        duplicate_relay_tags: physical.4,
        minimum_users: config.minimum_users,
        rows,
        rows_sha256,
        inputs,
        max_state_bytes: config.max_state_bytes,
        sqlite_cache_bytes: config.sqlite_cache_bytes,
        disk_reserve_bytes: config.disk_reserve_bytes,
    };
    validate_evidence(&evidence, &state)?;
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    Ok(BoundedRelayDistribution {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

/// Load and fully revalidate a completed relay distribution against its state.
pub fn load_bounded_relay_distribution(
    evidence_path: impl AsRef<Path>,
    state_database: impl AsRef<Path>,
) -> Result<BoundedRelayDistribution> {
    let evidence_path = evidence_path.as_ref();
    let evidence: RelayDistributionEvidence =
        serde_json::from_slice(&std::fs::read(evidence_path)?)
            .map_err(BoundedExecutionError::from)?;
    let state = SqliteConnection::open_with_flags(
        state_database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    validate_applied_inputs(&state, &evidence.inputs)?;
    validate_evidence(&evidence, &state)?;
    Ok(BoundedRelayDistribution {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

/// Load predecessor evidence for a resumable successor whose ledger may
/// already contain a validated prefix of the declared target.
pub fn load_bounded_relay_distribution_for_advance(
    evidence_path: impl AsRef<Path>,
    state_database: impl AsRef<Path>,
    target: &ResolvedSnapshot,
) -> Result<BoundedRelayDistribution> {
    let evidence_path = evidence_path.as_ref();
    let evidence: RelayDistributionEvidence =
        serde_json::from_slice(&std::fs::read(evidence_path)?)
            .map_err(BoundedExecutionError::from)?;
    validate_static_evidence(&evidence)?;
    let state = SqliteConnection::open_with_flags(
        state_database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let applied_objects = count(&state, "SELECT count(*) FROM applied_objects", [])?;
    let state_snapshot: Option<String> = state
        .query_row(
            "SELECT value FROM metadata WHERE key='snapshot_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if applied_objects == evidence.applied_objects
        && state_snapshot.as_deref() == Some(evidence.snapshot_id.as_str())
    {
        validate_applied_inputs(&state, &evidence.inputs)?;
        validate_evidence(&evidence, &state)?;
    } else {
        validate_inputs_present(&state, &evidence.inputs)?;
        let target_inputs = catalog_inputs(target)?;
        validate_applied_inputs(&state, &target_inputs)?;
        if applied_objects < evidence.applied_objects || state_snapshot.is_none() {
            return Err(BoundedExecutionError::Invalid(
                "relay resumable state is not an exact predecessor-to-target prefix".to_owned(),
            )
            .into());
        }
    }
    Ok(BoundedRelayDistribution {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    })
}

fn apply_batch(
    duck: &DuckConnection,
    state: &mut SqliteConnection,
    inputs: &[InputIdentity],
    locations: &[ObjectLocation],
) -> Result<()> {
    let paths = locations
        .iter()
        .map(|location| sql_string(&location.duckdb_path()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id,pubkey,created_at,kind,to_json(tags) \
         FROM read_parquet([{paths}],union_by_name=false)"
    );
    let mut statement = duck.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let transaction = state.transaction()?;
    let mut stats = BatchStats::default();
    while let Some(row) = rows.next()? {
        stats.physical_rows = checked_add(stats.physical_rows, 1, "relay physical rows")?;
        let kind: u16 = row.get(3)?;
        if kind != 10_002 {
            continue;
        }
        stats.physical_relay_events = checked_add(
            stats.physical_relay_events,
            1,
            "relay physical kind-10002 rows",
        )?;
        let event_id = fixed_32(row.get(0)?, "relay event ID")?;
        let pubkey = fixed_32(row.get(1)?, "relay pubkey")?;
        let created_at: u64 = row.get(2)?;
        let created_at = i64::try_from(created_at).map_err(|_| {
            BoundedExecutionError::Invalid("relay created_at exceeds i64".to_owned())
        })?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO candidate_events(event_id,pubkey,created_at)
             VALUES(?1,?2,?3)",
            params![&event_id[..], &pubkey[..], created_at],
        )?;
        if inserted == 0 {
            let existing: (Vec<u8>, i64) = transaction.query_row(
                "SELECT pubkey,created_at FROM candidate_events WHERE event_id=?1",
                [&event_id[..]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if existing.0 != pubkey || existing.1 != created_at {
                return Err(BoundedExecutionError::Invalid(
                    "duplicate relay event ID has conflicting canonical fields".to_owned(),
                )
                .into());
            }
            continue;
        }
        let tags_json: String = row.get(4)?;
        let tags: Vec<Vec<String>> = serde_json::from_str(&tags_json).map_err(|error| {
            BoundedExecutionError::Invalid(format!("decode relay tags: {error}"))
        })?;
        let raw = tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some("r"))
            .count();
        let memberships = relay_memberships(&tags);
        stats.raw_relay_tags = checked_add(
            stats.raw_relay_tags,
            u64::try_from(raw).map_err(|_| {
                BoundedExecutionError::Invalid("relay tag count exceeds u64".to_owned())
            })?,
            "raw relay tags",
        )?;
        let valid_physical = tags
            .iter()
            .filter(|tag| {
                tag.first().map(String::as_str) == Some("r")
                    && tag
                        .get(1)
                        .and_then(|url| pensieve_core::relay_url::normalize_nip65_relay_url(url))
                        .is_some()
            })
            .count();
        stats.invalid_relay_tags = checked_add(
            stats.invalid_relay_tags,
            u64::try_from(raw.saturating_sub(valid_physical)).map_err(|_| {
                BoundedExecutionError::Invalid("invalid relay tag count exceeds u64".to_owned())
            })?,
            "invalid relay tags",
        )?;
        stats.duplicate_relay_tags = checked_add(
            stats.duplicate_relay_tags,
            u64::try_from(valid_physical.saturating_sub(memberships.len())).map_err(|_| {
                BoundedExecutionError::Invalid("duplicate relay tag count exceeds u64".to_owned())
            })?,
            "duplicate relay tags",
        )?;
        for membership in memberships {
            transaction.execute(
                "INSERT INTO candidate_memberships(event_id,relay_url,read,write)
                 VALUES(?1,?2,?3,?4)",
                params![
                    &event_id[..],
                    membership.relay_url,
                    membership.read,
                    membership.write
                ],
            )?;
        }
    }
    for input in inputs {
        transaction.execute(
            "INSERT INTO applied_objects(object_key,byte_size,row_count,sha256)
             VALUES(?1,?2,?3,?4)",
            params![
                input.identity,
                to_i64("relay input byte size", input.byte_size)?,
                to_i64("relay input row count", input.row_count)?,
                input.sha256,
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO batch_stats(
             batch_id,physical_rows,physical_relay_events,raw_relay_tags,
             invalid_relay_tags,duplicate_relay_tags)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            batch_id(inputs)?,
            to_i64("relay batch physical rows", stats.physical_rows)?,
            to_i64("relay batch physical events", stats.physical_relay_events)?,
            to_i64("relay batch raw tags", stats.raw_relay_tags)?,
            to_i64("relay batch invalid tags", stats.invalid_relay_tags)?,
            to_i64("relay batch duplicate tags", stats.duplicate_relay_tags)?,
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn configure_state(connection: &SqliteConnection, config: &RelayDistributionConfig) -> Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS metadata(
             key TEXT PRIMARY KEY,value TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS applied_objects(
             object_key TEXT PRIMARY KEY,byte_size INTEGER NOT NULL,
             row_count INTEGER NOT NULL,sha256 TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS batch_stats(
             batch_id TEXT PRIMARY KEY,physical_rows INTEGER NOT NULL,
             physical_relay_events INTEGER NOT NULL,raw_relay_tags INTEGER NOT NULL,
             invalid_relay_tags INTEGER NOT NULL,duplicate_relay_tags INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS candidate_events(
             event_id BLOB PRIMARY KEY CHECK(length(event_id)=32),
             pubkey BLOB NOT NULL CHECK(length(pubkey)=32),created_at INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS relay_candidate_winner
             ON candidate_events(pubkey,created_at DESC,event_id DESC);
         CREATE TABLE IF NOT EXISTS candidate_memberships(
             event_id BLOB NOT NULL REFERENCES candidate_events(event_id) ON DELETE CASCADE,
             relay_url TEXT NOT NULL,read INTEGER NOT NULL CHECK(read IN (0,1)),
             write INTEGER NOT NULL CHECK(write IN (0,1)),
             PRIMARY KEY(event_id,relay_url)
         ) WITHOUT ROWID;",
    )?;
    let page_size: u64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let max_pages = config.max_state_bytes / page_size;
    if max_pages == 0 {
        return Err(BoundedExecutionError::Invalid(
            "relay max state bytes are below one SQLite page".to_owned(),
        )
        .into());
    }
    connection.pragma_update(None, "max_page_count", max_pages)?;
    let cache_kib = config.sqlite_cache_bytes.div_ceil(1024);
    let negative_cache = -i64::try_from(cache_kib)
        .map_err(|_| BoundedExecutionError::Invalid("relay SQLite cache exceeds i64".to_owned()))?;
    connection.pragma_update(None, "cache_size", negative_cache)?;
    Ok(())
}

fn materialize_rows(
    state: &SqliteConnection,
    as_of: u64,
    minimum_users: u64,
) -> Result<Vec<RelayDistributionRow>> {
    let mut statement = state.prepare(
        "WITH ranked AS (
             SELECT event_id,pubkey,
                    row_number() OVER (
                        PARTITION BY pubkey ORDER BY created_at DESC,event_id DESC
                    ) AS rank
               FROM candidate_events WHERE created_at <= ?1
         ), winners AS (SELECT event_id,pubkey FROM ranked WHERE rank=1)
         SELECT memberships.relay_url,count(*) AS users,
                sum(memberships.read) AS reads,sum(memberships.write) AS writes
           FROM winners
           JOIN candidate_memberships memberships USING(event_id)
          GROUP BY memberships.relay_url HAVING count(*) >= ?2
          ORDER BY users DESC,memberships.relay_url ASC",
    )?;
    let rows = statement.query_map(
        params![
            to_i64("relay as-of", as_of)?,
            to_i64("relay minimum users", minimum_users)?
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (relay_url, users, reads, writes) = row?;
        Ok(RelayDistributionRow {
            relay_url,
            user_count: from_i64("relay users", users)?,
            read_count: from_i64("relay reads", reads)?,
            write_count: from_i64("relay writes", writes)?,
        })
    })
    .collect()
}

fn state_totals(state: &SqliteConnection, as_of: u64) -> Result<(u64, u64, u64, u64)> {
    let candidate_events = count(state, "SELECT count(*) FROM candidate_events", [])?;
    let eligible = count(
        state,
        "SELECT count(*) FROM candidate_events WHERE created_at <= ?1",
        [to_i64("relay total as-of", as_of)?],
    )?;
    let winners = count(
        state,
        "SELECT count(DISTINCT pubkey) FROM candidate_events WHERE created_at <= ?1",
        [to_i64("relay winner as-of", as_of)?],
    )?;
    let memberships = count(state, "SELECT count(*) FROM candidate_memberships", [])?;
    Ok((candidate_events, eligible, winners, memberships))
}

fn physical_totals(state: &SqliteConnection) -> Result<(u64, u64, u64, u64, u64)> {
    let row: (i64, i64, i64, i64, i64) = state.query_row(
        "SELECT coalesce(sum(physical_rows),0),
                coalesce(sum(physical_relay_events),0),
                coalesce(sum(raw_relay_tags),0),
                coalesce(sum(invalid_relay_tags),0),
                coalesce(sum(duplicate_relay_tags),0) FROM batch_stats",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    Ok((
        from_i64("relay physical rows", row.0)?,
        from_i64("relay physical events", row.1)?,
        from_i64("relay raw tags", row.2)?,
        from_i64("relay invalid tags", row.3)?,
        from_i64("relay duplicate tags", row.4)?,
    ))
}

fn count<const N: usize>(state: &SqliteConnection, sql: &str, parameters: [i64; N]) -> Result<u64> {
    let value: i64 = state.query_row(sql, rusqlite::params_from_iter(parameters), |row| {
        row.get(0)
    })?;
    from_i64("relay state count", value)
}

fn validate_evidence(evidence: &RelayDistributionEvidence, state: &SqliteConnection) -> Result<()> {
    validate_static_evidence(evidence)?;
    let rows = materialize_rows(state, evidence.as_of_epoch, evidence.minimum_users)?;
    let rows_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&rows).map_err(BoundedExecutionError::from)?,
    ));
    let totals = state_totals(state, evidence.as_of_epoch)?;
    let physical = physical_totals(state)?;
    let snapshot_id: Option<String> = state
        .query_row(
            "SELECT value FROM metadata WHERE key='snapshot_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let applied_objects = count(state, "SELECT count(*) FROM applied_objects", [])?;
    let input_rows = evidence.inputs.iter().try_fold(0_u64, |sum, input| {
        checked_add(sum, input.row_count, "relay input physical rows")
    })?;
    if snapshot_id.as_deref() != Some(evidence.snapshot_id.as_str())
        || evidence.rows != rows
        || evidence.rows_sha256 != rows_sha256
        || evidence.object_count != evidence.inputs.len() as u64
        || evidence.delta_object_count > evidence.object_count
        || (evidence.baseline_evidence_sha256.is_none()
            && evidence.delta_object_count != evidence.object_count)
        || evidence.applied_objects != evidence.object_count
        || applied_objects != evidence.object_count
        || input_rows != evidence.physical_rows_scanned
        || evidence.candidate_events != totals.0
        || evidence.eligible_candidate_events != totals.1
        || evidence.winning_pubkeys != totals.2
        || evidence.candidate_memberships != totals.3
        || evidence.physical_rows_scanned != physical.0
        || evidence.physical_relay_events != physical.1
        || evidence.raw_relay_tags != physical.2
        || evidence.invalid_relay_tags != physical.3
        || evidence.duplicate_relay_tags != physical.4
        || evidence.rows.iter().any(|row| {
            row.user_count < evidence.minimum_users
                || row.read_count > row.user_count
                || row.write_count > row.user_count
        })
        || evidence.rows.windows(2).any(|rows| {
            rows[0].user_count < rows[1].user_count
                || (rows[0].user_count == rows[1].user_count
                    && rows[0].relay_url >= rows[1].relay_url)
        })
    {
        return Err(BoundedExecutionError::Invalid(
            "relay distribution evidence does not reconcile".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn validate_static_evidence(evidence: &RelayDistributionEvidence) -> Result<()> {
    let rows_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&evidence.rows).map_err(BoundedExecutionError::from)?,
    ));
    if evidence.schema_version != EVIDENCE_SCHEMA_VERSION
        || evidence.runner_version != RUNNER_VERSION
        || evidence.status != "completed"
        || evidence.product_version != RELAY_DISTRIBUTION_VERSION
        || evidence.rows_sha256 != rows_sha256
        || evidence.object_count != evidence.inputs.len() as u64
        || evidence.applied_objects != evidence.object_count
        || evidence.delta_object_count > evidence.object_count
        || (evidence.baseline_evidence_sha256.is_none()
            && evidence.delta_object_count != evidence.object_count)
        || evidence.rows.iter().any(|row| {
            row.user_count < evidence.minimum_users
                || row.read_count > row.user_count
                || row.write_count > row.user_count
        })
        || evidence.rows.windows(2).any(|rows| {
            rows[0].user_count < rows[1].user_count
                || (rows[0].user_count == rows[1].user_count
                    && rows[0].relay_url >= rows[1].relay_url)
        })
    {
        return Err(BoundedExecutionError::Invalid(
            "relay distribution static evidence is invalid".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn validate_applied_inputs(state: &SqliteConnection, inputs: &[InputIdentity]) -> Result<()> {
    let expected = inputs
        .iter()
        .map(|input| (input.identity.as_str(), input))
        .collect::<BTreeMap<_, _>>();
    let mut statement = state.prepare(
        "SELECT object_key,byte_size,row_count,sha256 FROM applied_objects ORDER BY object_key",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let Some(input) = expected.get(key.as_str()) else {
            return Err(BoundedExecutionError::Invalid(format!(
                "relay target snapshot removed previously applied object {key}"
            ))
            .into());
        };
        if from_i64("relay applied bytes", row.get(1)?)? != input.byte_size
            || from_i64("relay applied rows", row.get(2)?)? != input.row_count
            || row.get::<_, String>(3)? != input.sha256
        {
            return Err(BoundedExecutionError::Invalid(format!(
                "relay applied object {key} changed immutable identity"
            ))
            .into());
        }
    }
    Ok(())
}

fn validate_inputs_present(state: &SqliteConnection, inputs: &[InputIdentity]) -> Result<()> {
    for input in inputs {
        let stored: Option<(i64, i64, String)> = state
            .query_row(
                "SELECT byte_size,row_count,sha256 FROM applied_objects WHERE object_key=?1",
                [&input.identity],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((bytes, rows, sha256)) = stored else {
            return Err(BoundedExecutionError::Invalid(format!(
                "relay state is missing baseline object {}",
                input.identity
            ))
            .into());
        };
        if from_i64("relay baseline bytes", bytes)? != input.byte_size
            || from_i64("relay baseline rows", rows)? != input.row_count
            || sha256 != input.sha256
        {
            return Err(BoundedExecutionError::Invalid(format!(
                "relay baseline object {} changed immutable identity",
                input.identity
            ))
            .into());
        }
    }
    Ok(())
}

fn applied_input_keys(state: &SqliteConnection) -> Result<BTreeSet<String>> {
    let mut statement = state.prepare("SELECT object_key FROM applied_objects")?;
    let rows = statement.query_map([], |row| row.get(0))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
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

fn batch_id(inputs: &[InputIdentity]) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(inputs).map_err(BoundedExecutionError::from)?,
    )))
}

fn enforce_state_size(state: &SqliteConnection, maximum: u64) -> Result<()> {
    let page_count: u64 = state.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: u64 = state.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let bytes = page_count.checked_mul(page_size).ok_or_else(|| {
        BoundedExecutionError::Invalid("relay SQLite size accounting overflowed".to_owned())
    })?;
    if bytes > maximum {
        return Err(BoundedExecutionError::Invalid(format!(
            "relay SQLite state uses {bytes} bytes, above configured maximum {maximum}"
        ))
        .into());
    }
    Ok(())
}

fn validate_config(
    snapshot: &ResolvedSnapshot,
    build: &BuildConfig,
    config: &RelayDistributionConfig,
) -> Result<()> {
    if snapshot.catalog.objects().len() != snapshot.locations.len()
        || build.as_of_epoch == 0
        || config.batch_limits.max_bytes == 0
        || config.batch_limits.max_rows == 0
        || config.max_state_bytes == 0
        || config.sqlite_cache_bytes == 0
        || config.minimum_users == 0
    {
        return Err(BoundedExecutionError::Invalid(
            "relay distribution snapshot or bounded configuration is invalid".to_owned(),
        )
        .into());
    }
    Ok(())
}

fn fixed_32(value: Vec<u8>, label: &str) -> Result<[u8; 32]> {
    value.try_into().map_err(|value: Vec<u8>| {
        BoundedExecutionError::Invalid(format!("{label} has {} bytes instead of 32", value.len()))
            .into()
    })
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
    use super::*;

    #[test]
    fn materialization_selects_one_deterministic_winner_per_pubkey() {
        let directory = tempfile::tempdir().expect("temporary state");
        let database = directory.path().join("relay.sqlite");
        let state = SqliteConnection::open(database).expect("open state");
        configure_state(
            &state,
            &RelayDistributionConfig {
                state_database: PathBuf::from("unused"),
                batch_limits: BatchLimits {
                    max_bytes: 1,
                    max_rows: 1,
                },
                max_state_bytes: 16 * 1024 * 1024,
                sqlite_cache_bytes: 1024 * 1024,
                minimum_users: 1,
                disk_reserve_bytes: 0,
            },
        )
        .expect("configure state");
        let pubkey_a = [1_u8; 32];
        let pubkey_b = [2_u8; 32];
        let old = [1_u8; 32];
        let tie_winner = [2_u8; 32];
        let future = [3_u8; 32];
        let second_user = [4_u8; 32];
        for (event, pubkey, created_at) in [
            (old, pubkey_a, 100_i64),
            (tie_winner, pubkey_a, 100),
            (future, pubkey_a, 300),
            (second_user, pubkey_b, 100),
        ] {
            state
                .execute(
                    "INSERT INTO candidate_events(event_id,pubkey,created_at) VALUES(?1,?2,?3)",
                    params![&event[..], &pubkey[..], created_at],
                )
                .expect("insert candidate");
        }
        for (event, relay, read, write) in [
            (old, "wss://old.example", true, true),
            (tie_winner, "wss://shared.example", true, false),
            (future, "wss://future.example", true, true),
            (second_user, "wss://shared.example", false, true),
        ] {
            state
                .execute(
                    "INSERT INTO candidate_memberships(event_id,relay_url,read,write)
                     VALUES(?1,?2,?3,?4)",
                    params![&event[..], relay, read, write],
                )
                .expect("insert membership");
        }
        assert_eq!(
            materialize_rows(&state, 200, 2).expect("materialize"),
            vec![RelayDistributionRow {
                relay_url: "wss://shared.example".to_owned(),
                user_count: 2,
                read_count: 1,
                write_count: 1,
            }]
        );
    }
}
