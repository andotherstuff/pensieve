//! Attribute an exact ID-alignment gap to selected canonical notepack segments.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use duckdb::{Connection, params};
use pensieve_parquet::{DEFAULT_MAX_EVENT_BYTES, scan_segment};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-classify-segment-gap-v1";

#[derive(Debug, Parser)]
#[command(about = "Attribute a frozen Parquet/ClickHouse ID gap to source segments")]
struct Args {
    /// Validated exact-alignment evidence being classified.
    #[arg(long)]
    alignment_evidence: PathBuf,
    /// Required SHA-256 of the exact alignment evidence file.
    #[arg(long)]
    alignment_evidence_sha256: String,
    /// Preserved DuckDB ID index created by pensieve-analytics-align-ids.
    #[arg(long)]
    parquet_id_index: PathBuf,
    /// Canonical notepack segment to classify; repeat for every source segment.
    #[arg(long, required = true)]
    source_segment: Vec<PathBuf>,
    /// Directory for immutable, resumable ClickHouse batch checkpoints.
    #[arg(long)]
    checkpoint_dir: PathBuf,
    /// Immutable final JSON evidence path.
    #[arg(long)]
    output: PathBuf,
    /// Snapshot ID required in both the alignment evidence and ID index.
    #[arg(long)]
    snapshot_id: String,
    /// ClickHouse indexed_at barrier required in the alignment evidence.
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
    /// DuckDB memory limit for the candidate-to-frozen-index join.
    #[arg(long, default_value = "4GB")]
    duckdb_memory_limit: String,
    /// Maximum accepted notepack frame size.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_event_bytes: usize,
    /// Maximum example IDs retained in final evidence.
    #[arg(long, default_value_t = 20)]
    max_examples: usize,
}

#[derive(Debug, Deserialize)]
struct AlignmentEvidence {
    schema_version: u32,
    evidence_type: String,
    status: String,
    snapshot_id: String,
    clickhouse_database: String,
    clickhouse_table: String,
    clickhouse_indexed_at_max_epoch: u64,
    parquet_only_count: u64,
    clickhouse_only_count: u64,
    shards_completed: u16,
}

#[derive(Debug, Deserialize)]
struct IndexMetadata {
    schema_version: u32,
    runner_version: String,
    snapshot_id: String,
    event_count: u64,
}

#[derive(Clone, Debug)]
struct Candidate {
    created_at: u64,
    kind: u16,
    segments: BTreeSet<usize>,
}

#[derive(Debug)]
struct ScannedSources {
    candidates: BTreeMap<[u8; 32], Candidate>,
    segments: Vec<SegmentEvidence>,
    occurrences: usize,
}

#[derive(Debug, Serialize)]
struct SegmentEvidence {
    path: String,
    size_bytes: u64,
    sha256: String,
    valid_events: usize,
    rejected_events: usize,
    unique_ids: usize,
    parquet_only_at_barrier: usize,
    parquet_only_still_absent_current: usize,
}

