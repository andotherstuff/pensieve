//! Attribute every frozen Parquet-only event ID to immutable catalog objects.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::{Parser, ValueEnum};
use duckdb::{Connection, params};
use pensieve_lake::{ActiveRawSnapshot, CatalogObject, read_catalog_snapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-classify-parquet-gap-v1";
const ALIGNMENT_RUNNER_VERSION: &str = "pensieve-analytics-align-ids-v1";
const SHARD_COUNT: u16 = 256;
const ID_BYTES: u64 = 32;

#[derive(Debug, Parser)]
#[command(about = "Attribute exact Parquet-only IDs to catalog objects and source work")]
struct Args {
    /// Directional mismatch population to classify.
    #[arg(long, value_enum, default_value_t = CandidateDirection::ParquetOnly)]
    direction: CandidateDirection,
    /// Completed full directional alignment evidence.
    #[arg(long)]
    alignment_evidence: PathBuf,
    /// Required SHA-256 of the alignment evidence.
    #[arg(long)]
    alignment_evidence_sha256: String,
    /// Immutable alignment shard checkpoint directory.
    #[arg(long)]
    alignment_checkpoint_dir: PathBuf,
    /// Immutable full directional difference stream directory.
    #[arg(long)]
    difference_dir: PathBuf,
    /// Exact active-raw catalog used to build the frozen analytics snapshot.
    #[arg(long)]
    catalog: PathBuf,
    /// Frozen baseline catalog, required when classifying ClickHouse-only IDs
    /// against objects appended by a newer target catalog.
    #[arg(long)]
    baseline_catalog: Option<PathBuf>,
    /// Local root containing the catalog object keys.
    #[arg(long)]
    local_object_root: PathBuf,
    /// Resumable DuckDB classification database.
    #[arg(long)]
    work_database: PathBuf,
    /// Directory for immutable per-batch checkpoints.
    #[arg(long)]
    checkpoint_dir: PathBuf,
    /// Immutable final classification evidence.
    #[arg(long)]
    output: PathBuf,
    /// Number of catalog objects scanned in each committed transaction.
    #[arg(long, default_value_t = 16)]
    objects_per_batch: usize,
    /// DuckDB buffer-manager limit.
    #[arg(long, default_value = "4GB")]
    duckdb_memory_limit: String,
    /// Pause after each newly committed batch.
    #[arg(long, default_value_t = 2)]
    batch_delay_seconds: u64,
    /// Maximum unexplained example IDs retained in final evidence.
    #[arg(long, default_value_t = 20)]
    max_examples: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum CandidateDirection {
    ParquetOnly,
    ClickhouseOnly,
}

impl CandidateDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::ParquetOnly => "parquet_only",
            Self::ClickhouseOnly => "clickhouse_only",
        }
    }
}

#[derive(Debug, Deserialize)]
struct AlignmentEvidence {
    schema_version: u32,
    evidence_type: String,
    runner_version: String,
    status: String,
    snapshot_id: String,
    clickhouse_indexed_at_max_epoch: u64,
    parquet_only_count: u64,
    clickhouse_only_count: u64,
    shards_completed: u16,
}

