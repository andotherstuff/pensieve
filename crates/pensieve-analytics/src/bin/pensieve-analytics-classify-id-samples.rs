//! Classify bounded directional ID-alignment examples against DuckDB checkpoints.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use duckdb::{AccessMode, Config, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPORT_SCHEMA_VERSION: u32 = 1;
const CLASSIFIER_VERSION: &str = "pensieve-analytics-classify-id-samples-v1";
const ALIGNMENT_EVIDENCE_TYPE: &str = "pensieve-clickhouse-parquet-id-parity-v1";
const EXPECTED_SHARDS: u16 = 256;

#[derive(Debug, Parser)]
#[command(about = "Classify bounded alignment mismatch examples in DuckDB checkpoints")]
struct Args {
    /// Failed exact-alignment evidence whose examples are being classified.
    #[arg(long)]
    alignment_evidence: PathBuf,
    /// Directory containing all immutable exact-alignment shard checkpoints.
    #[arg(long)]
    alignment_checkpoint_dir: PathBuf,
    /// Frozen DuckDB checkpoint used by the exact alignment.
    #[arg(long)]
    frozen_database: PathBuf,
    /// Newer DuckDB checkpoint used to distinguish cutoff drift from durable absence.
    #[arg(long)]
    current_database: PathBuf,
    /// Immutable JSON report path.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
struct AlignmentEvidence {
    schema_version: u32,
    evidence_type: String,
    status: String,
    snapshot_id: String,
    clickhouse_indexed_at_max_epoch: u64,
    id_keyed_equal: bool,
    shards_completed: u16,
}

#[derive(Debug, Deserialize)]
struct ShardCheckpoint {
    schema_version: u32,
    snapshot_id: String,
    clickhouse_indexed_at_max_epoch: u64,
    shard: u16,
    parquet_only_count: u64,
    clickhouse_only_count: u64,
    parquet_only_examples: Vec<String>,
    clickhouse_only_examples: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    ParquetOnly,
    ClickhouseOnly,
}

impl Direction {
    fn as_sql(self) -> &'static str {
        match self {
            Self::ParquetOnly => "parquet_only",
            Self::ClickhouseOnly => "clickhouse_only",
        }
    }
}

#[derive(Debug)]
struct Sample {
    id: String,
    direction: Direction,
}

#[derive(Clone, Copy, Debug)]
struct EventMetadata {
    created_at: u64,
    kind: u16,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    classifier_version: &'static str,
    alignment: AlignmentInput,
    frozen_checkpoint: CheckpointInput,
    current_checkpoint: CheckpointInput,
    parquet_only_sample: DirectionReport,
    clickhouse_only_sample: DirectionReport,
    interpretation: Interpretation,
}

#[derive(Debug, Serialize)]
struct AlignmentInput {
    evidence_file: String,
    evidence_sha256: String,
    snapshot_id: String,
    clickhouse_indexed_at_max_epoch: u64,
    shards: u16,
    parquet_only_population: u64,
    clickhouse_only_population: u64,
}

#[derive(Debug, Serialize)]
struct CheckpointInput {
    path: String,
    snapshot_id: String,
    as_of_epoch: u64,
}

#[derive(Debug, Serialize)]
struct DirectionReport {
    requested_examples: usize,
    frozen_found: usize,
    current_found: usize,
    current_missing: usize,
    frozen_metadata: Option<MetadataDistribution>,
    current_metadata: Option<MetadataDistribution>,
}

#[derive(Debug, Serialize)]
struct MetadataDistribution {
    count: usize,
    min_created_at: u64,
    p10_created_at: u64,
    median_created_at: u64,
    p90_created_at: u64,
    max_created_at: u64,
    relative_to_frozen_as_of: BTreeMap<&'static str, usize>,
    top_kinds: Vec<KindCount>,
}

#[derive(Debug, Serialize)]
struct KindCount {
    kind: u16,
    count: usize,
}

