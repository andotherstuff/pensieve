//! Export and cryptographically validate an exact ClickHouse-only residual population.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use nostr_sdk::prelude::JsonUtil;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-recover-clickhouse-residual-v1";
const SOURCE_RUNNER_VERSION: &str = "pensieve-analytics-classify-clickhouse-residual-v1";
type OutputIds = (Vec<[u8; 32]>, BTreeMap<String, u64>);

#[derive(Debug, Parser)]
#[command(about = "Export and validate an exact ClickHouse-only residual population")]
struct Args {
    /// Completed residual classification evidence.
    #[arg(long)]
    residual_evidence: PathBuf,
    /// Required SHA-256 of the residual classification evidence.
    #[arg(long)]
    residual_evidence_sha256: String,
    /// Directory containing the residual classifier's immutable batch checkpoints.
    #[arg(long)]
    residual_checkpoint_dir: PathBuf,
    /// New or resumable recovery root containing atomic batch bundles.
    #[arg(long)]
    output_root: PathBuf,
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
    /// Pause after each newly completed ClickHouse batch.
    #[arg(long, default_value_t = 200)]
    batch_delay_millis: u64,
    /// Maximum rejected-event examples retained in final evidence.
    #[arg(long, default_value_t = 20)]
    max_examples: usize,
}

#[derive(Debug, Deserialize)]
struct ResidualEvidence {
    schema_version: u32,
    runner_version: String,
    status: String,
    residual_ids: u64,
    residual_ids_sha256: String,
    clickhouse_database: String,
    clickhouse_rows_found: u64,
    clickhouse_rows_missing: u64,
    completed_batches: usize,
    checkpoint_directory: String,
}

#[derive(Debug, Deserialize)]
struct SourceCheckpoint {
    schema_version: u32,
    runner_version: String,
    residual_ids_sha256: String,
    clickhouse_database: String,
    batch_index: usize,
    input_count: usize,
    input_sha256: String,
    rows: Vec<SourceRow>,
}

#[derive(Debug, Deserialize)]
struct SourceRow {
    id: String,
}

#[derive(Clone, Debug, Deserialize, Row)]
struct ClickhouseEventRow {
    id: String,
    pubkey: String,
    created_at: u32,
    kind: u16,
    content: String,
    sig: String,
    tags: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RecoveryCheckpoint {
    schema_version: u32,
    runner_version: String,
    residual_evidence_sha256: String,
    residual_ids_sha256: String,
    clickhouse_database: String,
    batch_index: usize,
    input_count: usize,
    input_sha256: String,
    valid_count: usize,
    valid_ids_sha256: String,
    valid_file: String,
    valid_bytes: u64,
    valid_sha256: String,
    rejected_count: usize,
    rejected_ids_sha256: String,
    rejected_file: String,
    rejected_bytes: u64,
    rejected_sha256: String,
    completed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct CountAttribution {
    value: String,
    count: u64,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    residual_evidence: String,
    residual_evidence_sha256: String,
    residual_ids: u64,
    residual_ids_sha256: String,
    clickhouse_database: String,
    clickhouse_table: &'static str,
    clickhouse_rows_exported: u64,
    valid_events: u64,
    valid_ids_sha256: String,
    rejected_events: u64,
    rejected_ids_sha256: String,
    rejection_reasons: Vec<CountAttribution>,
    rejected_examples: Vec<String>,
    completed_batches: usize,
    batch_root: String,
    valid_jsonl_root: String,
    rejected_jsonl_root: String,
    note: &'static str,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ClickHouse residual recovery failed: {error:#}");
        std::process::exit(1);
    }
}

#[tokio::main(flavor = "current_thread")]
async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let final_evidence = args.output_root.join("evidence.json");
    ensure!(
        !final_evidence.exists(),
        "refusing to replace immutable recovery evidence {}",
        final_evidence.display()
    );
    fs::create_dir_all(args.output_root.join("batches"))?;

    let residual_bytes = fs::read(&args.residual_evidence)?;
    let residual_sha256 = sha256_bytes(&residual_bytes);
    ensure!(
        residual_sha256 == args.residual_evidence_sha256,
        "residual evidence SHA-256 mismatch"
    );
    let residual: ResidualEvidence = serde_json::from_slice(&residual_bytes)?;
    validate_residual(&args, &residual)?;
    let batches = load_source_batches(&args, &residual)?;
    let mut all_ids = Vec::new();
    for batch in &batches {
        all_ids.extend_from_slice(batch);
    }
    ensure!(
        u64::try_from(all_ids.len())? == residual.residual_ids,
        "source checkpoints do not reproduce the residual count"
    );
    ensure!(
        sha256_ids(&all_ids) == residual.residual_ids_sha256,
        "source checkpoints do not reproduce the residual ID hash"
    );