#[derive(Debug, Deserialize)]
struct AlignmentCheckpoint {
    schema_version: u32,
    runner_version: String,
    snapshot_id: String,
    clickhouse_indexed_at_max_epoch: u64,
    shard: u16,
    parquet_only_count: u64,
    parquet_only_ids_file: Option<String>,
    parquet_only_ids_sha256: Option<String>,
    clickhouse_only_count: u64,
    clickhouse_only_ids_file: Option<String>,
    clickhouse_only_ids_sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct BatchCheckpoint {
    schema_version: u32,
    runner_version: String,
    catalog_snapshot_id: String,
    alignment_snapshot_id: String,
    batch_index: usize,
    object_start_inclusive: usize,
    object_end_exclusive: usize,
    object_count: usize,
    object_keys_sha256: String,
    matched_occurrences: u64,
    matched_unique_ids: u64,
    completed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct ObjectAttribution {
    object_key: String,
    work_unit_id: String,
    source_name: String,
    matched_unique_ids: u64,
}

#[derive(Debug, Serialize)]
struct SourceAttribution {
    work_unit_id: String,
    source_name: String,
    matched_occurrences: u64,
    matched_unique_ids: u64,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    candidate_direction: &'static str,
    alignment_evidence: String,
    alignment_evidence_sha256: String,
    alignment_snapshot_id: String,
    clickhouse_indexed_at_max_epoch: u64,
    alignment_parquet_only_population: u64,
    alignment_clickhouse_only_population: u64,
    catalog: String,
    catalog_sha256: String,
    catalog_snapshot_id: String,
    baseline_catalog: Option<String>,
    baseline_catalog_sha256: Option<String>,
    catalog_objects_scanned: usize,
    catalog_objects: usize,
    catalog_work_units: usize,
    local_object_root: String,
    candidate_ids: u64,
    matched_unique_ids: u64,
    matched_occurrences: u64,
    ids_in_multiple_objects: u64,
    ids_in_multiple_work_units: u64,
    residual_unattributed_ids: u64,
    attribution_percent: f64,
    objects_with_matches: usize,
    work_units_with_matches: usize,
    object_attribution: Vec<ObjectAttribution>,
    source_attribution: Vec<SourceAttribution>,
    residual_examples: Vec<String>,
    work_database: String,
    checkpoint_directory: String,
    completed_batches: usize,
    note: &'static str,
}

struct ClassificationScope {
    objects: Vec<CatalogObject>,
    baseline_catalog: Option<String>,
    baseline_catalog_sha256: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Parquet-gap classification failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    fs::create_dir_all(&args.checkpoint_dir)?;
    ensure!(
        !args.output.exists(),
        "refusing to replace immutable evidence {}",
        args.output.display()
    );

    let alignment_bytes = fs::read(&args.alignment_evidence)?;
    let alignment_sha256 = sha256_bytes(&alignment_bytes);
    ensure!(
        alignment_sha256 == args.alignment_evidence_sha256,
        "alignment evidence SHA-256 mismatch"
    );
    let alignment: AlignmentEvidence = serde_json::from_slice(&alignment_bytes)?;
    validate_alignment(&alignment)?;

    let catalog_bytes = fs::read(&args.catalog)?;
    let catalog_sha256 = sha256_bytes(&catalog_bytes);
    let catalog = read_catalog_snapshot(&args.catalog).context("read active-raw catalog")?;
    let scope = classification_scope(&args, &alignment, &catalog)?;
    let source_names = source_names(&catalog);
    validate_catalog_objects(&args, &scope.objects, &source_names)?;

    let streams = validate_difference_streams(&args, &alignment)?;
    let is_new = !args.work_database.exists();
    let connection = Connection::open(&args.work_database)?;
    configure_duckdb(&connection, &args)?;
    if is_new {
        initialize_work_database(
            &connection,
            &args,
            &alignment,
            &alignment_sha256,
            &catalog_sha256,
            scope.baseline_catalog_sha256.as_deref(),
            &streams,
        )?;
    } else {
        validate_work_database(
            &connection,
            &args,
            &alignment,
            &alignment_sha256,
            &catalog_sha256,
            scope.baseline_catalog_sha256.as_deref(),
        )?;
    }

    let objects = &scope.objects;
    let total_batches = objects.len().div_ceil(args.objects_per_batch);
    for (batch_index, batch) in objects.chunks(args.objects_per_batch).enumerate() {
        let start = batch_index * args.objects_per_batch;
        let checkpoint_path = args
            .checkpoint_dir
            .join(format!("batch-{batch_index:05}.json"));
        let expected_sha = object_keys_sha256(batch);
        let checkpoint = processed_batch(&connection, batch_index)?;
        let resumed = if let Some(checkpoint) = checkpoint {
            validate_batch_checkpoint(
                &checkpoint,
                &alignment.snapshot_id,
                &catalog.snapshot_id,
                batch_index,
                start,
                batch.len(),
                &expected_sha,
            )?;
            true
        } else {
            process_batch(
                &connection,
                &args,
                batch_index,
                start,
                batch,
                &source_names,
                &expected_sha,
                &alignment.snapshot_id,
                &catalog.snapshot_id,
            )?;
            false
        };
        let checkpoint = processed_batch(&connection, batch_index)?
            .context("committed batch checkpoint disappeared")?;
        if checkpoint_path.exists() {
            let file_checkpoint: BatchCheckpoint =
                serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
            ensure!(
                serde_json::to_vec(&file_checkpoint)? == serde_json::to_vec(&checkpoint)?,
                "immutable batch checkpoint differs from committed state"
            );
        } else {
            write_json_immutable(&checkpoint_path, &checkpoint, "batch checkpoint")?;
        }
        eprintln!(
            "catalog batch {} of {} completed: objects={} matched_occurrences={} matched_unique={}{}",
            batch_index + 1,
            total_batches,
            batch.len(),
            checkpoint.matched_occurrences,
            checkpoint.matched_unique_ids,
            if resumed { " (resumed)" } else { "" }
        );
        if !resumed && args.batch_delay_seconds > 0 {
            thread::sleep(Duration::from_secs(args.batch_delay_seconds));
        }
    }

    let evidence = build_evidence(
        &connection,
        &args,
        &alignment,
        alignment_sha256,
        &catalog,
        catalog_sha256,
        scope.baseline_catalog,
        scope.baseline_catalog_sha256,
        objects.len(),
        total_batches,
    )?;
    write_json_immutable(&args.output, &evidence, "classification evidence")?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    ensure!(
        args.alignment_evidence.is_file(),
        "alignment evidence is not a file"
    );
    ensure!(
        args.alignment_checkpoint_dir.is_dir(),
        "alignment checkpoint directory is missing"
    );
    ensure!(
        args.difference_dir.is_dir(),
        "difference directory is missing"
    );
    ensure!(args.catalog.is_file(), "catalog is not a file");
    match args.direction {
        CandidateDirection::ParquetOnly => ensure!(
            args.baseline_catalog.is_none(),
            "baseline catalog is only valid for ClickHouse-only classification"
        ),
        CandidateDirection::ClickhouseOnly => ensure!(
            args.baseline_catalog
                .as_ref()
                .is_some_and(|path| path.is_file()),
            "ClickHouse-only classification requires a baseline catalog file"
        ),
    }
    ensure!(
        args.local_object_root.is_dir(),
        "local object root is missing"
    );
    ensure!(
        args.objects_per_batch > 0,
        "objects per batch must be positive"
    );
    ensure!(
        args.alignment_evidence_sha256.len() == 64
            && args
                .alignment_evidence_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "alignment evidence SHA-256 must be lowercase hexadecimal"
    );
    Ok(())
}

fn validate_alignment(evidence: &AlignmentEvidence) -> Result<()> {
    ensure!(evidence.schema_version == 1, "unsupported alignment schema");
    ensure!(
        evidence.evidence_type == "pensieve-clickhouse-parquet-id-parity-v1",
        "unexpected alignment evidence type"
    );
    ensure!(
        evidence.runner_version == ALIGNMENT_RUNNER_VERSION,
        "unexpected alignment runner"
    );
    ensure!(
        evidence.status == "failed",
        "alignment is not a parity failure"
    );
    ensure!(
        evidence.shards_completed == SHARD_COUNT,
        "alignment is incomplete"
    );
    Ok(())
}

fn candidate_population(args: &Args, alignment: &AlignmentEvidence) -> u64 {
    match args.direction {
        CandidateDirection::ParquetOnly => alignment.parquet_only_count,
        CandidateDirection::ClickhouseOnly => alignment.clickhouse_only_count,
    }
}

fn source_names(catalog: &ActiveRawSnapshot) -> BTreeMap<String, String> {
    catalog
        .work_units()
        .iter()
        .map(|work| (work.work_unit_id.clone(), work.source_name.clone()))
        .collect()
}

fn classification_scope(
    args: &Args,
    alignment: &AlignmentEvidence,
    target: &ActiveRawSnapshot,
) -> Result<ClassificationScope> {
    match args.direction {
        CandidateDirection::ParquetOnly => {
            ensure!(
                target.snapshot_id == alignment.snapshot_id,
                "catalog and alignment snapshot IDs differ"
            );
            Ok(ClassificationScope {
                objects: target.objects().to_vec(),
                baseline_catalog: None,
                baseline_catalog_sha256: None,
            })
        }
        CandidateDirection::ClickhouseOnly => {
            let baseline_path = args
                .baseline_catalog
                .as_ref()
                .context("baseline catalog is required")?;
            let baseline_bytes = fs::read(baseline_path)?;
            let baseline_sha256 = sha256_bytes(&baseline_bytes);
            let baseline =
                read_catalog_snapshot(baseline_path).context("read baseline active-raw catalog")?;
            ensure!(
                baseline.snapshot_id == alignment.snapshot_id,
                "baseline catalog and alignment snapshot IDs differ"
            );
            ensure!(
                target.snapshot_id != baseline.snapshot_id,
                "target catalog does not advance the baseline"
            );
            let mut baseline_objects: BTreeMap<_, _> = baseline
                .objects()
                .iter()
                .map(|object| (object.object_key.as_str(), object))
                .collect();
            let mut added = Vec::new();
            for object in target.objects() {
                match baseline_objects.remove(object.object_key.as_str()) {
                    None => added.push(object.clone()),
                    Some(previous) => ensure!(
                        previous == object,
                        "immutable catalog object changed: {}",
                        object.object_key
                    ),
                }
            }
            ensure!(
                baseline_objects.is_empty(),
                "target catalog removes baseline objects"
            );
            ensure!(!added.is_empty(), "target catalog adds no objects");
            Ok(ClassificationScope {
                objects: added,
                baseline_catalog: Some(baseline_path.display().to_string()),
                baseline_catalog_sha256: Some(baseline_sha256),
            })
        }
    }
}

fn validate_catalog_objects(
    args: &Args,
    objects: &[CatalogObject],
    source_names: &BTreeMap<String, String>,
) -> Result<()> {
    for object in objects {
        ensure!(
            source_names.contains_key(&object.work_unit_id),
            "catalog object references unknown work unit"
        );
        let path = args.local_object_root.join(&object.object_key);
        let metadata = path
            .metadata()
            .with_context(|| format!("inspect catalog object {}", path.display()))?;
        ensure!(
            metadata.is_file(),
            "catalog object is not a file: {}",
            path.display()
        );
        ensure!(
            metadata.len() == object.byte_size,
            "catalog object size mismatch: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_difference_streams(args: &Args, alignment: &AlignmentEvidence) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(usize::from(SHARD_COUNT));
    let mut total = 0_u64;
    for shard in 0..SHARD_COUNT {
        let checkpoint_path = args
            .alignment_checkpoint_dir
            .join(format!("shard-{shard:03}.json"));
        let checkpoint: AlignmentCheckpoint = serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
        ensure!(
            checkpoint.schema_version == 1
                && checkpoint.runner_version == ALIGNMENT_RUNNER_VERSION
                && checkpoint.snapshot_id == alignment.snapshot_id
                && checkpoint.clickhouse_indexed_at_max_epoch
                    == alignment.clickhouse_indexed_at_max_epoch
                && checkpoint.shard == shard,
            "alignment checkpoint identity mismatch for shard {shard}"
        );
        let (file_name, expected_path, expected_count, expected_sha256) = match args.direction {
            CandidateDirection::ParquetOnly => (
                "parquet-only",
                checkpoint.parquet_only_ids_file.as_deref(),
                checkpoint.parquet_only_count,
                checkpoint.parquet_only_ids_sha256.as_deref(),
            ),
            CandidateDirection::ClickhouseOnly => (
                "clickhouse-only",
                checkpoint.clickhouse_only_ids_file.as_deref(),
                checkpoint.clickhouse_only_count,
                checkpoint.clickhouse_only_ids_sha256.as_deref(),
            ),
        };
        let path = args
            .difference_dir
            .join(format!("shard-{shard:03}.{file_name}.ids"));
        ensure!(
            expected_path == Some(path.to_string_lossy().as_ref()),
            "directional stream path mismatch for shard {shard}"
        );
        let expected_size = expected_count
            .checked_mul(ID_BYTES)
            .context("directional stream size overflow")?;
        ensure!(
            path.metadata()?.len() == expected_size,
            "directional stream size mismatch for shard {shard}"
        );
        ensure!(
            expected_sha256 == Some(&sha256_file(&path)?),
            "directional stream digest mismatch for shard {shard}"
        );
        total = total
            .checked_add(expected_count)
            .context("candidate count overflow")?;
        paths.push(path);
    }
    ensure!(
        total
            == match args.direction {
                CandidateDirection::ParquetOnly => alignment.parquet_only_count,
                CandidateDirection::ClickhouseOnly => alignment.clickhouse_only_count,
            },
        "directional streams do not reproduce the alignment count"
    );
    Ok(paths)
}

fn configure_duckdb(connection: &Connection, args: &Args) -> Result<()> {
    let temp_directory = args.work_database.with_extension("tmp");
    fs::create_dir_all(&temp_directory)?;
    connection.execute_batch(&format!(
        "SET threads = 1; SET memory_limit = {}; SET preserve_insertion_order = false; SET temp_directory = {};",
        sql_string(&args.duckdb_memory_limit),
        sql_string(&temp_directory.display().to_string())
    ))?;
    Ok(())
}

fn initialize_work_database(
    connection: &Connection,
    args: &Args,
    alignment: &AlignmentEvidence,
    alignment_sha256: &str,
    catalog_sha256: &str,
    baseline_catalog_sha256: Option<&str>,
    streams: &[PathBuf],
) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE run_metadata (key VARCHAR PRIMARY KEY, value VARCHAR NOT NULL);
         CREATE TABLE candidates (id BLOB PRIMARY KEY);
         CREATE TABLE matches (
             id BLOB NOT NULL,
             object_key VARCHAR NOT NULL,
             work_unit_id VARCHAR NOT NULL,
             source_name VARCHAR NOT NULL,
             PRIMARY KEY (id, object_key)
         );
         CREATE TABLE processed_batches (
             schema_version UINTEGER NOT NULL,
             runner_version VARCHAR NOT NULL,
             catalog_snapshot_id VARCHAR NOT NULL,
             alignment_snapshot_id VARCHAR NOT NULL,
             batch_index UINTEGER PRIMARY KEY,
             object_start_inclusive UBIGINT NOT NULL,
             object_end_exclusive UBIGINT NOT NULL,
             object_count UBIGINT NOT NULL,
             object_keys_sha256 VARCHAR NOT NULL,
             matched_occurrences UBIGINT NOT NULL,
             matched_unique_ids UBIGINT NOT NULL,
             completed_at VARCHAR NOT NULL
         );",
    )?;
    let load_result = (|| -> Result<()> {
        let mut appender = connection.appender("candidates")?;
        let mut previous: Option<[u8; ID_BYTES as usize]> = None;
        let mut count = 0_u64;
        for path in streams {
            let mut reader = IdReader::open(path)?;
            while let Some(id) = reader.next_id()? {
                if let Some(previous) = previous {
                    ensure!(
                        previous < id,
                        "candidate IDs are not globally sorted and unique"
                    );
                }
                appender.append_row(params![id.as_slice()])?;
                previous = Some(id);
                count += 1;
            }
        }
        appender.flush()?;
        ensure!(
            count == candidate_population(args, alignment),
            "candidate load count mismatch"
        );
        connection.execute(
            "INSERT INTO run_metadata VALUES
             ('runner_version', ?), ('candidate_direction', ?),
             ('alignment_snapshot_id', ?), ('alignment_evidence_sha256', ?),
             ('catalog_sha256', ?), ('baseline_catalog_sha256', ?),
             ('candidate_count', ?)",
            params![
                RUNNER_VERSION,
                args.direction.as_str(),
                alignment.snapshot_id,
                alignment_sha256,
                catalog_sha256,
                baseline_catalog_sha256.unwrap_or("none"),
                count.to_string()
            ],
        )?;
        Ok(())
    })();
    match load_result {
        Ok(()) => connection.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    connection.execute_batch("CHECKPOINT")?;
    eprintln!(
        "loaded {} exact {} candidate IDs",
        candidate_population(args, alignment),
        args.direction.as_str()
    );
    ensure!(
        args.work_database.is_file(),
        "work database was not created"
    );
    Ok(())
}

fn validate_work_database(
    connection: &Connection,
    args: &Args,
    alignment: &AlignmentEvidence,
    alignment_sha256: &str,
    catalog_sha256: &str,
    baseline_catalog_sha256: Option<&str>,
) -> Result<()> {
    for (key, expected) in [
        ("runner_version", RUNNER_VERSION.to_owned()),
        ("candidate_direction", args.direction.as_str().to_owned()),
        ("alignment_snapshot_id", alignment.snapshot_id.clone()),
        ("alignment_evidence_sha256", alignment_sha256.to_owned()),
        ("catalog_sha256", catalog_sha256.to_owned()),
        (
            "baseline_catalog_sha256",
            baseline_catalog_sha256.unwrap_or("none").to_owned(),
        ),
        (
            "candidate_count",
            candidate_population(args, alignment).to_string(),
        ),
    ] {
        let actual: String = connection.query_row(
            "SELECT value FROM run_metadata WHERE key = ?",
            params![key],
            |row| row.get(0),
        )?;
        ensure!(
            actual == expected,
            "work database metadata mismatch for {key}"
        );
    }
    let count: u64 =
        connection.query_row("SELECT count(*) FROM candidates", [], |row| row.get(0))?;
    ensure!(
        count == candidate_population(args, alignment),
        "work database candidate count mismatch"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_batch(
    connection: &Connection,
    args: &Args,
    batch_index: usize,
    start: usize,
    batch: &[CatalogObject],
    source_names: &BTreeMap<String, String>,
    object_keys_sha256: &str,
    alignment_snapshot_id: &str,
    catalog_snapshot_id: &str,
) -> Result<()> {
    connection.execute_batch(
        "DROP TABLE IF EXISTS batch_objects;
         CREATE TEMP TABLE batch_objects (
             filename VARCHAR PRIMARY KEY,
             object_key VARCHAR NOT NULL,
             work_unit_id VARCHAR NOT NULL,
             source_name VARCHAR NOT NULL
         );",
    )?;
    {
        let mut appender = connection.appender("batch_objects")?;
        for object in batch {
            let path = args.local_object_root.join(&object.object_key);
            appender.append_row(params![
                path.to_string_lossy().as_ref(),
                object.object_key,
                object.work_unit_id,
                source_names
                    .get(&object.work_unit_id)
                    .context("missing source name")?
            ])?;
        }
        appender.flush()?;
    }
    let paths = batch
        .iter()
        .map(|object| {
            sql_string(
                &args
                    .local_object_root
                    .join(&object.object_key)
                    .display()
                    .to_string(),
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch("BEGIN")?;
    let result = (|| -> Result<(u64, u64)> {
        connection.execute_batch(&format!(
            "INSERT OR IGNORE INTO matches
             SELECT DISTINCT p.id, o.object_key, o.work_unit_id, o.source_name
             FROM read_parquet([{paths}], union_by_name = false, filename = true) p
             INNER JOIN candidates c USING (id)
             INNER JOIN batch_objects o ON p.filename = o.filename;"
        ))?;
        let (occurrences, unique): (u64, u64) = connection.query_row(
            "SELECT count(*), count(DISTINCT matches.id)
             FROM matches INNER JOIN batch_objects USING (object_key)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        connection.execute(
            "INSERT INTO processed_batches VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                SCHEMA_VERSION,
                RUNNER_VERSION,
                catalog_snapshot_id,
                alignment_snapshot_id,
                u64::try_from(batch_index)?,
                u64::try_from(start)?,
                u64::try_from(start + batch.len())?,
                u64::try_from(batch.len())?,
                object_keys_sha256,
                occurrences,
                unique,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok((occurrences, unique))
    })();
    match result {
        Ok(_) => connection.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    Ok(())
}

fn processed_batch(connection: &Connection, batch_index: usize) -> Result<Option<BatchCheckpoint>> {
    let mut statement = connection.prepare(
        "SELECT schema_version, runner_version, catalog_snapshot_id,
                alignment_snapshot_id, batch_index, object_start_inclusive,
                object_end_exclusive, object_count, object_keys_sha256,
                matched_occurrences, matched_unique_ids, completed_at
         FROM processed_batches WHERE batch_index = ?",
    )?;
    let mut rows = statement.query(params![u64::try_from(batch_index)?])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(BatchCheckpoint {
        schema_version: row.get(0)?,
        runner_version: row.get(1)?,
        catalog_snapshot_id: row.get(2)?,
        alignment_snapshot_id: row.get(3)?,
        batch_index: usize::try_from(row.get::<_, u64>(4)?)?,
        object_start_inclusive: usize::try_from(row.get::<_, u64>(5)?)?,
        object_end_exclusive: usize::try_from(row.get::<_, u64>(6)?)?,
        object_count: usize::try_from(row.get::<_, u64>(7)?)?,
        object_keys_sha256: row.get(8)?,
        matched_occurrences: row.get(9)?,
        matched_unique_ids: row.get(10)?,
        completed_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(11)?)?.with_timezone(&Utc),
    }))
}

#[allow(clippy::too_many_arguments)]
fn validate_batch_checkpoint(
    checkpoint: &BatchCheckpoint,
    alignment_snapshot_id: &str,
    catalog_snapshot_id: &str,
    batch_index: usize,
    start: usize,
    object_count: usize,
    object_keys_sha256: &str,
) -> Result<()> {
    ensure!(
        checkpoint.schema_version == SCHEMA_VERSION
            && checkpoint.runner_version == RUNNER_VERSION
            && checkpoint.alignment_snapshot_id == alignment_snapshot_id
            && checkpoint.catalog_snapshot_id == catalog_snapshot_id
            && checkpoint.batch_index == batch_index
            && checkpoint.object_start_inclusive == start
            && checkpoint.object_end_exclusive == start + object_count
            && checkpoint.object_count == object_count
            && checkpoint.object_keys_sha256 == object_keys_sha256,
        "batch checkpoint identity mismatch for batch {batch_index}"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_evidence(
    connection: &Connection,
    args: &Args,
    alignment: &AlignmentEvidence,
    alignment_sha256: String,
    catalog: &ActiveRawSnapshot,
    catalog_sha256: String,
    baseline_catalog: Option<String>,
    baseline_catalog_sha256: Option<String>,
    catalog_objects_scanned: usize,
    total_batches: usize,
) -> Result<Evidence> {
    let candidate_ids: u64 =
        connection.query_row("SELECT count(*) FROM candidates", [], |row| row.get(0))?;
    let (matched_occurrences, matched_unique_ids): (u64, u64) = connection.query_row(
        "SELECT count(*), count(DISTINCT id) FROM matches",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let ids_in_multiple_objects: u64 = connection.query_row(
        "SELECT count(*) FROM (SELECT id FROM matches GROUP BY id HAVING count(*) > 1)",
        [],
        |row| row.get(0),
    )?;
    let ids_in_multiple_work_units: u64 = connection.query_row(
        "SELECT count(*) FROM (
             SELECT id FROM matches GROUP BY id HAVING count(DISTINCT work_unit_id) > 1
         )",
        [],
        |row| row.get(0),
    )?;
    let residual = candidate_ids
        .checked_sub(matched_unique_ids)
        .context("matched unique IDs exceed candidates")?;
    let object_attribution = query_object_attribution(connection)?;
    let source_attribution = query_source_attribution(connection)?;
    let residual_examples = query_residual_examples(connection, args.max_examples)?;
    let completed_batches: u64 =
        connection.query_row("SELECT count(*) FROM processed_batches", [], |row| {
            row.get(0)
        })?;
    ensure!(
        completed_batches == u64::try_from(total_batches)?,
        "not all catalog batches are complete"
    );
    Ok(Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "completed",
        generated_at: Utc::now(),
        candidate_direction: args.direction.as_str(),
        alignment_evidence: args.alignment_evidence.display().to_string(),
        alignment_evidence_sha256: alignment_sha256,
        alignment_snapshot_id: alignment.snapshot_id.clone(),
        clickhouse_indexed_at_max_epoch: alignment.clickhouse_indexed_at_max_epoch,
        alignment_parquet_only_population: alignment.parquet_only_count,
        alignment_clickhouse_only_population: alignment.clickhouse_only_count,
        catalog: args.catalog.display().to_string(),
        catalog_sha256,
        catalog_snapshot_id: catalog.snapshot_id.clone(),
        baseline_catalog,
        baseline_catalog_sha256,
        catalog_objects_scanned,
        catalog_objects: catalog.objects().len(),
        catalog_work_units: catalog.work_units().len(),
        local_object_root: args.local_object_root.display().to_string(),
        candidate_ids,
        matched_unique_ids,
        matched_occurrences,
        ids_in_multiple_objects,
        ids_in_multiple_work_units,
        residual_unattributed_ids: residual,
        attribution_percent: percent(matched_unique_ids, candidate_ids),
        objects_with_matches: object_attribution.len(),
        work_units_with_matches: source_attribution.len(),
        object_attribution,
        source_attribution,
        residual_examples,
        work_database: args.work_database.display().to_string(),
        checkpoint_directory: args.checkpoint_dir.display().to_string(),
        completed_batches: total_batches,
        note: match args.direction {
            CandidateDirection::ParquetOnly => {
                "Attribution counts preserve every catalog-object occurrence. Unique-ID counts de-duplicate overlaps, and residual IDs are exact candidates not found in any object from the catalog that built the frozen snapshot."
            }
            CandidateDirection::ClickhouseOnly => {
                "Matched IDs are timing differences found in immutable objects appended after the frozen baseline. Residual IDs remain absent from the selected newer Parquet catalog."
            }
        },
    })
}

fn query_object_attribution(connection: &Connection) -> Result<Vec<ObjectAttribution>> {
    let mut statement = connection.prepare(
        "SELECT object_key, min(work_unit_id), min(source_name), count(DISTINCT id)
         FROM matches GROUP BY object_key ORDER BY object_key",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ObjectAttribution {
            object_key: row.get(0)?,
            work_unit_id: row.get(1)?,
            source_name: row.get(2)?,
            matched_unique_ids: row.get(3)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn query_source_attribution(connection: &Connection) -> Result<Vec<SourceAttribution>> {
    let mut statement = connection.prepare(
        "SELECT work_unit_id, min(source_name), count(*), count(DISTINCT id)
         FROM matches GROUP BY work_unit_id ORDER BY work_unit_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SourceAttribution {
            work_unit_id: row.get(0)?,
            source_name: row.get(1)?,
            matched_occurrences: row.get(2)?,
            matched_unique_ids: row.get(3)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn query_residual_examples(connection: &Connection, limit: usize) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT lower(hex(c.id)) FROM candidates c ANTI JOIN matches m USING (id)
         ORDER BY c.id LIMIT ?",
    )?;
    let rows = statement.query_map(params![u64::try_from(limit)?], |row| row.get(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn object_keys_sha256(objects: &[CatalogObject]) -> String {
    let mut digest = Sha256::new();
    for object in objects {
        digest.update(object.object_key.as_bytes());
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 * 100.0) / denominator as f64
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn write_json_immutable(path: &Path, value: &impl Serialize, label: &str) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create immutable {label} {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

struct IdReader {
    reader: BufReader<File>,
}

impl IdReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
        })
    }

    fn next_id(&mut self) -> Result<Option<[u8; ID_BYTES as usize]>> {
        let mut id = [0_u8; ID_BYTES as usize];
        let mut offset = 0;
        while offset < id.len() {
            let read = self.reader.read(&mut id[offset..])?;
            if read == 0 {
                ensure!(offset == 0, "truncated fixed-width event ID stream");
                return Ok(None);
            }
            offset += read;
        }
        Ok(Some(id))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use duckdb::{Connection, params};
    use pensieve_lake::CatalogObject;
    use tempfile::tempdir;

    use super::{Args, CandidateDirection, object_keys_sha256, percent, process_batch, sql_string};

    fn object(key: &str) -> CatalogObject {
        CatalogObject {
            object_key: key.to_owned(),
            work_unit_id: "work".to_owned(),
            part_number: 0,
            byte_size: 1,
            sha256: "11".repeat(32),
            writer_version: "test".to_owned(),
            row_count: 1,
            min_created_at: None,
            max_created_at: None,
        }
    }

    #[test]
    fn object_batch_identity_depends_on_order_and_boundaries() {
        assert_ne!(
            object_keys_sha256(&[object("a"), object("b")]),
            object_keys_sha256(&[object("b"), object("a")])
        );
        assert_ne!(
            object_keys_sha256(&[object("a"), object("b")]),
            object_keys_sha256(&[object("ab")])
        );
    }

    #[test]
    fn percent_handles_an_empty_population() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(1, 4), 25.0);
    }

    #[test]
    fn parquet_filename_join_attributes_an_exact_candidate() {
        let directory = tempdir().unwrap();
        let object_key = "nostr/v1/raw/work/part.parquet";
        let object_path = directory.path().join(object_key);
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE source(id BLOB); INSERT INTO source VALUES (from_hex('{}')); COPY source TO {} (FORMAT parquet);
                 CREATE TABLE candidates(id BLOB PRIMARY KEY); INSERT INTO candidates SELECT id FROM source;
                 CREATE TABLE matches (id BLOB, object_key VARCHAR, work_unit_id VARCHAR, source_name VARCHAR, PRIMARY KEY(id, object_key));
                 CREATE TABLE processed_batches (
                     schema_version UINTEGER, runner_version VARCHAR, catalog_snapshot_id VARCHAR,
                     alignment_snapshot_id VARCHAR, batch_index UINTEGER PRIMARY KEY,
                     object_start_inclusive UBIGINT, object_end_exclusive UBIGINT,
                     object_count UBIGINT, object_keys_sha256 VARCHAR,
                     matched_occurrences UBIGINT, matched_unique_ids UBIGINT, completed_at VARCHAR
                 );",
                "11".repeat(32),
                sql_string(&object_path.display().to_string())
            ))
            .unwrap();
        let catalog_object = CatalogObject {
            object_key: object_key.to_owned(),
            work_unit_id: "work".to_owned(),
            part_number: 0,
            byte_size: object_path.metadata().unwrap().len(),
            sha256: "22".repeat(32),
            writer_version: "test".to_owned(),
            row_count: 1,
            min_created_at: None,
            max_created_at: None,
        };
        let args = Args {
            direction: CandidateDirection::ParquetOnly,
            alignment_evidence: PathBuf::new(),
            alignment_evidence_sha256: String::new(),
            alignment_checkpoint_dir: PathBuf::new(),
            difference_dir: PathBuf::new(),
            catalog: PathBuf::new(),
            baseline_catalog: None,
            local_object_root: directory.path().to_owned(),
            work_database: directory.path().join("work.duckdb"),
            checkpoint_dir: directory.path().join("checkpoints"),
            output: directory.path().join("evidence.json"),
            objects_per_batch: 1,
            duckdb_memory_limit: "1GB".to_owned(),
            batch_delay_seconds: 0,
            max_examples: 1,
        };
        let sources = BTreeMap::from([("work".to_owned(), "segment.notepack.gz".to_owned())]);
        let digest = object_keys_sha256(std::slice::from_ref(&catalog_object));
        process_batch(
            &connection,
            &args,
            0,
            0,
            &[catalog_object],
            &sources,
            &digest,
            "alignment",
            "catalog",
        )
        .unwrap();
        let row: (u64, String, String) = connection
            .query_row(
                "SELECT count(*), min(object_key), min(source_name) FROM matches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (1, object_key.to_owned(), "segment.notepack.gz".to_owned())
        );
        let committed: u64 = connection
            .query_row(
                "SELECT matched_unique_ids FROM processed_batches WHERE batch_index = ?",
                params![0_u64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(committed, 1);
    }
}