#[derive(Debug, Serialize)]
struct Interpretation {
    clickhouse_only_now_in_parquet: usize,
    clickhouse_only_still_absent_from_parquet: usize,
    parquet_only_still_absent_from_current_parquet: usize,
    note: &'static str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ID sample classification failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        !args.output.exists(),
        "refusing to replace immutable report {}",
        args.output.display()
    );
    ensure!(
        args.frozen_database.is_file(),
        "frozen database is not a file"
    );
    ensure!(
        args.current_database.is_file(),
        "current database is not a file"
    );
    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)?;
    }

    let evidence_bytes = fs::read(&args.alignment_evidence).context("read alignment evidence")?;
    let evidence: AlignmentEvidence =
        serde_json::from_slice(&evidence_bytes).context("decode alignment evidence")?;
    validate_evidence(&evidence)?;
    let evidence_sha256 = hex::encode(Sha256::digest(&evidence_bytes));
    let (samples, parquet_only_population, clickhouse_only_population) =
        load_samples(&args.alignment_checkpoint_dir, &evidence)?;

    let frozen = open_read_only(&args.frozen_database)?;
    let current = open_read_only(&args.current_database)?;
    let frozen_state = checkpoint_state(&frozen).context("read frozen checkpoint state")?;
    let current_state = checkpoint_state(&current).context("read current checkpoint state")?;
    ensure!(
        frozen_state.snapshot_id == evidence.snapshot_id,
        "frozen checkpoint snapshot does not match alignment evidence"
    );
    let frozen_rows =
        load_metadata(&frozen, &samples).context("query frozen checkpoint samples")?;
    let current_rows =
        load_metadata(&current, &samples).context("query current checkpoint samples")?;

    let parquet_samples = samples
        .iter()
        .filter(|sample| sample.direction == Direction::ParquetOnly)
        .count();
    let clickhouse_samples = samples.len() - parquet_samples;
    let parquet_report = direction_report(
        Direction::ParquetOnly,
        parquet_samples,
        &frozen_rows,
        &current_rows,
        frozen_state.as_of_epoch,
    )?;
    let clickhouse_report = direction_report(
        Direction::ClickhouseOnly,
        clickhouse_samples,
        &frozen_rows,
        &current_rows,
        frozen_state.as_of_epoch,
    )?;
    ensure!(
        parquet_report.frozen_found == parquet_samples,
        "not every Parquet-only example exists in the frozen checkpoint"
    );
    ensure!(
        clickhouse_report.frozen_found == 0,
        "a ClickHouse-only example unexpectedly exists in the frozen checkpoint"
    );

    let report = Report {
        schema_version: REPORT_SCHEMA_VERSION,
        classifier_version: CLASSIFIER_VERSION,
        alignment: AlignmentInput {
            evidence_file: args.alignment_evidence.display().to_string(),
            evidence_sha256,
            snapshot_id: evidence.snapshot_id,
            clickhouse_indexed_at_max_epoch: evidence.clickhouse_indexed_at_max_epoch,
            shards: evidence.shards_completed,
            parquet_only_population,
            clickhouse_only_population,
        },
        frozen_checkpoint: CheckpointInput {
            path: args.frozen_database.display().to_string(),
            snapshot_id: frozen_state.snapshot_id,
            as_of_epoch: frozen_state.as_of_epoch,
        },
        current_checkpoint: CheckpointInput {
            path: args.current_database.display().to_string(),
            snapshot_id: current_state.snapshot_id,
            as_of_epoch: current_state.as_of_epoch,
        },
        interpretation: Interpretation {
            clickhouse_only_now_in_parquet: clickhouse_report.current_found,
            clickhouse_only_still_absent_from_parquet: clickhouse_report.current_missing,
            parquet_only_still_absent_from_current_parquet: parquet_report.current_missing,
            note: "Bounded examples are deterministic by ID range, not a complete mismatch export; use them to classify causes, not to replace exact population counts.",
        },
        parquet_only_sample: parquet_report,
        clickhouse_only_sample: clickhouse_report,
    };
    write_json_immutable(&args.output, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn validate_evidence(evidence: &AlignmentEvidence) -> Result<()> {
    ensure!(
        evidence.schema_version == 1,
        "unsupported alignment evidence schema"
    );
    ensure!(
        evidence.evidence_type == ALIGNMENT_EVIDENCE_TYPE,
        "unexpected alignment evidence type"
    );
    ensure!(
        evidence.status == "failed",
        "classification requires failed parity evidence"
    );
    ensure!(
        !evidence.id_keyed_equal,
        "classification requires unequal ID sets"
    );
    ensure!(
        evidence.shards_completed == EXPECTED_SHARDS,
        "alignment evidence is incomplete"
    );
    Ok(())
}

fn load_samples(directory: &Path, evidence: &AlignmentEvidence) -> Result<(Vec<Sample>, u64, u64)> {
    let mut samples = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut parquet_population = 0_u64;
    let mut clickhouse_population = 0_u64;
    for shard in 0..EXPECTED_SHARDS {
        let path = directory.join(format!("shard-{shard:03}.json"));
        let checkpoint: ShardCheckpoint = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            checkpoint.schema_version == 1,
            "shard {shard} schema mismatch"
        );
        ensure!(checkpoint.shard == shard, "shard {shard} index mismatch");
        ensure!(
            checkpoint.snapshot_id == evidence.snapshot_id,
            "shard {shard} snapshot mismatch"
        );
        ensure!(
            checkpoint.clickhouse_indexed_at_max_epoch == evidence.clickhouse_indexed_at_max_epoch,
            "shard {shard} barrier mismatch"
        );
        parquet_population = parquet_population
            .checked_add(checkpoint.parquet_only_count)
            .context("Parquet-only population overflow")?;
        clickhouse_population = clickhouse_population
            .checked_add(checkpoint.clickhouse_only_count)
            .context("ClickHouse-only population overflow")?;
        for (direction, ids) in [
            (Direction::ParquetOnly, checkpoint.parquet_only_examples),
            (
                Direction::ClickhouseOnly,
                checkpoint.clickhouse_only_examples,
            ),
        ] {
            for id in ids {
                validate_id(&id)?;
                ensure!(seen_ids.insert(id.clone()), "duplicate sampled ID {id}");
                samples.push(Sample { id, direction });
            }
        }
    }
    Ok((samples, parquet_population, clickhouse_population))
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid sampled event ID"
    );
    Ok(())
}