    let client = connect_clickhouse(&args);
    for (batch_index, ids) in batches.iter().enumerate() {
        let bundle = args
            .output_root
            .join("batches")
            .join(format!("batch-{batch_index:05}"));
        let resumed = if bundle.is_dir() {
            validate_bundle(&args, &residual, batch_index, ids, &bundle)?;
            true
        } else {
            let query = clickhouse_event_query(ids);
            let rows = client
                .query(&query)
                .fetch_all::<ClickhouseEventRow>()
                .await?;
            publish_bundle(&args, &residual, batch_index, ids, rows, &bundle)?;
            false
        };
        eprintln!(
            "recovery batch {} of {} {}",
            batch_index + 1,
            batches.len(),
            if resumed { "resumed" } else { "completed" }
        );
        if !resumed && args.batch_delay_millis > 0 && batch_index + 1 < batches.len() {
            tokio::time::sleep(Duration::from_millis(args.batch_delay_millis)).await;
        }
    }

    publish_flat_views(&args, &residual, &batches)?;
    let evidence = build_evidence(&args, &residual, residual_sha256, &batches)?;
    write_json_create_new(&final_evidence, &evidence)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    ensure!(
        args.residual_evidence.is_file(),
        "residual evidence is missing"
    );
    ensure!(
        args.residual_checkpoint_dir.is_dir(),
        "residual checkpoint directory is missing"
    );
    ensure!(
        args.output_root != args.residual_checkpoint_dir,
        "recovery output must not replace source checkpoints"
    );
    Ok(())
}

fn validate_residual(args: &Args, residual: &ResidualEvidence) -> Result<()> {
    ensure!(
        residual.schema_version == 1
            && residual.runner_version == SOURCE_RUNNER_VERSION
            && residual.status == "completed",
        "unsupported residual evidence"
    );
    ensure!(
        residual.clickhouse_database == args.clickhouse_database,
        "residual evidence uses a different ClickHouse database"
    );
    ensure!(
        residual.clickhouse_rows_found == residual.residual_ids
            && residual.clickhouse_rows_missing == 0,
        "residual evidence does not prove complete ClickHouse presence"
    );
    ensure!(
        residual.checkpoint_directory == args.residual_checkpoint_dir.display().to_string(),
        "residual evidence names a different checkpoint directory"
    );
    Ok(())
}

fn load_source_batches(args: &Args, residual: &ResidualEvidence) -> Result<Vec<Vec<[u8; 32]>>> {
    let mut batches = Vec::with_capacity(residual.completed_batches);
    let mut previous = None;
    for batch_index in 0..residual.completed_batches {
        let path = args
            .residual_checkpoint_dir
            .join(format!("batch-{batch_index:05}.json"));
        let checkpoint: SourceCheckpoint = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            checkpoint.schema_version == 1
                && checkpoint.runner_version == SOURCE_RUNNER_VERSION
                && checkpoint.residual_ids_sha256 == residual.residual_ids_sha256
                && checkpoint.clickhouse_database == residual.clickhouse_database
                && checkpoint.batch_index == batch_index
                && checkpoint.input_count == checkpoint.rows.len(),
            "residual checkpoint identity mismatch for batch {batch_index}"
        );
        let mut ids = Vec::with_capacity(checkpoint.rows.len());
        for row in checkpoint.rows {
            let id = decode_id(&row.id)?;
            if let Some(previous) = previous {
                ensure!(
                    previous < id,
                    "residual IDs are not globally sorted and unique"
                );
            }
            previous = Some(id);
            ids.push(id);
        }
        ensure!(
            sha256_ids(&ids) == checkpoint.input_sha256,
            "residual checkpoint input hash mismatch for batch {batch_index}"
        );
        batches.push(ids);
    }
    Ok(batches)
}