#[derive(Clone, Debug, Deserialize, Row, Serialize)]
struct ClickhousePresenceRow {
    id: String,
    first_indexed_at: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct BatchCheckpoint {
    schema_version: u32,
    runner_version: String,
    snapshot_id: String,
    clickhouse_database: String,
    clickhouse_indexed_at_max_epoch: u64,
    batch_index: usize,
    input_count: usize,
    input_sha256: String,
    rows: Vec<ClickhousePresenceRow>,
    completed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct KindCount {
    kind: u16,
    count: usize,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    alignment_evidence: String,
    alignment_evidence_sha256: String,
    snapshot_id: String,
    clickhouse_database: String,
    clickhouse_table: &'static str,
    clickhouse_indexed_at_max_epoch: u64,
    parquet_id_index: String,
    parquet_id_index_size_bytes: u64,
    source_segments: Vec<SegmentEvidence>,
    source_event_occurrences: usize,
    unique_source_ids: usize,
    duplicate_source_occurrences: usize,
    source_ids_sha256: String,
    source_ids_in_frozen_parquet: usize,
    source_ids_absent_from_frozen_parquet: usize,
    source_ids_in_clickhouse_at_barrier: usize,
    source_ids_in_clickhouse_current: usize,
    parquet_only_at_barrier: usize,
    parquet_only_now_in_clickhouse: usize,
    parquet_only_still_absent_current: usize,
    alignment_parquet_only_population: u64,
    alignment_clickhouse_only_population: u64,
    residual_parquet_only_after_selected_segments: u64,
    selected_segments_share_percent: f64,
    parquet_only_created_at_min: Option<u64>,
    parquet_only_created_at_median: Option<u64>,
    parquet_only_created_at_max: Option<u64>,
    parquet_only_top_kinds: Vec<KindCount>,
    parquet_only_examples: Vec<String>,
    checkpoint_directory: String,
    completed_batches: usize,
    note: &'static str,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("segment-gap classification failed: {error:#}");
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

    let alignment_bytes = fs::read(&args.alignment_evidence)?;
    let alignment_sha256 = sha256_bytes(&alignment_bytes);
    ensure!(
        alignment_sha256 == args.alignment_evidence_sha256,
        "alignment evidence SHA-256 mismatch"
    );
    let alignment: AlignmentEvidence = serde_json::from_slice(&alignment_bytes)?;
    validate_alignment(&args, &alignment)?;

    let ScannedSources {
        mut candidates,
        mut segments,
        occurrences,
    } = scan_sources(&args)?;
    let candidate_ids: Vec<[u8; 32]> = candidates.keys().copied().collect();
    let source_ids_sha256 = sha256_ids(&candidate_ids);
    let parquet_present = parquet_membership(
        &args.parquet_id_index,
        &args.snapshot_id,
        &args.duckdb_memory_limit,
        &candidate_ids,
    )?;
    let clickhouse_presence = clickhouse_membership(&args, &candidate_ids).await?;

    let mut parquet_only = Vec::new();
    let mut now_in_clickhouse = 0usize;
    let mut in_clickhouse_at_barrier = 0usize;
    for id in &candidate_ids {
        let indexed_at = clickhouse_presence.get(id).copied();
        if indexed_at.is_some_and(|value| u64::from(value) <= args.clickhouse_indexed_at_max_epoch)
        {
            in_clickhouse_at_barrier += 1;
        }
        if parquet_present.contains(id)
            && indexed_at
                .is_none_or(|value| u64::from(value) > args.clickhouse_indexed_at_max_epoch)
        {
            parquet_only.push(*id);
            if indexed_at.is_some() {
                now_in_clickhouse += 1;
            }
        }
    }
    let still_absent: HashSet<[u8; 32]> = parquet_only
        .iter()
        .copied()
        .filter(|id| !clickhouse_presence.contains_key(id))
        .collect();
    let parquet_only_set: HashSet<[u8; 32]> = parquet_only.iter().copied().collect();
    for (segment_index, segment) in segments.iter_mut().enumerate() {
        segment.parquet_only_at_barrier = candidates
            .iter()
            .filter(|(id, candidate)| {
                candidate.segments.contains(&segment_index) && parquet_only_set.contains(*id)
            })
            .count();
        segment.parquet_only_still_absent_current = candidates
            .iter()
            .filter(|(id, candidate)| {
                candidate.segments.contains(&segment_index) && still_absent.contains(*id)
            })
            .count();
    }

    let explained = u64::try_from(parquet_only.len())?;
    ensure!(
        explained <= alignment.parquet_only_count,
        "selected segments explain more IDs than the alignment Parquet-only population"
    );
    let metadata: Vec<&Candidate> = parquet_only
        .iter()
        .filter_map(|id| candidates.get(id))
        .collect();
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "completed",
        generated_at: Utc::now(),
        alignment_evidence: args.alignment_evidence.display().to_string(),
        alignment_evidence_sha256: alignment_sha256,
        snapshot_id: args.snapshot_id,
        clickhouse_database: args.clickhouse_database,
        clickhouse_table: "events_local",
        clickhouse_indexed_at_max_epoch: args.clickhouse_indexed_at_max_epoch,
        parquet_id_index: args.parquet_id_index.display().to_string(),
        parquet_id_index_size_bytes: args.parquet_id_index.metadata()?.len(),
        source_segments: segments,
        source_event_occurrences: occurrences,
        unique_source_ids: candidate_ids.len(),
        duplicate_source_occurrences: occurrences.saturating_sub(candidate_ids.len()),
        source_ids_sha256,
        source_ids_in_frozen_parquet: parquet_present.len(),
        source_ids_absent_from_frozen_parquet: candidate_ids.len() - parquet_present.len(),
        source_ids_in_clickhouse_at_barrier: in_clickhouse_at_barrier,
        source_ids_in_clickhouse_current: clickhouse_presence.len(),
        parquet_only_at_barrier: parquet_only.len(),
        parquet_only_now_in_clickhouse: now_in_clickhouse,
        parquet_only_still_absent_current: still_absent.len(),
        alignment_parquet_only_population: alignment.parquet_only_count,
        alignment_clickhouse_only_population: alignment.clickhouse_only_count,
        residual_parquet_only_after_selected_segments: alignment.parquet_only_count - explained,
        selected_segments_share_percent: percent(explained, alignment.parquet_only_count),
        parquet_only_created_at_min: metadata.iter().map(|row| row.created_at).min(),
        parquet_only_created_at_median: percentile(
            metadata.iter().map(|row| row.created_at).collect(),
            50,
        ),
        parquet_only_created_at_max: metadata.iter().map(|row| row.created_at).max(),
        parquet_only_top_kinds: top_kinds(&metadata, 20),
        parquet_only_examples: parquet_only
            .iter()
            .take(args.max_examples)
            .map(hex::encode)
            .collect(),
        checkpoint_directory: args.checkpoint_dir.display().to_string(),
        completed_batches: candidate_ids.len().div_ceil(args.clickhouse_batch_size),
        note: "The fixed barrier is reproduced from current ClickHouse parts. ReplacingMergeTree merges after the original alignment can remove older duplicate versions, so this is exact for currently retained rows at that barrier and explicitly reports later/current presence.",
    };
    candidates.clear();
    write_json_immutable(&args.output, &evidence)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    ensure!(
        args.alignment_evidence.is_file(),
        "alignment evidence is not a file"
    );
    ensure!(
        args.parquet_id_index.is_file(),
        "Parquet ID index is not a file"
    );
    ensure!(
        !args.source_segment.is_empty(),
        "at least one source segment is required"
    );
    ensure!(
        args.source_segment.iter().all(|path| path.is_file()),
        "a source segment is not a file"
    );
    let unique_paths: BTreeSet<_> = args.source_segment.iter().collect();
    ensure!(
        unique_paths.len() == args.source_segment.len(),
        "source segment paths must be unique"
    );
    ensure!(
        args.alignment_evidence_sha256.len() == 64
            && args
                .alignment_evidence_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "alignment evidence SHA-256 must be lowercase hexadecimal"
    );
    ensure!(
        args.clickhouse_batch_size > 0,
        "ClickHouse batch size must be positive"
    );
    ensure!(
        args.max_event_bytes > 0,
        "maximum event bytes must be positive"
    );
    Ok(())
}

fn validate_alignment(args: &Args, evidence: &AlignmentEvidence) -> Result<()> {
    ensure!(evidence.schema_version == 1, "unsupported alignment schema");
    ensure!(
        evidence.evidence_type == "pensieve-clickhouse-parquet-id-parity-v1",
        "unexpected alignment evidence type"
    );
    ensure!(
        evidence.status == "failed",
        "alignment evidence is not a parity failure"
    );
    ensure!(
        evidence.snapshot_id == args.snapshot_id,
        "alignment snapshot mismatch"
    );
    ensure!(
        evidence.clickhouse_database == args.clickhouse_database,
        "ClickHouse database mismatch"
    );
    ensure!(
        evidence.clickhouse_table == "events_local",
        "ClickHouse table mismatch"
    );
    ensure!(
        evidence.clickhouse_indexed_at_max_epoch == args.clickhouse_indexed_at_max_epoch,
        "ClickHouse barrier mismatch"
    );
    ensure!(
        evidence.shards_completed == 256,
        "alignment did not complete all shards"
    );
    Ok(())
}

fn scan_sources(args: &Args) -> Result<ScannedSources> {
    let mut candidates = BTreeMap::<[u8; 32], Candidate>::new();
    let mut evidence = Vec::with_capacity(args.source_segment.len());
    let mut occurrences = 0usize;
    for (segment_index, path) in args.source_segment.iter().enumerate() {
        let scan = scan_segment(path, args.max_event_bytes)
            .with_context(|| format!("scan {}", path.display()))?;
        occurrences = occurrences
            .checked_add(scan.events.len())
            .context("source occurrence count overflow")?;
        let mut unique = HashSet::new();
        for event in &scan.events {
            unique.insert(*event.id());
            let candidate = candidates.entry(*event.id()).or_insert_with(|| Candidate {
                created_at: event.created_at(),
                kind: event.kind(),
                segments: BTreeSet::new(),
            });
            ensure!(
                candidate.created_at == event.created_at() && candidate.kind == event.kind(),
                "duplicate ID has inconsistent committed metadata"
            );
            candidate.segments.insert(segment_index);
        }
        evidence.push(SegmentEvidence {
            path: path.display().to_string(),
            size_bytes: path.metadata()?.len(),
            sha256: sha256_file(path)?,
            valid_events: scan.events.len(),
            rejected_events: scan.rejected.len(),
            unique_ids: unique.len(),
            parquet_only_at_barrier: 0,
            parquet_only_still_absent_current: 0,
        });
    }
    Ok(ScannedSources {
        candidates,
        segments: evidence,
        occurrences,
    })
}

fn parquet_membership(
    index_path: &Path,
    snapshot_id: &str,
    memory_limit: &str,
    ids: &[[u8; 32]],
) -> Result<HashSet<[u8; 32]>> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(&format!(
        "SET threads = 1; SET memory_limit = {}; SET preserve_insertion_order = false; CREATE TEMP TABLE candidates(id BLOB NOT NULL);",
        sql_string(memory_limit)
    ))?;
    {
        let mut appender = connection.appender("candidates")?;
        for id in ids {
            appender.append_row(params![id.as_slice()])?;
        }
        appender.flush()?;
    }
    let index_path = sql_string(&index_path.display().to_string());
    connection.execute_batch(&format!("ATTACH {index_path} AS frozen (READ_ONLY)"))?;
    let encoded: String = connection.query_row(
        "SELECT metadata_json FROM frozen.alignment_metadata WHERE singleton = true",
        [],
        |row| row.get(0),
    )?;
    let metadata: IndexMetadata = serde_json::from_str(&encoded)?;
    ensure!(
        metadata.schema_version == 1
            && metadata.runner_version == "pensieve-analytics-align-ids-v1",
        "unexpected ID index metadata"
    );
    ensure!(
        metadata.snapshot_id == snapshot_id,
        "ID index snapshot mismatch"
    );
    ensure!(metadata.event_count > 0, "ID index is empty");
    let mut statement =
        connection.prepare("SELECT c.id FROM candidates c SEMI JOIN frozen.ids f USING (id)")?;
    let mut rows = statement.query([])?;
    let mut present = HashSet::with_capacity(ids.len());
    while let Some(row) = rows.next()? {
        let value: Vec<u8> = row.get(0)?;
        let id: [u8; 32] = value
            .try_into()
            .map_err(|value: Vec<u8>| anyhow::anyhow!("DuckDB ID has {} bytes", value.len()))?;
        ensure!(
            present.insert(id),
            "duplicate candidate returned by frozen ID index"
        );
    }
    Ok(present)
}

