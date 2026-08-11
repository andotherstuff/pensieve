//! Prove exact event-ID parity between a frozen DuckDB checkpoint and ClickHouse.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use duckdb::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const EVIDENCE_TYPE: &str = "pensieve-clickhouse-parquet-id-parity-v1";
const RUNNER_VERSION: &str = "pensieve-analytics-align-ids-v1";
const ID_BYTES: usize = 32;
const SHARD_COUNT: u16 = 256;

#[derive(Debug, Parser)]
#[command(about = "Prove exact event-ID parity for one immutable analytics snapshot")]
struct Args {
    /// Frozen DuckDB checkpoint containing canonical_events and analytics_state.
    #[arg(long)]
    source_database: PathBuf,
    /// Dedicated resumable DuckDB database used for the ID-sorted index.
    #[arg(long)]
    work_database: PathBuf,
    /// Directory containing immutable shard checkpoints and temporary exports.
    #[arg(long)]
    checkpoint_dir: PathBuf,
    /// Immutable final JSON evidence path.
    #[arg(long)]
    output: PathBuf,
    /// Optional directory for immutable full directional difference ID streams.
    #[arg(long)]
    difference_dir: Option<PathBuf>,
    /// Snapshot ID expected inside the frozen DuckDB checkpoint.
    #[arg(long)]
    snapshot_id: String,
    /// Maximum ClickHouse indexed_at epoch included in the comparison.
    #[arg(long)]
    clickhouse_indexed_at_max_epoch: u64,
    /// ClickHouse HTTP endpoint.
    #[arg(long, env = "CLICKHOUSE_URL", default_value = "http://localhost:8123")]
    clickhouse_url: String,
    /// ClickHouse database containing events_local.
    #[arg(long, env = "CLICKHOUSE_DATABASE", default_value = "nostr")]
    clickhouse_database: String,
    /// Optional ClickHouse user.
    #[arg(long, env = "CLICKHOUSE_USER")]
    clickhouse_user: Option<String>,
    /// Optional ClickHouse password; never written to evidence.
    #[arg(long, env = "CLICKHOUSE_PASSWORD")]
    clickhouse_password: Option<String>,
    /// DuckDB buffer-manager limit for index preparation and shard reads.
    #[arg(long, default_value = "8GB")]
    duckdb_memory_limit: String,
    /// Maximum ClickHouse memory per shard query in bytes.
    #[arg(long, default_value_t = 4 * 1024 * 1024 * 1024)]
    clickhouse_max_memory_usage: u64,
    /// Maximum ClickHouse query execution time per shard in seconds.
    #[arg(long, default_value_t = 21_600)]
    clickhouse_max_execution_time: u64,
    /// Pause after each newly completed shard to reduce sustained production load.
    #[arg(long, default_value_t = 15)]
    shard_delay_seconds: u64,
    /// Maximum directional mismatch examples retained per shard.
    #[arg(long, default_value_t = 10)]
    max_difference_examples: usize,
}