fn publish_bundle(
    args: &Args,
    residual: &ResidualEvidence,
    batch_index: usize,
    ids: &[[u8; 32]],
    rows: Vec<ClickhouseEventRow>,
    bundle: &Path,
) -> Result<()> {
    ensure!(
        rows.len() == ids.len(),
        "ClickHouse row count mismatch for batch {batch_index}"
    );
    let mut valid_bytes = Vec::new();
    let mut rejected_bytes = Vec::new();
    let mut valid_ids = Vec::new();
    let mut rejected_ids = Vec::new();
    for (expected, row) in ids.iter().zip(rows) {
        let row_id = decode_id(&row.id)?;
        ensure!(
            *expected == row_id,
            "ClickHouse returned an unexpected or unsorted ID"
        );
        let raw = json!({
            "id": row.id,
            "pubkey": row.pubkey,
            "created_at": row.created_at,
            "kind": row.kind,
            "tags": row.tags,
            "content": row.content,
            "sig": row.sig,
        });
        let raw_json = serde_json::to_string(&raw)?;
        match pensieve_core::validate_event(&raw_json) {
            Ok(event) => {
                ensure!(*event.id.as_bytes() == row_id, "validated event ID changed");
                valid_bytes.extend_from_slice(event.as_json().as_bytes());
                valid_bytes.push(b'\n');
                valid_ids.push(row_id);
            }
            Err(error) => {
                let rejection =
                    json!({"id": hex::encode(row_id), "error": error.to_string(), "event": raw});
                serde_json::to_writer(&mut rejected_bytes, &rejection)?;
                rejected_bytes.push(b'\n');
                rejected_ids.push(row_id);
            }
        }
    }
    ensure!(
        valid_ids.len() + rejected_ids.len() == ids.len(),
        "recovery partition does not cover the input batch"
    );

    let parent = bundle.parent().context("bundle has no parent")?;
    let temporary = parent.join(format!(
        ".batch-{batch_index:05}.partial-{}",
        std::process::id()
    ));
    fs::create_dir(&temporary)?;
    let valid_path = temporary.join("valid.jsonl");
    let rejected_path = temporary.join("rejected.jsonl");
    write_bytes_create_new(&valid_path, &valid_bytes)?;
    write_bytes_create_new(&rejected_path, &rejected_bytes)?;
    let checkpoint = RecoveryCheckpoint {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION.to_owned(),
        residual_evidence_sha256: args.residual_evidence_sha256.clone(),
        residual_ids_sha256: residual.residual_ids_sha256.clone(),
        clickhouse_database: args.clickhouse_database.clone(),
        batch_index,
        input_count: ids.len(),
        input_sha256: sha256_ids(ids),
        valid_count: valid_ids.len(),
        valid_ids_sha256: sha256_ids(&valid_ids),
        valid_file: "valid.jsonl".to_owned(),
        valid_bytes: u64::try_from(valid_bytes.len())?,
        valid_sha256: sha256_bytes(&valid_bytes),
        rejected_count: rejected_ids.len(),
        rejected_ids_sha256: sha256_ids(&rejected_ids),
        rejected_file: "rejected.jsonl".to_owned(),
        rejected_bytes: u64::try_from(rejected_bytes.len())?,
        rejected_sha256: sha256_bytes(&rejected_bytes),
        completed_at: Utc::now(),
    };
    write_json_create_new(&temporary.join("checkpoint.json"), &checkpoint)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(&temporary, bundle)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn validate_bundle(
    args: &Args,
    residual: &ResidualEvidence,
    batch_index: usize,
    ids: &[[u8; 32]],
    bundle: &Path,
) -> Result<RecoveryCheckpoint> {
    let checkpoint: RecoveryCheckpoint =
        serde_json::from_slice(&fs::read(bundle.join("checkpoint.json"))?)?;
    ensure!(
        checkpoint.schema_version == SCHEMA_VERSION
            && checkpoint.runner_version == RUNNER_VERSION
            && checkpoint.residual_evidence_sha256 == args.residual_evidence_sha256
            && checkpoint.residual_ids_sha256 == residual.residual_ids_sha256
            && checkpoint.clickhouse_database == args.clickhouse_database
            && checkpoint.batch_index == batch_index
            && checkpoint.input_count == ids.len()
            && checkpoint.input_sha256 == sha256_ids(ids),
        "recovery checkpoint identity mismatch for batch {batch_index}"
    );
    validate_file(
        &bundle.join(&checkpoint.valid_file),
        checkpoint.valid_bytes,
        &checkpoint.valid_sha256,
    )?;
    validate_file(
        &bundle.join(&checkpoint.rejected_file),
        checkpoint.rejected_bytes,
        &checkpoint.rejected_sha256,
    )?;
    let (valid_ids, _) = read_output_ids(&bundle.join(&checkpoint.valid_file), false)?;
    let (rejected_ids, _) = read_output_ids(&bundle.join(&checkpoint.rejected_file), true)?;
    ensure!(
        valid_ids.len() == checkpoint.valid_count
            && sha256_ids(&valid_ids) == checkpoint.valid_ids_sha256
            && rejected_ids.len() == checkpoint.rejected_count
            && sha256_ids(&rejected_ids) == checkpoint.rejected_ids_sha256,
        "recovery bundle ID accounting mismatch for batch {batch_index}"
    );
    let mut partition = valid_ids;
    partition.extend(rejected_ids);
    partition.sort_unstable();
    ensure!(
        partition == ids,
        "recovery bundle does not partition batch {batch_index}"
    );
    Ok(checkpoint)
}

fn build_evidence(
    args: &Args,
    residual: &ResidualEvidence,
    residual_evidence_sha256: String,
    batches: &[Vec<[u8; 32]>],
) -> Result<Evidence> {
    let mut valid_ids = Vec::new();
    let mut rejected_ids = Vec::new();
    let mut reasons = BTreeMap::<String, u64>::new();
    let mut rejected_examples = Vec::new();
    for (batch_index, ids) in batches.iter().enumerate() {
        let bundle = args
            .output_root
            .join("batches")
            .join(format!("batch-{batch_index:05}"));
        let checkpoint = validate_bundle(args, residual, batch_index, ids, &bundle)?;
        let (mut batch_valid, _) = read_output_ids(&bundle.join(checkpoint.valid_file), false)?;
        let (mut batch_rejected, batch_reasons) =
            read_output_ids(&bundle.join(checkpoint.rejected_file), true)?;
        valid_ids.append(&mut batch_valid);
        for (reason, count) in batch_reasons {
            *reasons.entry(reason).or_default() += count;
        }
        for id in &batch_rejected {
            if rejected_examples.len() < args.max_examples {
                rejected_examples.push(hex::encode(id));
            }
        }
        rejected_ids.append(&mut batch_rejected);
    }
    let mut partition = valid_ids.clone();
    partition.extend_from_slice(&rejected_ids);
    partition.sort_unstable();
    ensure!(
        u64::try_from(partition.len())? == residual.residual_ids
            && sha256_ids(&partition) == residual.residual_ids_sha256,
        "final recovery output does not partition the residual population"
    );
    Ok(Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "completed",
        generated_at: Utc::now(),
        residual_evidence: args.residual_evidence.display().to_string(),
        residual_evidence_sha256,
        residual_ids: residual.residual_ids,
        residual_ids_sha256: residual.residual_ids_sha256.clone(),
        clickhouse_database: args.clickhouse_database.clone(),
        clickhouse_table: "events_local FINAL",
        clickhouse_rows_exported: residual.residual_ids,
        valid_events: u64::try_from(valid_ids.len())?,
        valid_ids_sha256: sha256_ids(&valid_ids),
        rejected_events: u64::try_from(rejected_ids.len())?,
        rejected_ids_sha256: sha256_ids(&rejected_ids),
        rejection_reasons: reasons
            .into_iter()
            .map(|(value, count)| CountAttribution { value, count })
            .collect(),
        rejected_examples,
        completed_batches: batches.len(),
        batch_root: args.output_root.join("batches").display().to_string(),
        valid_jsonl_root: args.output_root.join("valid").display().to_string(),
        rejected_jsonl_root: args.output_root.join("rejected").display().to_string(),
        note: "Every exported row is reconstructed from all seven Nostr event fields and cryptographically validated. Valid and rejected outputs form an exact, disjoint partition of the frozen residual ID population.",
    })
}

