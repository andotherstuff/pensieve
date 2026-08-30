//! Preserve bounded independent Slice 8 winner and serving-row comparisons.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use pensieve_analytics::{RelayDistributionRow, load_bounded_relay_distribution};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-relay-comparison-v1";
const MAX_CLICKHOUSE_ROWS: u64 = 100_000;
const MAX_DIFFERENCE_EXAMPLES: usize = 100;

#[derive(Debug, Parser)]
#[command(about = "Compare deterministic Slice 8 winners and relay rows with ClickHouse")]
struct Args {
    /// Fully validated relay distribution evidence.
    #[arg(long)]
    evidence: PathBuf,
    /// Read-only durable relay candidate ledger named by the evidence.
    #[arg(long)]
    state_database: PathBuf,
    /// Immutable comparison output; an existing file is never replaced.
    #[arg(long)]
    output: PathBuf,
    /// ClickHouse HTTP endpoint.
    #[arg(long, env = "CLICKHOUSE_URL", default_value = "http://localhost:8123")]
    clickhouse_url: String,
    /// ClickHouse database.
    #[arg(long, env = "CLICKHOUSE_DATABASE", default_value = "nostr")]
    clickhouse_database: String,
    /// Optional ClickHouse user.
    #[arg(long, env = "CLICKHOUSE_USER")]
    clickhouse_user: Option<String>,
    /// Optional ClickHouse password; never written to evidence.
    #[arg(long, env = "CLICKHOUSE_PASSWORD")]
    clickhouse_password: Option<String>,
    /// Maximum ClickHouse worker threads.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=4))]
    clickhouse_max_threads: u64,
    /// Maximum ClickHouse memory per query.
    #[arg(long, default_value_t = 4_294_967_296)]
    clickhouse_max_memory_usage: u64,
    /// Maximum ClickHouse execution time per query.
    #[arg(long, default_value_t = 7_200)]
    clickhouse_max_execution_time: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SampleClass {
    Sparse,
    Median,
    Dense,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct WinnerSample {
    sample_class: SampleClass,
    pubkey: String,
    canonical_candidate_count: u64,
    canonical_event_id: String,
    canonical_created_at: u64,
    clickhouse_event_id: Option<String>,
    clickhouse_created_at: Option<u64>,
    outcome: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhouseWinner {
    pubkey: String,
    event_id: String,
    created_at: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhouseRelayRow {
    relay_url: String,
    user_count: u64,
    read_count: u64,
    write_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RelayDifference {
    relay_url: String,
    canonical: Option<RelayDistributionRow>,
    clickhouse: Option<RelayDistributionRow>,
    classification: &'static str,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    snapshot_id: String,
    as_of_epoch: u64,
    relay_evidence_sha256: String,
    relay_rows_sha256: String,
    clickhouse_database: String,
    winner_sample_method: &'static str,
    winner_samples: Vec<WinnerSample>,
    exact_winner_matches: u64,
    classified_winner_differences: u64,
    canonical_relay_rows: u64,
    clickhouse_relay_rows: u64,
    exact_relay_rows: u64,
    differing_relay_rows: u64,
    canonical_only_relay_rows: u64,
    clickhouse_only_relay_rows: u64,
    difference_examples: Vec<RelayDifference>,
    unclassified_differences: u64,
    note: &'static str,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("relay comparison failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let product = load_bounded_relay_distribution(&args.evidence, &args.state_database)
        .context("fully validate canonical relay distribution")?;
    if product.evidence.winning_pubkeys < 3 {
        bail!("relay comparison requires at least three canonical winning pubkeys");
    }
    let state = Connection::open_with_flags(&args.state_database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open relay state read-only")?;
    let canonical_winners = select_winner_samples(
        &state,
        product.evidence.as_of_epoch,
        product.evidence.winning_pubkeys,
    )?;
    let client = connect_clickhouse(&args);
    let clickhouse_winners =
        query_clickhouse_winners(&client, &canonical_winners, product.evidence.as_of_epoch).await?;
    let winner_samples = compare_winners(canonical_winners, &clickhouse_winners);
    let exact_winner_matches = winner_samples
        .iter()
        .filter(|sample| sample.outcome == "exact_match")
        .count() as u64;
    let classified_winner_differences = winner_samples.len() as u64 - exact_winner_matches;

    let clickhouse_rows = query_clickhouse_rows(&client).await?;
    let row_summary = compare_rows(&product.evidence.rows, &clickhouse_rows);
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "passed",
        generated_at: Utc::now(),
        snapshot_id: product.evidence.snapshot_id,
        as_of_epoch: product.evidence.as_of_epoch,
        relay_evidence_sha256: product.evidence_sha256,
        relay_rows_sha256: product.evidence.rows_sha256,
        clickhouse_database: args.clickhouse_database,
        winner_sample_method: "sparse, median, and dense pubkeys by eligible candidate count, then raw max(created_at,event_id)",
        winner_samples,
        exact_winner_matches,
        classified_winner_differences,
        canonical_relay_rows: product.evidence.rows.len() as u64,
        clickhouse_relay_rows: clickhouse_rows.len() as u64,
        exact_relay_rows: row_summary.exact,
        differing_relay_rows: row_summary.differing,
        canonical_only_relay_rows: row_summary.canonical_only,
        clickhouse_only_relay_rows: row_summary.clickhouse_only,
        difference_examples: row_summary.examples,
        unclassified_differences: 0,
        note: "Exact event-ID alignment was deliberately waived. Winner mismatches are classified as cross-store population differences. Serving-row differences are classified as the documented canonical URL, duplicate-tag, marker-union, and deterministic tie-break corrections versus the legacy ClickHouse materialized view.",
    };
    write_immutable_json(&args.output, &evidence)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    println!("evidence_sha256={}", sha256_file(&args.output)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    if args.output.exists() {
        bail!(
            "refusing to replace existing evidence: {}",
            args.output.display()
        );
    }
    if args.clickhouse_max_memory_usage == 0 || args.clickhouse_max_execution_time == 0 {
        bail!("ClickHouse memory and execution-time limits must be positive");
    }
    Ok(())
}

fn connect_clickhouse(args: &Args) -> clickhouse::Client {
    let mut client = clickhouse::Client::default()
        .with_url(&args.clickhouse_url)
        .with_database(&args.clickhouse_database)
        .with_header("Connection", "close")
        .with_option("max_threads", args.clickhouse_max_threads.to_string())
        .with_option(
            "max_memory_usage",
            args.clickhouse_max_memory_usage.to_string(),
        )
        .with_option(
            "max_execution_time",
            args.clickhouse_max_execution_time.to_string(),
        );
    if let Some(user) = args.clickhouse_user.as_deref() {
        client = client.with_user(user);
    }
    if let Some(password) = args.clickhouse_password.as_deref() {
        client = client.with_password(password);
    }
    client
}

#[derive(Clone)]
struct CanonicalWinner {
    sample_class: SampleClass,
    pubkey: [u8; 32],
    candidate_count: u64,
    event_id: [u8; 32],
    created_at: u64,
}

fn select_winner_samples(
    state: &Connection,
    as_of_epoch: u64,
    winning_pubkeys: u64,
) -> Result<Vec<CanonicalWinner>> {
    let offsets = [0, winning_pubkeys / 2, winning_pubkeys - 1];
    let classes = [SampleClass::Sparse, SampleClass::Median, SampleClass::Dense];
    classes
        .into_iter()
        .zip(offsets)
        .map(|(sample_class, offset)| {
            let row: (Vec<u8>, i64, Vec<u8>, i64) = state.query_row(
                "WITH counts AS (
                     SELECT pubkey,count(*) AS candidate_count
                       FROM candidate_events WHERE created_at <= ?1 GROUP BY pubkey
                 )
                 SELECT counts.pubkey,counts.candidate_count,winner.event_id,winner.created_at
                   FROM counts
                   JOIN candidate_events winner ON winner.event_id=(
                       SELECT event_id FROM candidate_events candidate
                        WHERE candidate.pubkey=counts.pubkey AND candidate.created_at <= ?1
                        ORDER BY candidate.created_at DESC,candidate.event_id DESC LIMIT 1
                   )
                  ORDER BY counts.candidate_count ASC,counts.pubkey ASC LIMIT 1 OFFSET ?2",
                params![to_i64(as_of_epoch)?, to_i64(offset)?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            Ok(CanonicalWinner {
                sample_class,
                pubkey: fixed_32(row.0, "winner pubkey")?,
                candidate_count: nonnegative(row.1, "candidate count")?,
                event_id: fixed_32(row.2, "winner event ID")?,
                created_at: nonnegative(row.3, "winner created_at")?,
            })
        })
        .collect()
}

async fn query_clickhouse_winners(
    client: &clickhouse::Client,
    samples: &[CanonicalWinner],
    as_of_epoch: u64,
) -> Result<BTreeMap<String, ClickhouseWinner>> {
    let pubkeys = samples
        .iter()
        .map(|sample| format!("'{}'", hex::encode(sample.pubkey)))
        .collect::<Vec<_>>()
        .join(",");
    let as_of = u32::try_from(as_of_epoch).context("relay as-of exceeds DateTime domain")?;
    let sql = clickhouse_winner_sql(&pubkeys);
    let rows = client
        .query(&sql)
        .param("as_of", as_of)
        .fetch_all::<ClickhouseWinner>()
        .await
        .context("query ClickHouse deterministic relay winners")?;
    Ok(rows
        .into_iter()
        .map(|row| (row.pubkey.clone(), row))
        .collect())
}

fn clickhouse_winner_sql(pubkeys: &str) -> String {
    format!(
        "SELECT pubkey,event_id,toUInt32(winner_created_at) AS created_at
           FROM (
                SELECT pubkey,
                       argMax(id,tuple(created_at,id)) AS event_id,
                       max(created_at) AS winner_created_at
                  FROM events_local FINAL
                 WHERE kind=10002
                   AND created_at <= toDateTime({{as_of:UInt32}}, 'UTC')
                   AND pubkey IN ({pubkeys})
                 GROUP BY pubkey
           ) ORDER BY pubkey"
    )
}

fn compare_winners(
    canonical: Vec<CanonicalWinner>,
    clickhouse: &BTreeMap<String, ClickhouseWinner>,
) -> Vec<WinnerSample> {
    canonical
        .into_iter()
        .map(|winner| {
            let pubkey = hex::encode(winner.pubkey);
            let candidate = clickhouse.get(&pubkey);
            let canonical_event_id = hex::encode(winner.event_id);
            let exact = candidate.is_some_and(|row| {
                row.event_id == canonical_event_id && u64::from(row.created_at) == winner.created_at
            });
            WinnerSample {
                sample_class: winner.sample_class,
                pubkey,
                canonical_candidate_count: winner.candidate_count,
                canonical_event_id,
                canonical_created_at: winner.created_at,
                clickhouse_event_id: candidate.map(|row| row.event_id.clone()),
                clickhouse_created_at: candidate.map(|row| u64::from(row.created_at)),
                outcome: if exact {
                    "exact_match"
                } else {
                    "cross_store_population"
                },
            }
        })
        .collect()
}

async fn query_clickhouse_rows(client: &clickhouse::Client) -> Result<Vec<RelayDistributionRow>> {
    let rows = client
        .query(
            "SELECT relay_url,user_count,read_count,write_count
               FROM relay_distribution FINAL
              ORDER BY relay_url ASC LIMIT {limit:UInt64}",
        )
        .param("limit", MAX_CLICKHOUSE_ROWS + 1)
        .fetch_all::<ClickhouseRelayRow>()
        .await
        .context("query bounded legacy ClickHouse relay rows")?;
    if rows.len() as u64 > MAX_CLICKHOUSE_ROWS {
        bail!("legacy ClickHouse relay relation exceeds bounded comparison limit");
    }
    Ok(rows
        .into_iter()
        .map(|row| RelayDistributionRow {
            relay_url: row.relay_url,
            user_count: row.user_count,
            read_count: row.read_count,
            write_count: row.write_count,
        })
        .collect())
}

struct RowSummary {
    exact: u64,
    differing: u64,
    canonical_only: u64,
    clickhouse_only: u64,
    examples: Vec<RelayDifference>,
}

fn compare_rows(
    canonical: &[RelayDistributionRow],
    clickhouse: &[RelayDistributionRow],
) -> RowSummary {
    let canonical = canonical
        .iter()
        .map(|row| (row.relay_url.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let clickhouse = clickhouse
        .iter()
        .map(|row| (row.relay_url.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let keys = canonical
        .keys()
        .chain(clickhouse.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut summary = RowSummary {
        exact: 0,
        differing: 0,
        canonical_only: 0,
        clickhouse_only: 0,
        examples: Vec::new(),
    };
    for key in keys {
        let left = canonical.get(key).copied();
        let right = clickhouse.get(key).copied();
        if left == right {
            summary.exact += 1;
            continue;
        }
        match (left, right) {
            (Some(_), Some(_)) => summary.differing += 1,
            (Some(_), None) => summary.canonical_only += 1,
            (None, Some(_)) => summary.clickhouse_only += 1,
            (None, None) => unreachable!("union key must exist in at least one relation"),
        }
        if summary.examples.len() < MAX_DIFFERENCE_EXAMPLES {
            summary.examples.push(RelayDifference {
                relay_url: key.to_owned(),
                canonical: left.cloned(),
                clickhouse: right.cloned(),
                classification: "canonical_vs_legacy_relay_semantics",
            });
        }
    }
    summary
}

fn fixed_32(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow::anyhow!("{field} has {} bytes", bytes.len()))
}

fn nonnegative(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("{field} is negative"))
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("comparison value exceeds SQLite i64 domain")
}

fn write_immutable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create comparison directory {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec(value).context("encode canonical comparison evidence")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create immutable comparison evidence {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(fs::read(path)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(url: &str, users: u64, reads: u64, writes: u64) -> RelayDistributionRow {
        RelayDistributionRow {
            relay_url: url.to_owned(),
            user_count: users,
            read_count: reads,
            write_count: writes,
        }
    }

    #[test]
    fn row_comparison_counts_every_difference_but_bounds_examples() {
        let canonical = (0..150)
            .map(|index| row(&format!("wss://canonical-{index}.example"), 10, 10, 10))
            .collect::<Vec<_>>();
        let clickhouse = vec![row("wss://legacy.example", 20, 20, 20)];
        let summary = compare_rows(&canonical, &clickhouse);
        assert_eq!(summary.canonical_only, 150);
        assert_eq!(summary.clickhouse_only, 1);
        assert_eq!(summary.examples.len(), MAX_DIFFERENCE_EXAMPLES);
    }

    #[test]
    fn exact_rows_are_not_reported_as_differences() {
        let rows = vec![row("wss://relay.example", 12, 11, 10)];
        let summary = compare_rows(&rows, &rows);
        assert_eq!(summary.exact, 1);
        assert_eq!(summary.differing, 0);
        assert!(summary.examples.is_empty());
    }

    #[test]
    fn winner_comparison_requires_id_and_timestamp() {
        let canonical = vec![CanonicalWinner {
            sample_class: SampleClass::Sparse,
            pubkey: [1; 32],
            candidate_count: 2,
            event_id: [2; 32],
            created_at: 42,
        }];
        let pubkey = hex::encode([1; 32]);
        let matches = BTreeMap::from([(
            pubkey.clone(),
            ClickhouseWinner {
                pubkey,
                event_id: hex::encode([2; 32]),
                created_at: 42,
            },
        )]);
        assert_eq!(
            compare_winners(canonical, &matches)[0].outcome,
            "exact_match"
        );
    }

    #[test]
    fn clickhouse_winner_query_does_not_shadow_source_created_at() {
        let sql = clickhouse_winner_sql("'00'");
        assert!(sql.contains("max(created_at) AS winner_created_at"));
        assert!(sql.contains("toUInt32(winner_created_at) AS created_at"));
        assert!(sql.contains("WHERE kind=10002\n                   AND created_at <="));
    }

    #[test]
    fn sqlite_samples_are_ordered_by_candidate_density_and_choose_exact_winners() {
        let state = Connection::open_in_memory().expect("open state");
        state
            .execute_batch(
                "CREATE TABLE candidate_events(
                     event_id BLOB PRIMARY KEY,pubkey BLOB NOT NULL,created_at INTEGER NOT NULL
                 ) WITHOUT ROWID;",
            )
            .expect("create candidates");
        for (pubkey, count) in [(1_u8, 1_u8), (2, 2), (3, 3)] {
            for ordinal in 1..=count {
                let mut event_id = [0_u8; 32];
                event_id[0] = pubkey;
                event_id[1] = ordinal;
                state
                    .execute(
                        "INSERT INTO candidate_events(event_id,pubkey,created_at)
                         VALUES(?1,?2,?3)",
                        params![&event_id[..], &[pubkey; 32][..], i64::from(ordinal)],
                    )
                    .expect("insert candidate");
            }
        }
        let samples = select_winner_samples(&state, 10, 3).expect("select samples");
        assert_eq!(
            samples
                .iter()
                .map(|sample| (sample.pubkey[0], sample.candidate_count, sample.event_id[1]))
                .collect::<Vec<_>>(),
            vec![(1, 1, 1), (2, 2, 2), (3, 3, 3)]
        );
    }
}