#[derive(Debug, Deserialize, Row)]
struct ClickhouseIdRow {
    id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct IndexMetadata {
    schema_version: u32,
    runner_version: String,
    snapshot_id: String,
    source_database: String,
    source_size_bytes: u64,
    event_count: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ShardCheckpoint {
    schema_version: u32,
    runner_version: String,
    snapshot_id: String,
    clickhouse_database: String,
    clickhouse_table: String,
    clickhouse_indexed_at_max_epoch: u64,
    shard: u16,
    lower_inclusive: String,
    upper_exclusive: Option<String>,
    parquet_count: u64,
    clickhouse_count: u64,
    parquet_sha256: String,
    clickhouse_sha256: String,
    parquet_only_count: u64,
    clickhouse_only_count: u64,
    parquet_only_examples: Vec<String>,
    clickhouse_only_examples: Vec<String>,
    #[serde(default)]
    parquet_only_ids_file: Option<String>,
    #[serde(default)]
    parquet_only_ids_sha256: Option<String>,
    #[serde(default)]
    clickhouse_only_ids_file: Option<String>,
    #[serde(default)]
    clickhouse_only_ids_sha256: Option<String>,
    id_keyed_equal: bool,
    completed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    evidence_type: &'static str,
    runner_version: &'static str,
    status: &'static str,
    snapshot_id: String,
    clickhouse_database: String,
    clickhouse_table: &'static str,
    clickhouse_indexed_at_max_epoch: u64,
    id_keyed_equal: bool,
    parquet_id_count: u64,
    clickhouse_id_count: u64,
    parquet_only_count: u64,
    clickhouse_only_count: u64,
    shards_completed: u16,
    checkpoint_directory: String,
    source_database: String,
    source_size_bytes: u64,
    difference_directory: Option<String>,
    generated_at: DateTime<Utc>,
}

#[derive(Debug)]
struct ExportSummary {
    count: u64,
    sha256: String,
}

#[derive(Default)]
struct DifferenceSummary {
    left_only_count: u64,
    right_only_count: u64,
    left_only_examples: Vec<String>,
    right_only_examples: Vec<String>,
    left_only_sha256: Option<String>,
    right_only_sha256: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("event-ID alignment failed: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    fs::create_dir_all(&args.checkpoint_dir).with_context(|| {
        format!(
            "create checkpoint directory {}",
            args.checkpoint_dir.display()
        )
    })?;
    if let Some(directory) = &args.difference_dir {
        fs::create_dir_all(directory)
            .with_context(|| format!("create difference directory {}", directory.display()))?;
    }
    ensure!(
        !args.output.exists(),
        "refusing to replace immutable evidence {}",
        args.output.display()
    );

    let metadata = prepare_index(&args)?;
    let client = connect_clickhouse(&args);
    let mut checkpoints = Vec::with_capacity(usize::from(SHARD_COUNT));
    for shard in 0..SHARD_COUNT {
        let checkpoint_path = args.checkpoint_dir.join(format!("shard-{shard:03}.json"));
        let (checkpoint, resumed) = if checkpoint_path.exists() {
            let checkpoint: ShardCheckpoint = serde_json::from_slice(
                &fs::read(&checkpoint_path)
                    .with_context(|| format!("read {}", checkpoint_path.display()))?,
            )
            .with_context(|| format!("decode {}", checkpoint_path.display()))?;
            validate_checkpoint(&args, shard, &checkpoint)?;
            (checkpoint, true)
        } else {
            let checkpoint = compare_shard(&args, &client, shard).await?;
            write_json_immutable(&checkpoint_path, &checkpoint, "shard checkpoint")?;
            (checkpoint, false)
        };
        eprintln!(
            "ID shard {}/{} {}: parquet={} clickhouse={} parquet_only={} clickhouse_only={}",
            shard + 1,
            SHARD_COUNT,
            if resumed { "resumed" } else { "completed" },
            checkpoint.parquet_count,
            checkpoint.clickhouse_count,
            checkpoint.parquet_only_count,
            checkpoint.clickhouse_only_count
        );
        checkpoints.push(checkpoint);
        if !resumed && args.shard_delay_seconds != 0 && shard + 1 < SHARD_COUNT {
            tokio::time::sleep(Duration::from_secs(args.shard_delay_seconds)).await;
        }
    }

    let parquet_id_count = checked_sum(checkpoints.iter().map(|item| item.parquet_count))?;
    let clickhouse_id_count = checked_sum(checkpoints.iter().map(|item| item.clickhouse_count))?;
    let parquet_only_count = checked_sum(checkpoints.iter().map(|item| item.parquet_only_count))?;
    let clickhouse_only_count =
        checked_sum(checkpoints.iter().map(|item| item.clickhouse_only_count))?;
    ensure!(
        parquet_id_count == metadata.event_count,
        "sharded Parquet count does not match prepared index"
    );
    let id_keyed_equal = parquet_only_count == 0 && clickhouse_only_count == 0;
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        evidence_type: EVIDENCE_TYPE,
        runner_version: RUNNER_VERSION,
        status: if id_keyed_equal { "passed" } else { "failed" },
        snapshot_id: args.snapshot_id,
        clickhouse_database: args.clickhouse_database,
        clickhouse_table: "events_local",
        clickhouse_indexed_at_max_epoch: args.clickhouse_indexed_at_max_epoch,
        id_keyed_equal,
        parquet_id_count,
        clickhouse_id_count,
        parquet_only_count,
        clickhouse_only_count,
        shards_completed: SHARD_COUNT,
        checkpoint_directory: args.checkpoint_dir.display().to_string(),
        source_database: metadata.source_database,
        source_size_bytes: metadata.source_size_bytes,
        difference_directory: args
            .difference_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        generated_at: Utc::now(),
    };
    write_json_immutable(&args.output, &evidence, "alignment evidence")?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    ensure!(
        args.source_database.is_file(),
        "source database is not a file: {}",
        args.source_database.display()
    );
    ensure!(
        args.snapshot_id.starts_with("sha256:") && args.snapshot_id.len() == 71,
        "snapshot ID must be sha256:<64 lowercase hex characters>"
    );
    ensure!(
        args.snapshot_id[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "snapshot ID must use lowercase hexadecimal"
    );
    if let Some(parent) = args.work_database.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    Ok(())
}

fn prepare_index(args: &Args) -> Result<IndexMetadata> {
    let source_size_bytes = fs::metadata(&args.source_database)?.len();
    let connection = Connection::open(&args.work_database)
        .with_context(|| format!("open work database {}", args.work_database.display()))?;
    configure_duckdb(&connection, args)?;
    let metadata_exists: bool = connection.query_row(
        "SELECT count(*) != 0 FROM information_schema.tables WHERE table_name = 'alignment_metadata'",
        [],
        |row| row.get(0),
    )?;
    if metadata_exists {
        let encoded: String = connection.query_row(
            "SELECT metadata_json FROM alignment_metadata WHERE singleton = true",
            [],
            |row| row.get(0),
        )?;
        let metadata: IndexMetadata = serde_json::from_str(&encoded)?;
        validate_index_metadata(args, source_size_bytes, &metadata)?;
        return Ok(metadata);
    }

    connection.execute_batch("DROP TABLE IF EXISTS ids")?;
    let source = sql_string(&args.source_database.display().to_string());
    connection.execute_batch(&format!("ATTACH {source} AS source (READ_ONLY)"))?;
    let (snapshot_id, event_count): (String, u64) = connection.query_row(
        "SELECT snapshot_id, (SELECT count(*) FROM source.canonical_events) FROM source.analytics_state WHERE singleton = true",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).context("read frozen checkpoint identity")?;
    ensure!(
        snapshot_id == args.snapshot_id,
        "frozen checkpoint snapshot mismatch: expected {}, found {}",
        args.snapshot_id,
        snapshot_id
    );
    eprintln!(
        "Preparing resumable ID index for {event_count} events; this is the only full DuckDB sort"
    );
    connection.execute_batch(
        "BEGIN; CREATE TABLE ids AS SELECT id FROM source.canonical_events ORDER BY id;",
    )?;
    let indexed_count: u64 =
        connection.query_row("SELECT count(*) FROM ids", [], |row| row.get(0))?;
    ensure!(
        indexed_count == event_count,
        "prepared ID index row count mismatch"
    );
    let metadata = IndexMetadata {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        snapshot_id,
        source_database: args.source_database.display().to_string(),
        source_size_bytes,
        event_count,
    };
    connection.execute_batch("CREATE TABLE alignment_metadata (singleton BOOLEAN PRIMARY KEY, metadata_json VARCHAR NOT NULL, CHECK (singleton));")?;
    connection.execute(
        "INSERT INTO alignment_metadata VALUES (true, ?)",
        params![serde_json::to_string(&metadata)?],
    )?;
    connection.execute_batch("COMMIT; CHECKPOINT;")?;
    Ok(metadata)
}

fn configure_duckdb(connection: &Connection, args: &Args) -> Result<()> {
    connection.execute_batch(&format!(
        "SET threads = 1; SET memory_limit = {}; SET preserve_insertion_order = false;",
        sql_string(&args.duckdb_memory_limit)
    ))?;
    Ok(())
}

fn validate_index_metadata(
    args: &Args,
    source_size_bytes: u64,
    metadata: &IndexMetadata,
) -> Result<()> {
    ensure!(
        metadata.schema_version == SCHEMA_VERSION,
        "unsupported index metadata schema"
    );
    ensure!(
        metadata.runner_version == RUNNER_VERSION,
        "index runner version mismatch"
    );
    ensure!(
        metadata.snapshot_id == args.snapshot_id,
        "index snapshot mismatch"
    );
    ensure!(
        metadata.source_database == args.source_database.display().to_string(),
        "index source path mismatch"
    );
    ensure!(
        metadata.source_size_bytes == source_size_bytes,
        "index source size mismatch"
    );
    Ok(())
}

fn connect_clickhouse(args: &Args) -> clickhouse::Client {
    let mut client = clickhouse::Client::default()
        .with_url(&args.clickhouse_url)
        .with_database(&args.clickhouse_database)
        .with_option("max_threads", "1")
        .with_option(
            "max_memory_usage",
            args.clickhouse_max_memory_usage.to_string(),
        )
        .with_option(
            "max_execution_time",
            args.clickhouse_max_execution_time.to_string(),
        )
        .with_option("optimize_aggregation_in_order", "1");
    if let Some(user) = args.clickhouse_user.as_deref() {
        client = client.with_user(user);
    }
    if let Some(password) = args.clickhouse_password.as_deref() {
        client = client.with_password(password);
    }
    client
}

async fn compare_shard(
    args: &Args,
    client: &clickhouse::Client,
    shard: u16,
) -> Result<ShardCheckpoint> {
    let (lower_bytes, upper_bytes, lower_hex, upper_hex) = shard_bounds(shard);
    let parquet_path = args
        .checkpoint_dir
        .join(format!("shard-{shard:03}.parquet-ids.partial"));
    let clickhouse_path = args
        .checkpoint_dir
        .join(format!("shard-{shard:03}.clickhouse-ids.partial"));
    remove_if_exists(&parquet_path)?;
    remove_if_exists(&clickhouse_path)?;
    let parquet = export_duckdb_ids(args, &lower_bytes, upper_bytes.as_deref(), &parquet_path)
        .with_context(|| format!("export DuckDB ID shard {shard}"))?;
    let clickhouse = export_clickhouse_ids(
        args,
        client,
        &lower_hex,
        upper_hex.as_deref(),
        &clickhouse_path,
    )
    .await
    .with_context(|| format!("export ClickHouse ID shard {shard}"))?;
    let difference_paths = args.difference_dir.as_ref().map(|directory| {
        (
            directory.join(format!("shard-{shard:03}.parquet-only.ids")),
            directory.join(format!("shard-{shard:03}.clickhouse-only.ids")),
        )
    });
    let differences = compare_sorted_id_files(
        &parquet_path,
        &clickhouse_path,
        args.max_difference_examples,
        difference_paths
            .as_ref()
            .map(|(left, right)| (left.as_path(), right.as_path())),
    )?;
    let checkpoint = ShardCheckpoint {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        snapshot_id: args.snapshot_id.clone(),
        clickhouse_database: args.clickhouse_database.clone(),
        clickhouse_table: "events_local".to_owned(),
        clickhouse_indexed_at_max_epoch: args.clickhouse_indexed_at_max_epoch,
        shard,
        lower_inclusive: lower_hex,
        upper_exclusive: upper_hex,
        parquet_count: parquet.count,
        clickhouse_count: clickhouse.count,
        parquet_sha256: parquet.sha256,
        clickhouse_sha256: clickhouse.sha256,
        parquet_only_count: differences.left_only_count,
        clickhouse_only_count: differences.right_only_count,
        parquet_only_examples: differences.left_only_examples,
        clickhouse_only_examples: differences.right_only_examples,
        parquet_only_ids_file: difference_paths
            .as_ref()
            .map(|(path, _)| path.display().to_string()),
        parquet_only_ids_sha256: differences.left_only_sha256,
        clickhouse_only_ids_file: difference_paths
            .as_ref()
            .map(|(_, path)| path.display().to_string()),
        clickhouse_only_ids_sha256: differences.right_only_sha256,
        id_keyed_equal: differences.left_only_count == 0 && differences.right_only_count == 0,
        completed_at: Utc::now(),
    };
    remove_if_exists(&parquet_path)?;
    remove_if_exists(&clickhouse_path)?;
    Ok(checkpoint)
}

fn export_duckdb_ids(
    args: &Args,
    lower: &[u8],
    upper: Option<&[u8]>,
    output: &Path,
) -> Result<ExportSummary> {
    let connection = Connection::open(&args.work_database)?;
    configure_duckdb(&connection, args)?;
    let mut statement = if upper.is_some() {
        connection.prepare("SELECT id FROM ids WHERE id >= ? AND id < ? ORDER BY id")?
    } else {
        connection.prepare("SELECT id FROM ids WHERE id >= ? ORDER BY id")?
    };
    let mut rows = match upper {
        Some(upper) => statement.query(params![lower, upper])?,
        None => statement.query(params![lower])?,
    };
    let mut writer = BufWriter::new(File::create(output)?);
    let mut digest = Sha256::new();
    let mut count = 0_u64;
    let mut previous: Option<[u8; ID_BYTES]> = None;
    while let Some(row) = rows.next()? {
        let value: Vec<u8> = row.get(0)?;
        let id: [u8; ID_BYTES] = value
            .try_into()
            .map_err(|value: Vec<u8>| anyhow::anyhow!("DuckDB ID has {} bytes", value.len()))?;
        ensure_sorted_unique(previous.as_ref(), &id, "DuckDB")?;
        writer.write_all(&id)?;
        digest.update(id);
        count = count
            .checked_add(1)
            .context("DuckDB shard count overflow")?;
        previous = Some(id);
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(ExportSummary {
        count,
        sha256: hex::encode(digest.finalize()),
    })
}

async fn export_clickhouse_ids(
    args: &Args,
    client: &clickhouse::Client,
    lower: &str,
    upper: Option<&str>,
    output: &Path,
) -> Result<ExportSummary> {
    let indexed_at = u32::try_from(args.clickhouse_indexed_at_max_epoch)
        .context("ClickHouse indexed_at barrier exceeds DateTime domain")?;
    let sql = if upper.is_some() {
        "SELECT id FROM events_local WHERE indexed_at <= toDateTime({barrier:UInt32}, 'UTC') AND id >= {lower:String} AND id < {upper:String} GROUP BY id ORDER BY id"
    } else {
        "SELECT id FROM events_local WHERE indexed_at <= toDateTime({barrier:UInt32}, 'UTC') AND id >= {lower:String} GROUP BY id ORDER BY id"
    };
    let mut query = client
        .query(sql)
        .param("barrier", indexed_at)
        .param("lower", lower);
    if let Some(upper) = upper {
        query = query.param("upper", upper);
    }
    let mut cursor = query.fetch::<ClickhouseIdRow>()?;
    let mut writer = BufWriter::new(File::create(output)?);
    let mut digest = Sha256::new();
    let mut count = 0_u64;
    let mut previous: Option<[u8; ID_BYTES]> = None;
    while let Some(row) = cursor.next().await? {
        let decoded = hex::decode(&row.id).context("decode ClickHouse event ID")?;
        let id: [u8; ID_BYTES] = decoded
            .try_into()
            .map_err(|value: Vec<u8>| anyhow::anyhow!("ClickHouse ID has {} bytes", value.len()))?;
        ensure_sorted_unique(previous.as_ref(), &id, "ClickHouse")?;
        writer.write_all(&id)?;
        digest.update(id);
        count = count
            .checked_add(1)
            .context("ClickHouse shard count overflow")?;
        previous = Some(id);
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(ExportSummary {
        count,
        sha256: hex::encode(digest.finalize()),
    })
}

fn ensure_sorted_unique(
    previous: Option<&[u8; ID_BYTES]>,
    current: &[u8; ID_BYTES],
    source: &str,
) -> Result<()> {
    if let Some(previous) = previous {
        ensure!(
            previous < current,
            "{source} IDs are not strictly sorted and unique"
        );
    }
    Ok(())
}

fn compare_sorted_id_files(
    left: &Path,
    right: &Path,
    max_examples: usize,
    difference_paths: Option<(&Path, &Path)>,
) -> Result<DifferenceSummary> {
    let mut left = IdReader::open(left)?;
    let mut right = IdReader::open(right)?;
    let mut left_id = left.next_id()?;
    let mut right_id = right.next_id()?;
    let mut summary = DifferenceSummary::default();
    let mut left_sink = difference_paths
        .map(|(path, _)| DifferenceSink::new(path))
        .transpose()?;
    let mut right_sink = difference_paths
        .map(|(_, path)| DifferenceSink::new(path))
        .transpose()?;
    while left_id.is_some() || right_id.is_some() {
        match (left_id.as_ref(), right_id.as_ref()) {
            (Some(a), Some(b)) if a == b => {
                left_id = left.next_id()?;
                right_id = right.next_id()?;
            }
            (Some(a), Some(b)) if a < b => {
                record_difference(
                    &mut summary.left_only_count,
                    &mut summary.left_only_examples,
                    left_sink.as_mut(),
                    a,
                    max_examples,
                )?;
                left_id = left.next_id()?;
            }
            (Some(_), Some(b)) => {
                record_difference(
                    &mut summary.right_only_count,
                    &mut summary.right_only_examples,
                    right_sink.as_mut(),
                    b,
                    max_examples,
                )?;
                right_id = right.next_id()?;
            }
            (Some(a), None) => {
                record_difference(
                    &mut summary.left_only_count,
                    &mut summary.left_only_examples,
                    left_sink.as_mut(),
                    a,
                    max_examples,
                )?;
                left_id = left.next_id()?;
            }
            (None, Some(b)) => {
                record_difference(
                    &mut summary.right_only_count,
                    &mut summary.right_only_examples,
                    right_sink.as_mut(),
                    b,
                    max_examples,
                )?;
                right_id = right.next_id()?;
            }
            (None, None) => break,
        }
    }
    summary.left_only_sha256 = left_sink.map(DifferenceSink::finish).transpose()?;
    summary.right_only_sha256 = right_sink.map(DifferenceSink::finish).transpose()?;
    Ok(summary)
}

fn record_difference(
    count: &mut u64,
    examples: &mut Vec<String>,
    sink: Option<&mut DifferenceSink>,
    id: &[u8; ID_BYTES],
    max_examples: usize,
) -> Result<()> {
    *count = count.checked_add(1).context("difference count overflow")?;
    if examples.len() < max_examples {
        examples.push(hex::encode(id));
    }
    if let Some(sink) = sink {
        sink.write(id)?;
    }
    Ok(())
}

struct DifferenceSink {
    final_path: PathBuf,
    partial_path: PathBuf,
    writer: BufWriter<File>,
    digest: Sha256,
}

impl DifferenceSink {
    fn new(final_path: &Path) -> Result<Self> {
        let partial_path = final_path.with_extension("ids.partial");
        remove_if_exists(&partial_path)?;
        let writer = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial_path)?,
        );
        Ok(Self {
            final_path: final_path.to_owned(),
            partial_path,
            writer,
            digest: Sha256::new(),
        })
    }

    fn write(&mut self, id: &[u8; ID_BYTES]) -> Result<()> {
        self.writer.write_all(id)?;
        self.digest.update(id);
        Ok(())
    }

    fn finish(mut self) -> Result<String> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        let sha256 = hex::encode(self.digest.finalize());
        let expected_size = self.partial_path.metadata()?.len();
        if self.final_path.exists() {
            ensure!(
                self.final_path.metadata()?.len() == expected_size
                    && sha256_file(&self.final_path)? == sha256,
                "existing directional ID stream differs from recomputed output {}",
                self.final_path.display()
            );
            fs::remove_file(&self.partial_path)?;
        } else {
            fs::rename(&self.partial_path, &self.final_path)?;
        }
        Ok(sha256)
    }
}

