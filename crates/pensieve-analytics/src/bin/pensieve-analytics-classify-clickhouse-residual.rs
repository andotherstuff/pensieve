//! Classify exact ClickHouse-only residual IDs using bounded ClickHouse metadata lookups.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use duckdb::{AccessMode, Config, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-classify-clickhouse-residual-v1";
const PARENT_RUNNER_VERSION: &str = "pensieve-analytics-classify-parquet-gap-v1";

#[derive(Debug, Parser)]
#[command(about = "Classify exact ClickHouse-only residual IDs by ClickHouse metadata")]
struct Args {
    /// Completed ClickHouse-only Parquet comparison evidence.
    #[arg(long)]
    classification_evidence: PathBuf,
    /// Required SHA-256 of the comparison evidence.
    #[arg(long)]
    classification_evidence_sha256: String,
    /// Preserved comparison DuckDB containing candidates and Parquet matches.
    #[arg(long)]
    attribution_database: PathBuf,
    /// Start of production live Parquet shadow publication, as a Unix epoch.
    #[arg(long)]
    shadow_started_at_epoch: u64,
    /// Directory for immutable, resumable ClickHouse batch checkpoints.
    #[arg(long)]
    checkpoint_dir: PathBuf,
    /// Immutable final JSON evidence path.
    #[arg(long)]
    output: PathBuf,
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
    /// Maximum candidate IDs per primary-key ClickHouse lookup.
    #[arg(long, default_value_t = 10_000)]
    clickhouse_batch_size: usize,
    /// Maximum ClickHouse memory per lookup in bytes.
    #[arg(long, default_value_t = 1024 * 1024 * 1024)]
    clickhouse_max_memory_usage: u64,
    /// Maximum ClickHouse execution time per lookup in seconds.
    #[arg(long, default_value_t = 900)]
    clickhouse_max_execution_time: u64,
    /// Pause after each newly completed ClickHouse batch.
    #[arg(long, default_value_t = 100)]
    batch_delay_millis: u64,
    /// Maximum example IDs retained for missing ClickHouse rows.
    #[arg(long, default_value_t = 20)]
    max_examples: usize,
}

#[derive(Debug, Deserialize)]
struct ParentEvidence {
    schema_version: u32,
    runner_version: String,
    status: String,
    candidate_direction: String,
    alignment_snapshot_id: String,
    catalog_snapshot_id: String,
    baseline_catalog_sha256: Option<String>,
    catalog_sha256: String,
    alignment_evidence_sha256: String,
    candidate_ids: u64,
    matched_unique_ids: u64,
    residual_unattributed_ids: u64,
    work_database: String,
}