async fn clickhouse_membership(args: &Args, ids: &[[u8; 32]]) -> Result<BTreeMap<[u8; 32], u32>> {
    let client = connect_clickhouse(args);
    let mut output = BTreeMap::new();
    for (batch_index, batch) in ids.chunks(args.clickhouse_batch_size).enumerate() {
        let input_sha256 = sha256_ids(batch);
        let path = args
            .checkpoint_dir
            .join(format!("batch-{batch_index:05}.json"));
        let (checkpoint, resumed) = if path.exists() {
            let checkpoint: BatchCheckpoint = serde_json::from_slice(&fs::read(&path)?)?;
            validate_batch(args, batch_index, batch, &input_sha256, &checkpoint)?;
            (checkpoint, true)
        } else {
            let input: Vec<String> = batch.iter().map(hex::encode).collect();
            let mut rows = client
                .query(
                    "SELECT id, toUInt32(min(indexed_at)) AS first_indexed_at FROM events_local WHERE id IN {ids:Array(String)} GROUP BY id ORDER BY id SETTINGS max_memory_usage={max_memory:UInt64}, max_execution_time={max_time:UInt64}",
                )
                .param("ids", input)
                .param("max_memory", args.clickhouse_max_memory_usage)
                .param("max_time", args.clickhouse_max_execution_time)
                .fetch_all::<ClickhousePresenceRow>()
                .await?;
            rows.sort_by(|left, right| left.id.cmp(&right.id));
            let checkpoint = BatchCheckpoint {
                schema_version: SCHEMA_VERSION,
                runner_version: RUNNER_VERSION.to_owned(),
                snapshot_id: args.snapshot_id.clone(),
                clickhouse_database: args.clickhouse_database.clone(),
                clickhouse_indexed_at_max_epoch: args.clickhouse_indexed_at_max_epoch,
                batch_index,
                input_count: batch.len(),
                input_sha256: input_sha256.clone(),
                rows,
                completed_at: Utc::now(),
            };
            validate_batch(args, batch_index, batch, &input_sha256, &checkpoint)?;
            write_json_immutable(&path, &checkpoint)?;
            (checkpoint, false)
        };
        for row in checkpoint.rows {
            let decoded = hex::decode(&row.id)?;
            let id: [u8; 32] = decoded.try_into().map_err(|value: Vec<u8>| {
                anyhow::anyhow!("ClickHouse ID has {} bytes", value.len())
            })?;
            ensure!(
                output.insert(id, row.first_indexed_at).is_none(),
                "ClickHouse returned a candidate ID more than once"
            );
        }
        eprintln!(
            "ClickHouse candidate batch {} of {} {}",
            batch_index + 1,
            ids.len().div_ceil(args.clickhouse_batch_size),
            if resumed { "resumed" } else { "completed" }
        );
        if !resumed
            && args.batch_delay_millis > 0
            && batch_index + 1 < ids.len().div_ceil(args.clickhouse_batch_size)
        {
            tokio::time::sleep(Duration::from_millis(args.batch_delay_millis)).await;
        }
    }
    Ok(output)
}