struct IdReader(BufReader<File>);

impl IdReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self(BufReader::new(File::open(path)?)))
    }

    fn next_id(&mut self) -> Result<Option<[u8; ID_BYTES]>> {
        let mut id = [0_u8; ID_BYTES];
        let mut read = 0;
        while read < ID_BYTES {
            let amount = self.0.read(&mut id[read..])?;
            if amount == 0 {
                if read == 0 {
                    return Ok(None);
                }
                bail!("truncated fixed-width ID file");
            }
            read += amount;
        }
        Ok(Some(id))
    }
}

fn shard_bounds(shard: u16) -> (Vec<u8>, Option<Vec<u8>>, String, Option<String>) {
    let mut lower = vec![0_u8; ID_BYTES];
    lower[0] = shard as u8;
    let upper = if shard + 1 < SHARD_COUNT {
        let mut value = vec![0_u8; ID_BYTES];
        value[0] = (shard + 1) as u8;
        Some(value)
    } else {
        None
    };
    let lower_hex = hex::encode(&lower);
    let upper_hex = upper.as_ref().map(hex::encode);
    (lower, upper, lower_hex, upper_hex)
}

fn validate_checkpoint(args: &Args, shard: u16, checkpoint: &ShardCheckpoint) -> Result<()> {
    let (_, _, lower, upper) = shard_bounds(shard);
    ensure!(
        checkpoint.schema_version == SCHEMA_VERSION,
        "shard {shard} schema mismatch"
    );
    ensure!(
        checkpoint.runner_version == RUNNER_VERSION,
        "shard {shard} runner mismatch"
    );
    ensure!(
        checkpoint.snapshot_id == args.snapshot_id,
        "shard {shard} snapshot mismatch"
    );
    ensure!(
        checkpoint.clickhouse_database == args.clickhouse_database,
        "shard {shard} database mismatch"
    );
    ensure!(
        checkpoint.clickhouse_table == "events_local",
        "shard {shard} table mismatch"
    );
    ensure!(
        checkpoint.clickhouse_indexed_at_max_epoch == args.clickhouse_indexed_at_max_epoch,
        "shard {shard} barrier mismatch"
    );
    ensure!(
        checkpoint.shard == shard
            && checkpoint.lower_inclusive == lower
            && checkpoint.upper_exclusive == upper,
        "shard {shard} bounds mismatch"
    );
    ensure!(
        checkpoint.id_keyed_equal
            == (checkpoint.parquet_only_count == 0 && checkpoint.clickhouse_only_count == 0),
        "shard {shard} equality result is inconsistent with its directional counts"
    );
    ensure!(
        is_sha256_hex(&checkpoint.parquet_sha256) && is_sha256_hex(&checkpoint.clickhouse_sha256),
        "shard {shard} has an invalid stream digest"
    );
    match &args.difference_dir {
        Some(directory) => {
            let parquet_path = directory.join(format!("shard-{shard:03}.parquet-only.ids"));
            let clickhouse_path = directory.join(format!("shard-{shard:03}.clickhouse-only.ids"));
            validate_difference_file(
                shard,
                "Parquet-only",
                &parquet_path,
                checkpoint.parquet_only_count,
                checkpoint.parquet_only_ids_file.as_deref(),
                checkpoint.parquet_only_ids_sha256.as_deref(),
            )?;
            validate_difference_file(
                shard,
                "ClickHouse-only",
                &clickhouse_path,
                checkpoint.clickhouse_only_count,
                checkpoint.clickhouse_only_ids_file.as_deref(),
                checkpoint.clickhouse_only_ids_sha256.as_deref(),
            )?;
        }
        None => ensure!(
            checkpoint.parquet_only_ids_file.is_none()
                && checkpoint.parquet_only_ids_sha256.is_none()
                && checkpoint.clickhouse_only_ids_file.is_none()
                && checkpoint.clickhouse_only_ids_sha256.is_none(),
            "shard {shard} unexpectedly contains directional ID stream metadata"
        ),
    }
    Ok(())
}