fn publish_flat_views(
    args: &Args,
    residual: &ResidualEvidence,
    batches: &[Vec<[u8; 32]>],
) -> Result<()> {
    for (view_name, checkpoint_file) in [("valid", "valid.jsonl"), ("rejected", "rejected.jsonl")] {
        let destination = args.output_root.join(view_name);
        if destination.is_dir() {
            validate_flat_view(args, residual, batches, view_name, checkpoint_file)?;
            continue;
        }

        let temporary = args
            .output_root
            .join(format!(".{view_name}.partial-{}", std::process::id()));
        fs::create_dir(&temporary)?;
        for (batch_index, ids) in batches.iter().enumerate() {
            let bundle = args
                .output_root
                .join("batches")
                .join(format!("batch-{batch_index:05}"));
            let checkpoint = validate_bundle(args, residual, batch_index, ids, &bundle)?;
            let source = match checkpoint_file {
                "valid.jsonl" => bundle.join(checkpoint.valid_file),
                "rejected.jsonl" => bundle.join(checkpoint.rejected_file),
                _ => unreachable!("flat view file is fixed by the runner"),
            };
            fs::hard_link(
                source,
                temporary.join(format!("batch-{batch_index:05}.jsonl")),
            )?;
        }
        File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, &destination)?;
        File::open(&args.output_root)?.sync_all()?;
        validate_flat_view(args, residual, batches, view_name, checkpoint_file)?;
    }
    Ok(())
}