fn open_read_only(path: &Path) -> Result<Connection> {
    let config = Config::default().access_mode(AccessMode::ReadOnly)?;
    Connection::open_with_flags(path, config)
        .with_context(|| format!("open {} read-only", path.display()))
}

#[derive(Debug)]
struct CheckpointState {
    snapshot_id: String,
    as_of_epoch: u64,
}

fn checkpoint_state(connection: &Connection) -> Result<CheckpointState> {
    connection
        .query_row(
            "SELECT snapshot_id, as_of_epoch FROM analytics_state WHERE singleton = true",
            [],
            |row| {
                Ok(CheckpointState {
                    snapshot_id: row.get(0)?,
                    as_of_epoch: row.get(1)?,
                })
            },
        )
        .map_err(Into::into)
}

fn load_metadata(
    connection: &Connection,
    samples: &[Sample],
) -> Result<HashMap<(Direction, String), EventMetadata>> {
    let values = samples
        .iter()
        .map(|sample| {
            format!(
                "(from_hex('{}'), '{}')",
                sample.id,
                sample.direction.as_sql()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "
        WITH sampled(id, direction) AS (VALUES {values})
        SELECT lower(hex(events.id)), sampled.direction, events.created_at, events.kind
        FROM canonical_events AS events
        INNER JOIN sampled USING (id)
        "
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        let direction: String = row.get(1)?;
        Ok((
            row.get::<_, String>(0)?,
            direction,
            EventMetadata {
                created_at: row.get(2)?,
                kind: row.get(3)?,
            },
        ))
    })?;
    let mut output = HashMap::new();
    for row in rows {
        let (id, direction, metadata) = row?;
        let direction = match direction.as_str() {
            "parquet_only" => Direction::ParquetOnly,
            "clickhouse_only" => Direction::ClickhouseOnly,
            _ => anyhow::bail!("unexpected sample direction {direction}"),
        };
        ensure!(
            output.insert((direction, id.clone()), metadata).is_none(),
            "duplicate canonical event for sampled ID {id}"
        );
    }
    Ok(output)
}

fn direction_report(
    direction: Direction,
    requested: usize,
    frozen: &HashMap<(Direction, String), EventMetadata>,
    current: &HashMap<(Direction, String), EventMetadata>,
    frozen_as_of: u64,
) -> Result<DirectionReport> {
    let frozen_rows = frozen
        .iter()
        .filter_map(|((row_direction, _), metadata)| {
            (*row_direction == direction).then_some(*metadata)
        })
        .collect::<Vec<_>>();
    let current_rows = current
        .iter()
        .filter_map(|((row_direction, _), metadata)| {
            (*row_direction == direction).then_some(*metadata)
        })
        .collect::<Vec<_>>();
    ensure!(
        current_rows.len() <= requested,
        "current sample count exceeds request"
    );
    Ok(DirectionReport {
        requested_examples: requested,
        frozen_found: frozen_rows.len(),
        current_found: current_rows.len(),
        current_missing: requested - current_rows.len(),
        frozen_metadata: distribution(&frozen_rows, frozen_as_of),
        current_metadata: distribution(&current_rows, frozen_as_of),
    })
}

fn distribution(rows: &[EventMetadata], as_of: u64) -> Option<MetadataDistribution> {
    if rows.is_empty() {
        return None;
    }
    let mut timestamps = rows.iter().map(|row| row.created_at).collect::<Vec<_>>();
    timestamps.sort_unstable();
    let mut relative = BTreeMap::from([
        ("after_snapshot_as_of", 0),
        ("within_1d_before", 0),
        ("2_to_7d_before", 0),
        ("8_to_30d_before", 0),
        ("31_to_365d_before", 0),
        ("older_than_365d", 0),
    ]);
    let mut kinds = BTreeMap::<u16, usize>::new();
    for row in rows {
        let bucket = if row.created_at > as_of {
            "after_snapshot_as_of"
        } else {
            match as_of - row.created_at {
                0..=86_400 => "within_1d_before",
                86_401..=604_800 => "2_to_7d_before",
                604_801..=2_592_000 => "8_to_30d_before",
                2_592_001..=31_536_000 => "31_to_365d_before",
                _ => "older_than_365d",
            }
        };
        *relative
            .get_mut(bucket)
            .expect("all time buckets initialized") += 1;
        *kinds.entry(row.kind).or_default() += 1;
    }
    let mut top_kinds = kinds
        .into_iter()
        .map(|(kind, count)| KindCount { kind, count })
        .collect::<Vec<_>>();
    top_kinds.sort_by_key(|item| (std::cmp::Reverse(item.count), item.kind));
    top_kinds.truncate(20);
    Some(MetadataDistribution {
        count: rows.len(),
        min_created_at: timestamps[0],
        p10_created_at: percentile(&timestamps, 10),
        median_created_at: percentile(&timestamps, 50),
        p90_created_at: percentile(&timestamps, 90),
        max_created_at: *timestamps.last().expect("nonempty timestamps"),
        relative_to_frozen_as_of: relative,
        top_kinds,
    })
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let index = (sorted.len() - 1) * percentile / 100;
    sorted[index]
}

fn write_json_immutable(path: &Path, report: &Report) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_uses_exclusive_relative_time_buckets() {
        let rows = [
            EventMetadata {
                created_at: 101,
                kind: 1,
            },
            EventMetadata {
                created_at: 100,
                kind: 1,
            },
            EventMetadata {
                created_at: 10,
                kind: 2,
            },
        ];
        let result = distribution(&rows, 100).unwrap();
        assert_eq!(result.relative_to_frozen_as_of["after_snapshot_as_of"], 1);
        assert_eq!(result.relative_to_frozen_as_of["within_1d_before"], 2);
        assert_eq!(result.top_kinds[0].kind, 1);
        assert_eq!(result.top_kinds[0].count, 2);
    }

    #[test]
    fn percentile_is_nearest_lower_rank() {
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 10), 10);
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 50), 30);
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 90), 40);
    }
}