fn validate_difference_file(
    shard: u16,
    label: &str,
    expected_path: &Path,
    expected_count: u64,
    recorded_path: Option<&str>,
    recorded_sha256: Option<&str>,
) -> Result<()> {
    ensure!(
        recorded_path == Some(expected_path.display().to_string().as_str()),
        "shard {shard} {label} ID stream path mismatch"
    );
    let recorded_sha256 = recorded_sha256
        .with_context(|| format!("shard {shard} {label} ID stream digest is missing"))?;
    ensure!(
        is_sha256_hex(recorded_sha256),
        "shard {shard} {label} ID stream digest is invalid"
    );
    ensure!(
        expected_path.is_file(),
        "shard {shard} {label} ID stream is missing"
    );
    ensure!(
        expected_path.metadata()?.len() == expected_count * ID_BYTES as u64,
        "shard {shard} {label} ID stream size mismatch"
    );
    ensure!(
        sha256_file(expected_path)? == recorded_sha256,
        "shard {shard} {label} ID stream SHA-256 mismatch"
    );
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn checked_sum(mut values: impl Iterator<Item = u64>) -> Result<u64> {
    values.try_fold(0_u64, |total, value| {
        total.checked_add(value).context("count overflow")
    })
}

fn write_json_immutable(path: &Path, value: &impl Serialize, label: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create immutable {label} {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn shard_bounds_cover_the_id_domain() {
        let (first, first_upper, first_hex, _) = shard_bounds(0);
        assert_eq!(first, vec![0; ID_BYTES]);
        assert_eq!(first_hex, "0".repeat(ID_BYTES * 2));
        assert_eq!(first_upper.unwrap()[0], 1);
        let (last, last_upper, _, last_upper_hex) = shard_bounds(255);
        assert_eq!(last[0], 255);
        assert!(last_upper.is_none());
        assert!(last_upper_hex.is_none());
    }

    #[test]
    fn sorted_merge_counts_directional_differences() {
        let directory = tempdir().unwrap();
        let left = directory.path().join("left");
        let right = directory.path().join("right");
        let id = |byte| [byte; ID_BYTES];
        fs::write(&left, [id(1), id(2), id(4)].concat()).unwrap();
        fs::write(&right, [id(1), id(3), id(4), id(5)].concat()).unwrap();
        let left_only = directory.path().join("left-only.ids");
        let right_only = directory.path().join("right-only.ids");
        let result =
            compare_sorted_id_files(&left, &right, 1, Some((&left_only, &right_only))).unwrap();
        assert_eq!(result.left_only_count, 1);
        assert_eq!(result.right_only_count, 2);
        assert_eq!(result.left_only_examples, vec![hex::encode(id(2))]);
        assert_eq!(result.right_only_examples, vec![hex::encode(id(3))]);
        assert_eq!(fs::read(left_only).unwrap(), id(2));
        assert_eq!(fs::read(right_only).unwrap(), [id(3), id(5)].concat());
        assert!(result.left_only_sha256.is_some());
        assert!(result.right_only_sha256.is_some());
    }

    #[test]
    fn fixed_width_reader_rejects_truncation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("truncated");
        fs::write(&path, [7_u8; ID_BYTES - 1]).unwrap();
        let mut reader = IdReader::open(&path).unwrap();
        assert!(reader.next_id().is_err());
    }

    #[test]
    fn preparation_validates_snapshot_and_sorts_ids() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source.duckdb");
        let source_connection = Connection::open(&source).unwrap();
        source_connection
            .execute_batch(
                "
                CREATE TABLE canonical_events (id BLOB NOT NULL);
                INSERT INTO canonical_events VALUES (from_hex('02')), (from_hex('01'));
                CREATE TABLE analytics_state (
                    singleton BOOLEAN PRIMARY KEY,
                    snapshot_id VARCHAR NOT NULL,
                    as_of_epoch UBIGINT NOT NULL,
                    query_version VARCHAR NOT NULL,
                    CHECK (singleton)
                );
                INSERT INTO analytics_state VALUES (
                    true,
                    'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                    1,
                    'test'
                );
                CHECKPOINT;
                ",
            )
            .unwrap();
        drop(source_connection);
        let args = Args {
            source_database: source,
            work_database: directory.path().join("work.duckdb"),
            checkpoint_dir: directory.path().join("checkpoints"),
            output: directory.path().join("evidence.json"),
            difference_dir: None,
            snapshot_id: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
            clickhouse_indexed_at_max_epoch: 1,
            clickhouse_url: "http://localhost:8123".to_owned(),
            clickhouse_database: "nostr".to_owned(),
            clickhouse_user: None,
            clickhouse_password: None,
            duckdb_memory_limit: "32MB".to_owned(),
            clickhouse_max_memory_usage: 1,
            clickhouse_max_execution_time: 1,
            shard_delay_seconds: 0,
            max_difference_examples: 1,
        };
        let metadata = prepare_index(&args).unwrap();
        assert_eq!(metadata.event_count, 2);
        let work = Connection::open(&args.work_database).unwrap();
        let ids = work
            .prepare("SELECT hex(id) FROM ids ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<duckdb::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, vec!["01", "02"]);
    }
}