fn validate_flat_view(
    args: &Args,
    residual: &ResidualEvidence,
    batches: &[Vec<[u8; 32]>],
    view_name: &str,
    checkpoint_file: &str,
) -> Result<()> {
    let root = args.output_root.join(view_name);
    ensure!(root.is_dir(), "flat {view_name} JSONL view is missing");
    ensure!(
        fs::read_dir(&root)?.count() == batches.len(),
        "flat {view_name} JSONL view has unexpected entries"
    );
    for (batch_index, ids) in batches.iter().enumerate() {
        let bundle = args
            .output_root
            .join("batches")
            .join(format!("batch-{batch_index:05}"));
        let checkpoint = validate_bundle(args, residual, batch_index, ids, &bundle)?;
        let (expected_bytes, expected_sha256) = match checkpoint_file {
            "valid.jsonl" => (checkpoint.valid_bytes, checkpoint.valid_sha256),
            "rejected.jsonl" => (checkpoint.rejected_bytes, checkpoint.rejected_sha256),
            _ => unreachable!("flat view file is fixed by the runner"),
        };
        validate_file(
            &root.join(format!("batch-{batch_index:05}.jsonl")),
            expected_bytes,
            &expected_sha256,
        )?;
    }
    Ok(())
}

fn read_output_ids(path: &Path, rejected: bool) -> Result<OutputIds> {
    let mut ids = Vec::new();
    let mut reasons = BTreeMap::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let value: serde_json::Value = serde_json::from_str(&line?)?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .context("output row is missing its ID")?;
        ids.push(decode_id(id)?);
        if rejected {
            let reason = value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .context("rejected row is missing its error")?;
            *reasons.entry(reason.to_owned()).or_default() += 1;
        }
    }
    Ok((ids, reasons))
}

fn clickhouse_event_query(ids: &[[u8; 32]]) -> String {
    let ids = ids
        .iter()
        .map(|id| format!("'{}'", hex::encode(id)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "SELECT id, pubkey, toUInt32(created_at) AS created_at, kind, content, sig, tags
         FROM events_local FINAL WHERE id IN ({ids}) ORDER BY id"
    )
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

fn decode_id(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid lowercase event ID"
    );
    let decoded = hex::decode(value)?;
    decoded
        .try_into()
        .map_err(|value: Vec<u8>| anyhow::anyhow!("event ID has {} bytes", value.len()))
}

fn validate_file(path: &Path, expected_bytes: u64, expected_sha256: &str) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == expected_bytes,
        "file size mismatch for {}",
        path.display()
    );
    ensure!(
        sha256_file(path)? == expected_sha256,
        "file SHA-256 mismatch for {}",
        path.display()
    );
    Ok(())
}

fn write_bytes_create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_json_create_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{clickhouse_event_query, decode_id, sha256_ids};

    #[test]
    fn query_uses_final_and_fixed_width_hex_ids() {
        let query = clickhouse_event_query(&[[0xab; 32], [1; 32]]);
        assert!(query.contains("events_local FINAL"));
        assert!(query.contains(&format!("'{}'", "ab".repeat(32))));
        assert!(query.contains(&format!("'{}'", "01".repeat(32))));
    }

    #[test]
    fn id_decode_and_digest_are_strict() {
        let id = decode_id(&"ab".repeat(32)).expect("valid ID");
        assert_eq!(id, [0xab; 32]);
        assert!(decode_id(&"AB".repeat(32)).is_err());
        assert_eq!(sha256_ids(&[id]).len(), 64);
    }
}