fn validate_batch(
    args: &Args,
    batch_index: usize,
    batch: &[[u8; 32]],
    input_sha256: &str,
    checkpoint: &BatchCheckpoint,
) -> Result<()> {
    ensure!(
        checkpoint.schema_version == SCHEMA_VERSION && checkpoint.runner_version == RUNNER_VERSION,
        "batch checkpoint version mismatch"
    );
    ensure!(
        checkpoint.snapshot_id == args.snapshot_id
            && checkpoint.clickhouse_database == args.clickhouse_database,
        "batch checkpoint source mismatch"
    );
    ensure!(
        checkpoint.clickhouse_indexed_at_max_epoch == args.clickhouse_indexed_at_max_epoch,
        "batch checkpoint barrier mismatch"
    );
    ensure!(
        checkpoint.batch_index == batch_index
            && checkpoint.input_count == batch.len()
            && checkpoint.input_sha256 == input_sha256,
        "batch checkpoint input mismatch"
    );
    let input: HashSet<[u8; 32]> = batch.iter().copied().collect();
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
            "ClickHouse returned an ID outside the candidate batch"
        );
        previous = Some(&row.id);
    }
    Ok(())
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

fn top_kinds(rows: &[&Candidate], limit: usize) -> Vec<KindCount> {
    let mut counts = BTreeMap::<u16, usize>::new();
    for row in rows {
        *counts.entry(row.kind).or_default() += 1;
    }
    let mut counts: Vec<_> = counts
        .into_iter()
        .map(|(kind, count)| KindCount { kind, count })
        .collect();
    counts.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then(left.kind.cmp(&right.kind))
    });
    counts.truncate(limit);
    counts
}