#[derive(Clone, Debug, Deserialize, Row, Serialize)]
struct MetadataRow {
    id: String,
    created_at: u32,
    kind: u16,
    first_indexed_at: u32,
    last_indexed_at: u32,
    versions: u64,
    relay_source: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct BatchCheckpoint {
    schema_version: u32,
    runner_version: String,
    classification_evidence_sha256: String,
    residual_ids_sha256: String,
    clickhouse_database: String,
    shadow_started_at_epoch: u64,
    batch_index: usize,
    input_count: usize,
    input_sha256: String,
    rows: Vec<MetadataRow>,
    completed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct CountAttribution<T> {
    value: T,
    count: u64,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    classification_evidence: String,
    classification_evidence_sha256: String,
    alignment_snapshot_id: String,
    catalog_snapshot_id: String,
    attribution_database: String,
    attribution_database_size_bytes: u64,
    residual_ids: u64,
    residual_ids_sha256: String,
    clickhouse_database: String,
    clickhouse_table: &'static str,
    clickhouse_rows_found: u64,
    clickhouse_rows_missing: u64,
    missing_examples: Vec<String>,
    shadow_started_at_epoch: u64,
    shadow_started_at: DateTime<Utc>,
    first_indexed_before_shadow: u64,
    first_indexed_at_or_after_shadow: u64,
    before_shadow_percent: f64,
    duplicate_versions: u64,
    created_at_min: Option<DateTime<Utc>>,
    created_at_median: Option<DateTime<Utc>>,
    created_at_max: Option<DateTime<Utc>>,
    first_indexed_at_attribution: Vec<CountAttribution<DateTime<Utc>>>,
    first_indexed_day_attribution: Vec<CountAttribution<String>>,
    kind_attribution: Vec<CountAttribution<u16>>,
    relay_source_attribution: Vec<CountAttribution<String>>,
    completed_batches: usize,
    checkpoint_directory: String,
    note: &'static str,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("ClickHouse residual classification failed: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    fs::create_dir_all(&args.checkpoint_dir)?;
    ensure!(
        !args.output.exists(),
        "refusing to replace immutable evidence {}",
        args.output.display()
    );

    let parent_bytes = fs::read(&args.classification_evidence)?;
    let parent_sha256 = sha256_bytes(&parent_bytes);
    ensure!(
        parent_sha256 == args.classification_evidence_sha256,
        "classification evidence SHA-256 mismatch"
    );
    let parent: ParentEvidence = serde_json::from_slice(&parent_bytes)?;
    validate_parent(&args, &parent)?;

    let config = Config::default().access_mode(AccessMode::ReadOnly)?;
    let connection = Connection::open_with_flags(&args.attribution_database, config)?;
    validate_database(&connection, &parent)?;
    let residual_ids = load_residual_ids(&connection)?;
    ensure!(
        u64::try_from(residual_ids.len())? == parent.residual_unattributed_ids,
        "residual ID count does not reproduce parent evidence"
    );
    let residual_ids_sha256 = sha256_ids(&residual_ids);
    eprintln!("loaded {} exact residual IDs", residual_ids.len());

    let rows = query_clickhouse(&args, &parent_sha256, &residual_ids_sha256, &residual_ids).await?;
    let evidence = build_evidence(
        &args,
        &parent,
        parent_sha256,
        residual_ids_sha256,
        &residual_ids,
        rows,
    )?;
    write_json_immutable(&args.output, &evidence)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    ensure!(
        args.classification_evidence.is_file(),
        "classification evidence is missing"
    );
    ensure!(
        args.attribution_database.is_file(),
        "attribution database is missing"
    );
    ensure!(
        args.clickhouse_batch_size > 0,
        "ClickHouse batch size must be positive"
    );
    ensure!(
        args.shadow_started_at_epoch > 0,
        "shadow start epoch must be positive"
    );
    ensure!(
        args.output.parent()
            == Some(
                args.checkpoint_dir
                    .parent()
                    .context("checkpoint directory needs a parent")?
            ),
        "output and checkpoint directory must share an evidence root"
    );
    Ok(())
}

fn validate_parent(args: &Args, evidence: &ParentEvidence) -> Result<()> {
    ensure!(
        evidence.schema_version == 1
            && evidence.runner_version == PARENT_RUNNER_VERSION
            && evidence.status == "completed"
            && evidence.candidate_direction == "clickhouse_only",
        "unsupported parent evidence"
    );
    ensure!(
        evidence.work_database == args.attribution_database.display().to_string(),
        "parent evidence names a different attribution database"
    );
    ensure!(
        evidence.matched_unique_ids + evidence.residual_unattributed_ids == evidence.candidate_ids,
        "parent candidate accounting is inconsistent"
    );
    Ok(())
}

fn validate_database(connection: &Connection, parent: &ParentEvidence) -> Result<()> {
    for (key, expected) in [
        ("runner_version", PARENT_RUNNER_VERSION.to_owned()),
        ("candidate_direction", "clickhouse_only".to_owned()),
        (
            "alignment_snapshot_id",
            parent.alignment_snapshot_id.clone(),
        ),
        (
            "alignment_evidence_sha256",
            parent.alignment_evidence_sha256.clone(),
        ),
        ("catalog_sha256", parent.catalog_sha256.clone()),
        (
            "baseline_catalog_sha256",
            parent
                .baseline_catalog_sha256
                .clone()
                .context("parent baseline catalog SHA is missing")?,
        ),
        ("candidate_count", parent.candidate_ids.to_string()),
    ] {
        let actual: String = connection.query_row(
            "SELECT value FROM run_metadata WHERE key = ?",
            [key],
            |row| row.get(0),
        )?;
        ensure!(actual == expected, "database metadata mismatch for {key}");
    }
    let matched: u64 =
        connection.query_row("SELECT count(DISTINCT id) FROM matches", [], |row| {
            row.get(0)
        })?;
    ensure!(
        matched == parent.matched_unique_ids,
        "database match count mismatch"
    );
    Ok(())
}

fn load_residual_ids(connection: &Connection) -> Result<Vec<[u8; 32]>> {
    let mut statement = connection.prepare(
        "SELECT c.id FROM candidates c LEFT JOIN matches m USING (id)
         WHERE m.id IS NULL ORDER BY c.id",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    rows.map(|row| {
        let bytes = row?;
        bytes
            .try_into()
            .map_err(|value: Vec<u8>| anyhow::anyhow!("candidate ID has {} bytes", value.len()))
    })
    .collect()
}

async fn query_clickhouse(
    args: &Args,
    parent_sha256: &str,
    residual_sha256: &str,
    ids: &[[u8; 32]],
) -> Result<BTreeMap<[u8; 32], MetadataRow>> {
    let client = connect_clickhouse(args);
    let total_batches = ids.len().div_ceil(args.clickhouse_batch_size);
    let mut output = BTreeMap::new();
    for (batch_index, batch) in ids.chunks(args.clickhouse_batch_size).enumerate() {
        let input_sha256 = sha256_ids(batch);
        let path = args
            .checkpoint_dir
            .join(format!("batch-{batch_index:05}.json"));
        let (checkpoint, resumed) = if path.exists() {
            let checkpoint: BatchCheckpoint = serde_json::from_slice(&fs::read(&path)?)?;
            validate_batch(
                args,
                parent_sha256,
                residual_sha256,
                batch_index,
                batch,
                &input_sha256,
                &checkpoint,
            )?;
            (checkpoint, true)
        } else {
            let query = clickhouse_metadata_query(
                batch,
                args.clickhouse_max_memory_usage,
                args.clickhouse_max_execution_time,
            );
            let rows = client.query(&query).fetch_all::<MetadataRow>().await?;
            let checkpoint = BatchCheckpoint {
                schema_version: SCHEMA_VERSION,
                runner_version: RUNNER_VERSION.to_owned(),
                classification_evidence_sha256: parent_sha256.to_owned(),
                residual_ids_sha256: residual_sha256.to_owned(),
                clickhouse_database: args.clickhouse_database.clone(),
                shadow_started_at_epoch: args.shadow_started_at_epoch,
                batch_index,
                input_count: batch.len(),
                input_sha256: input_sha256.clone(),
                rows,
                completed_at: Utc::now(),
            };
            validate_batch(
                args,
                parent_sha256,
                residual_sha256,
                batch_index,
                batch,
                &input_sha256,
                &checkpoint,
            )?;
            write_json_immutable(&path, &checkpoint)?;
            (checkpoint, false)
        };
        for row in checkpoint.rows {
            let decoded = hex::decode(&row.id)?;
            let id: [u8; 32] = decoded.try_into().map_err(|value: Vec<u8>| {
                anyhow::anyhow!("ClickHouse ID has {} bytes", value.len())
            })?;
            ensure!(
                output.insert(id, row).is_none(),
                "duplicate ClickHouse result ID"
            );
        }
        eprintln!(
            "ClickHouse metadata batch {} of {} {}",
            batch_index + 1,
            total_batches,
            if resumed { "resumed" } else { "completed" }
        );
        if !resumed && args.batch_delay_millis > 0 && batch_index + 1 < total_batches {
            tokio::time::sleep(Duration::from_millis(args.batch_delay_millis)).await;
        }
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn validate_batch(
    args: &Args,
    parent_sha256: &str,
    residual_sha256: &str,
    batch_index: usize,
    batch: &[[u8; 32]],
    input_sha256: &str,
    checkpoint: &BatchCheckpoint,
) -> Result<()> {
    ensure!(
        checkpoint.schema_version == SCHEMA_VERSION
            && checkpoint.runner_version == RUNNER_VERSION
            && checkpoint.classification_evidence_sha256 == parent_sha256
            && checkpoint.residual_ids_sha256 == residual_sha256
            && checkpoint.clickhouse_database == args.clickhouse_database
            && checkpoint.shadow_started_at_epoch == args.shadow_started_at_epoch
            && checkpoint.batch_index == batch_index
            && checkpoint.input_count == batch.len()
            && checkpoint.input_sha256 == input_sha256,
        "batch checkpoint identity mismatch"
    );
    let input: HashSet<_> = batch.iter().copied().collect();
    let mut previous: Option<&str> = None;
    for row in &checkpoint.rows {
        ensure!(
            row.id.len() == 64
                && row
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid ClickHouse ID"
        );
        if let Some(previous) = previous {
            ensure!(
                previous < row.id.as_str(),
                "ClickHouse batch IDs are not sorted and unique"
            );
        }
        let decoded = hex::decode(&row.id)?;
        let id: [u8; 32] = decoded
            .try_into()
            .map_err(|value: Vec<u8>| anyhow::anyhow!("ClickHouse ID has {} bytes", value.len()))?;
        ensure!(
            input.contains(&id),
            "ClickHouse returned an ID outside the input batch"
        );
        previous = Some(&row.id);
    }
    Ok(())
}

fn build_evidence(
    args: &Args,
    parent: &ParentEvidence,
    parent_sha256: String,
    residual_sha256: String,
    ids: &[[u8; 32]],
    rows: BTreeMap<[u8; 32], MetadataRow>,
) -> Result<Evidence> {
    let missing: Vec<_> = ids
        .iter()
        .filter(|id| !rows.contains_key(*id))
        .copied()
        .collect();
    let shadow_epoch = u32::try_from(args.shadow_started_at_epoch)
        .context("shadow epoch exceeds DateTime domain")?;
    let before = rows
        .values()
        .filter(|row| row.first_indexed_at < shadow_epoch)
        .count() as u64;
    let after = rows.len() as u64 - before;
    let duplicate_versions = rows.values().filter(|row| row.versions > 1).count() as u64;
    let created: Vec<_> = rows.values().map(|row| row.created_at).collect();
    let first_indexed_at = attribution(rows.values().map(|row| row.first_indexed_at), |epoch| {
        epoch_datetime(*epoch)
    });
    let first_indexed_day = attribution(rows.values().map(|row| row.first_indexed_at), |epoch| {
        epoch_datetime(*epoch).format("%Y-%m-%d").to_string()
    });
    let kinds = attribution(rows.values().map(|row| row.kind), |kind| *kind);
    let relays = attribution(
        rows.values().map(|row| row.relay_source.as_str()),
        |relay| relay.to_string(),
    );
    let completed_batches = ids.len().div_ceil(args.clickhouse_batch_size);
    Ok(Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "completed",
        generated_at: Utc::now(),
        classification_evidence: args.classification_evidence.display().to_string(),
        classification_evidence_sha256: parent_sha256,
        alignment_snapshot_id: parent.alignment_snapshot_id.clone(),
        catalog_snapshot_id: parent.catalog_snapshot_id.clone(),
        attribution_database: args.attribution_database.display().to_string(),
        attribution_database_size_bytes: fs::metadata(&args.attribution_database)?.len(),
        residual_ids: ids.len() as u64,
        residual_ids_sha256: residual_sha256,
        clickhouse_database: args.clickhouse_database.clone(),
        clickhouse_table: "events_local",
        clickhouse_rows_found: rows.len() as u64,
        clickhouse_rows_missing: missing.len() as u64,
        missing_examples: missing
            .iter()
            .take(args.max_examples)
            .map(hex::encode)
            .collect(),
        shadow_started_at_epoch: args.shadow_started_at_epoch,
        shadow_started_at: epoch_datetime(shadow_epoch),
        first_indexed_before_shadow: before,
        first_indexed_at_or_after_shadow: after,
        before_shadow_percent: percent(before, rows.len() as u64),
        duplicate_versions,
        created_at_min: created.iter().min().copied().map(epoch_datetime),
        created_at_median: percentile(created.clone(), 50).map(epoch_datetime),
        created_at_max: created.iter().max().copied().map(epoch_datetime),
        first_indexed_at_attribution: first_indexed_at,
        first_indexed_day_attribution: first_indexed_day,
        kind_attribution: kinds,
        relay_source_attribution: relays,
        completed_batches,
        checkpoint_directory: args.checkpoint_dir.display().to_string(),
        note: "Insertion time classifies whether residual rows predate the live Parquet shadow. Pre-shadow rows demonstrate legacy cross-store divergence, not a live-shadow publication omission; source provenance remains limited by ClickHouse metadata.",
    })
}

fn attribution<I, K, F>(values: I, map: F) -> Vec<CountAttribution<K>>
where
    I: IntoIterator,
    F: Fn(&I::Item) -> K,
    K: Ord,
{
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(map(&value)).or_insert(0_u64) += 1;
    }
    counts
        .into_iter()
        .map(|(value, count)| CountAttribution { value, count })
        .collect()
}

fn connect_clickhouse(args: &Args) -> clickhouse::Client {
    let mut client = clickhouse::Client::default()
        .with_url(&args.clickhouse_url)
        .with_database(&args.clickhouse_database);
    if let Some(user) = args.clickhouse_user.as_deref() {
        client = client.with_user(user);
    }
    if let Some(password) = args.clickhouse_password.as_deref() {
        client = client.with_password(password);
    }
    client
}

fn clickhouse_metadata_query(ids: &[[u8; 32]], max_memory: u64, max_time: u64) -> String {
    let ids = ids
        .iter()
        .map(|id| format!("'{}'", hex::encode(id)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "SELECT id, toUInt32(argMax(created_at, indexed_at)) AS created_at,
                argMax(kind, indexed_at) AS kind,
                toUInt32(min(indexed_at)) AS first_indexed_at,
                toUInt32(max(indexed_at)) AS last_indexed_at,
                count() AS versions,
                argMax(relay_source, indexed_at) AS relay_source
         FROM events_local WHERE id IN ({ids}) GROUP BY id ORDER BY id
         SETTINGS max_memory_usage={max_memory}, max_execution_time={max_time}"
    )
}

fn epoch_datetime(epoch: u32) -> DateTime<Utc> {
    DateTime::from_timestamp(i64::from(epoch), 0).expect("UInt32 epoch is a valid DateTime")
}

fn percentile(mut values: Vec<u32>, percentile: usize) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.get((values.len() - 1) * percentile / 100).copied()
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_ids(ids: &[[u8; 32]]) -> String {
    let mut digest = Sha256::new();
    for id in ids {
        digest.update(id);
    }
    hex::encode(digest.finalize())
}

fn write_json_immutable(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{attribution, clickhouse_metadata_query, percent, percentile};

    #[test]
    fn attribution_is_sorted_and_exact() {
        let result = attribution([3_u16, 1, 3], |value| *value);
        assert_eq!(result.len(), 2);
        assert_eq!((result[0].value, result[0].count), (1, 1));
        assert_eq!((result[1].value, result[1].count), (3, 2));
    }

    #[test]
    fn percent_and_percentile_handle_edges() {
        assert_eq!(percent(1, 4), 25.0);
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(percentile(vec![5, 1, 3, 2, 4], 50), Some(3));
        assert_eq!(percentile(Vec::new(), 50), None);
    }

    #[test]
    fn clickhouse_query_embeds_only_fixed_width_hex_ids() {
        let query = clickhouse_metadata_query(&[[0xab; 32], [0x01; 32]], 123, 45);
        assert!(query.contains(&format!("'{}'", "ab".repeat(32))));
        assert!(query.contains(&format!("'{}'", "01".repeat(32))));
        assert!(query.contains("max_memory_usage=123"));
        assert!(query.contains("max_execution_time=45"));
    }
}