fn percentile(mut values: Vec<u64>, percentile: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = (values.len() - 1) * percentile / 100;
    values.get(index).copied()
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
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

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
    use duckdb::{Connection, params};
    use tempfile::tempdir;

    use super::{parquet_membership, percent, percentile};

    #[test]
    fn percentile_uses_nearest_rank_floor() {
        assert_eq!(percentile(vec![5, 1, 3, 2, 4], 50), Some(3));
        assert_eq!(percentile(Vec::new(), 50), None);
    }

    #[test]
    fn percent_handles_zero_denominator() {
        assert_eq!(percent(1, 4), 25.0);
        assert_eq!(percent(1, 0), 0.0);
    }

    #[test]
    fn parquet_membership_joins_candidates_against_read_only_index() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("index.duckdb");
        let present = [7_u8; 32];
        let absent = [8_u8; 32];
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE ids(id BLOB NOT NULL); CREATE TABLE alignment_metadata(singleton BOOLEAN PRIMARY KEY, metadata_json VARCHAR NOT NULL);",
                )
                .unwrap();
            connection
                .execute("INSERT INTO ids VALUES (?)", params![present.as_slice()])
                .unwrap();
            let metadata = serde_json::json!({
                "schema_version": 1,
                "runner_version": "pensieve-analytics-align-ids-v1",
                "snapshot_id": "sha256:test",
                "event_count": 1
            });
            connection
                .execute(
                    "INSERT INTO alignment_metadata VALUES (true, ?)",
                    params![metadata.to_string()],
                )
                .unwrap();
        }
        let result = parquet_membership(&path, "sha256:test", "64MB", &[present, absent]).unwrap();
        assert_eq!(result, [present].into_iter().collect());
    }
}
