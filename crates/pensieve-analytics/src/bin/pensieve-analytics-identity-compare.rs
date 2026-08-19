//! Reconcile one immutable bounded first-seen candidate with ClickHouse semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use pensieve_analytics::{
    PUBKEY_FIRST_SEEN_BYTES, PUBKEY_FIRST_SEEN_KEY_BYTES, load_bounded_pubkey_first_seen,
    publish_canonical_json,
};
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::Serialize;
use sha2::{Digest, Sha256};

const RUNNER_VERSION: &str = "pensieve-analytics-identity-compare-v1";

#[derive(Debug, Parser)]
#[command(about = "Compare bounded B1 identity metrics with exact ClickHouse first-seen semantics")]
struct Args {
    /// Immutable bounded first-seen evidence.
    #[arg(long)]
    identity_evidence: PathBuf,
    /// Immutable comparison evidence output.
    #[arg(long)]
    output: PathBuf,
    /// ClickHouse HTTP endpoint.
    #[arg(long, env = "CLICKHOUSE_URL", default_value = "http://localhost:8123")]
    clickhouse_url: String,
    /// ClickHouse database containing pubkey_first_seen_data.
    #[arg(long, env = "CLICKHOUSE_DATABASE", default_value = "nostr")]
    clickhouse_database: String,
    /// Optional ClickHouse user.
    #[arg(long, env = "CLICKHOUSE_USER")]
    clickhouse_user: Option<String>,
    /// Optional ClickHouse password; never written to evidence.
    #[arg(long, env = "CLICKHOUSE_PASSWORD")]
    clickhouse_password: Option<String>,
    /// Maximum ClickHouse query memory in bytes.
    #[arg(long, default_value_t = 8_589_934_592)]
    clickhouse_max_memory_usage: u64,
    /// Maximum query execution time in seconds.
    #[arg(long, default_value_t = 21_600)]
    clickhouse_max_execution_time: u64,
    /// Maximum detailed day/shard differences retained in evidence.
    #[arg(long, default_value_t = 100)]
    max_difference_examples: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    snapshot_id: String,
    as_of_epoch: u64,
    candidate: CandidateMetadata,
    clickhouse: ClickhouseMetadata,
    total_pubkeys: ScalarComparison,
    new_users_daily: SeriesComparison,
    input_alignment: InputAlignment,
}

#[derive(Debug, Serialize)]
struct CandidateMetadata {
    evidence_sha256: String,
    artifact_sha256: String,
    first_seen_records: u64,
    eligible_pubkeys: u64,
    daily_rows: usize,
}

#[derive(Debug, Serialize)]
struct ClickhouseMetadata {
    database: String,
    source: &'static str,
    semantics: &'static str,
    query_started_at: DateTime<Utc>,
    query_completed_at: DateTime<Utc>,
    max_threads: u8,
    max_memory_usage: u64,
}

#[derive(Debug, Serialize)]
struct ScalarComparison {
    candidate: u64,
    clickhouse: u64,
    difference: i128,
    equal: bool,
}

#[derive(Debug, Serialize)]
struct SeriesComparison {
    candidate_rows: usize,
    clickhouse_rows: usize,
    candidate_sha256: String,
    clickhouse_sha256: String,
    differing_keys: u64,
    candidate_only_keys: u64,
    clickhouse_only_keys: u64,
    examples: Vec<Difference>,
    equal: bool,
}

#[derive(Debug, Serialize)]
struct Difference {
    key: String,
    candidate: Option<u64>,
    clickhouse: Option<u64>,
}

#[derive(Debug, Serialize)]
struct InputAlignment {
    status: &'static str,
    note: &'static str,
}

#[derive(Debug, serde::Deserialize, Row)]
struct ClickhouseDailyRow {
    shard: String,
    day: String,
    new_pubkeys: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => {
            eprintln!("identity comparison failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<bool> {
    let args = Args::parse();
    let identity = load_bounded_pubkey_first_seen(&args.identity_evidence)
        .context("load immutable identity evidence")?;
    let candidate = read_candidate_state(
        Path::new(&identity.evidence.final_artifact.path),
        identity.evidence.as_of_epoch,
    )?;
    if candidate.total != identity.evidence.eligible_pubkeys {
        bail!("candidate comparison count does not match identity evidence");
    }
    let expected_daily = identity
        .evidence
        .new_users_daily
        .iter()
        .map(|row| (row.day.clone(), row.new_pubkeys))
        .collect::<BTreeMap<_, _>>();
    let candidate_daily = collapse_shards(&candidate.daily)?;
    if candidate_daily != expected_daily {
        bail!("candidate comparison daily rows do not match identity evidence");
    }

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
        .with_option("optimize_aggregation_in_order", "1")
        .with_option(
            "max_bytes_before_external_group_by",
            (args.clickhouse_max_memory_usage / 2).to_string(),
        );
    if let Some(user) = args.clickhouse_user.as_deref() {
        client = client.with_user(user);
    }
    if let Some(password) = args.clickhouse_password.as_deref() {
        client = client.with_password(password);
    }
    let as_of = u32::try_from(identity.evidence.as_of_epoch)
        .context("identity as-of exceeds ClickHouse DateTime domain")?;
    let query_started_at = Utc::now();
    let rows = client
        .query(
            "
            SELECT substring(pubkey, 1, 2) AS shard,
                   toString(toDate(first_seen, 'UTC')) AS day,
                   count() AS new_pubkeys
            FROM (
                SELECT pubkey, minMerge(first_seen_state) AS first_seen
                FROM pubkey_first_seen_data
                GROUP BY pubkey
            )
            WHERE first_seen >= toDateTime(?,'UTC')
              AND first_seen <= toDateTime(?,'UTC')
            GROUP BY shard, day
            ORDER BY shard, day
            ",
        )
        .bind(NOSTR_GENESIS_TIMESTAMP)
        .bind(as_of)
        .fetch_all::<ClickhouseDailyRow>()
        .await
        .context("query exact ClickHouse first-seen daily state")?;
    let query_completed_at = Utc::now();
    let mut clickhouse_daily = BTreeMap::new();
    let mut clickhouse_total = 0_u64;
    for row in rows {
        if row.shard.len() != 2 || hex::decode(&row.shard).is_err() {
            bail!("ClickHouse returned a non-canonical pubkey shard");
        }
        let key = format!("{}|{}", row.shard.to_lowercase(), row.day);
        if clickhouse_daily.insert(key, row.new_pubkeys).is_some() {
            bail!("ClickHouse returned a duplicate shard/day row");
        }
        clickhouse_total = clickhouse_total
            .checked_add(row.new_pubkeys)
            .context("ClickHouse eligible pubkey total overflow")?;
    }

    let total_equal = candidate.total == clickhouse_total;
    let series = compare_series(
        &candidate.daily,
        &clickhouse_daily,
        args.max_difference_examples,
    )?;
    let equal = total_equal && series.equal;
    let report = Report {
        schema_version: 1,
        runner_version: RUNNER_VERSION,
        status: if equal { "passed" } else { "different" },
        generated_at: Utc::now(),
        snapshot_id: identity.evidence.snapshot_id.clone(),
        as_of_epoch: identity.evidence.as_of_epoch,
        candidate: CandidateMetadata {
            evidence_sha256: identity.evidence_sha256,
            artifact_sha256: identity.evidence.final_artifact.sha256,
            first_seen_records: identity.evidence.first_seen_records,
            eligible_pubkeys: identity.evidence.eligible_pubkeys,
            daily_rows: identity.evidence.new_users_daily.len(),
        },
        clickhouse: ClickhouseMetadata {
            database: args.clickhouse_database,
            source: "pubkey_first_seen_data",
            semantics: "minMerge(first_seen_state) per pubkey, kinds 445 and 1059 excluded by the owning materialized view, filtered to the candidate genesis/as-of domain",
            query_started_at,
            query_completed_at,
            max_threads: 1,
            max_memory_usage: args.clickhouse_max_memory_usage,
        },
        total_pubkeys: ScalarComparison {
            candidate: candidate.total,
            clickhouse: clickhouse_total,
            difference: i128::from(clickhouse_total) - i128::from(candidate.total),
            equal: total_equal,
        },
        new_users_daily: series,
        input_alignment: InputAlignment {
            status: "unproven_continuous_clickhouse_head",
            note: "The candidate is one immutable Parquet snapshot. ClickHouse parts are fixed at query start but may include events indexed after that catalog snapshot; any difference requires head-lag attribution or exact event-ID alignment before publication.",
        },
    };
    publish_canonical_json(&args.output, &report).context("publish comparison evidence")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(equal)
}

struct CandidateState {
    total: u64,
    daily: BTreeMap<String, u64>,
}

fn read_candidate_state(path: &Path, as_of: u64) -> Result<CandidateState> {
    let mut reader = BufReader::new(File::open(path).context("open first-seen artifact")?);
    let mut record = [0_u8; PUBKEY_FIRST_SEEN_BYTES];
    let mut total = 0_u64;
    let mut daily = BTreeMap::<String, u64>::new();
    loop {
        let mut offset = 0;
        while offset < record.len() {
            let read = reader.read(&mut record[offset..])?;
            if read == 0 {
                break;
            }
            offset += read;
        }
        if offset == 0 {
            break;
        }
        if offset != record.len() {
            bail!("truncated first-seen comparison record");
        }
        let first_seen = u64::from_be_bytes(
            record[PUBKEY_FIRST_SEEN_KEY_BYTES..]
                .try_into()
                .expect("fixed record timestamp"),
        );
        if first_seen < u64::from(NOSTR_GENESIS_TIMESTAMP) || first_seen > as_of {
            continue;
        }
        let day = DateTime::<Utc>::from_timestamp(i64::try_from(first_seen / 86_400)? * 86_400, 0)
            .context("invalid first-seen UTC day")?
            .date_naive();
        let key = format!("{:02x}|{day}", record[0]);
        let count = daily.entry(key).or_default();
        *count = count
            .checked_add(1)
            .context("candidate shard/day count overflow")?;
        total = total.checked_add(1).context("candidate total overflow")?;
    }
    Ok(CandidateState { total, daily })
}

fn collapse_shards(values: &BTreeMap<String, u64>) -> Result<BTreeMap<String, u64>> {
    let mut collapsed = BTreeMap::<String, u64>::new();
    for (key, value) in values {
        let (_, day) = key
            .split_once('|')
            .context("candidate comparison key lacks shard separator")?;
        let current = collapsed.get(day).copied().unwrap_or(0);
        collapsed.insert(
            day.to_owned(),
            current
                .checked_add(*value)
                .context("daily total overflow")?,
        );
    }
    Ok(collapsed)
}

fn compare_series(
    candidate: &BTreeMap<String, u64>,
    clickhouse: &BTreeMap<String, u64>,
    max_examples: usize,
) -> Result<SeriesComparison> {
    let mut differing_keys = 0_u64;
    let mut candidate_only_keys = 0_u64;
    let mut clickhouse_only_keys = 0_u64;
    let mut examples = Vec::new();
    let keys = candidate
        .keys()
        .chain(clickhouse.keys())
        .collect::<BTreeSet<_>>();
    for key in keys {
        let left = candidate.get(key).copied();
        let right = clickhouse.get(key).copied();
        if left == right {
            continue;
        }
        differing_keys = differing_keys
            .checked_add(1)
            .context("difference overflow")?;
        if left.is_some() && right.is_none() {
            candidate_only_keys += 1;
        } else if left.is_none() && right.is_some() {
            clickhouse_only_keys += 1;
        }
        if examples.len() < max_examples {
            examples.push(Difference {
                key: key.to_string(),
                candidate: left,
                clickhouse: right,
            });
        }
    }
    Ok(SeriesComparison {
        candidate_rows: candidate.len(),
        clickhouse_rows: clickhouse.len(),
        candidate_sha256: stable_map_sha256(candidate),
        clickhouse_sha256: stable_map_sha256(clickhouse),
        differing_keys,
        candidate_only_keys,
        clickhouse_only_keys,
        examples,
        equal: candidate == clickhouse,
    })
}

fn stable_map_sha256(values: &BTreeMap<String, u64>) -> String {
    let mut hash = Sha256::new();
    for (key, value) in values {
        hash.update(key.as_bytes());
        hash.update([0]);
        hash.update(value.to_string().as_bytes());
        hash.update(b"\n");
    }
    hex::encode(hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_comparison_is_exact_and_stable() {
        let candidate = BTreeMap::from([
            ("00|2026-01-01".to_owned(), 2),
            ("01|2026-01-01".to_owned(), 3),
        ]);
        let clickhouse = BTreeMap::from([
            ("00|2026-01-01".to_owned(), 2),
            ("01|2026-01-01".to_owned(), 4),
            ("ff|2026-01-02".to_owned(), 1),
        ]);
        let comparison = compare_series(&candidate, &clickhouse, 10).expect("compare");
        assert!(!comparison.equal);
        assert_eq!(comparison.differing_keys, 2);
        assert_eq!(comparison.clickhouse_only_keys, 1);
        assert_eq!(comparison.examples.len(), 2);
        assert_ne!(comparison.candidate_sha256, comparison.clickhouse_sha256);
    }

    #[test]
    fn collapsing_shards_preserves_exact_daily_totals() {
        let values = BTreeMap::from([
            ("00|2026-01-01".to_owned(), 2),
            ("ff|2026-01-01".to_owned(), 3),
            ("01|2026-01-02".to_owned(), 4),
        ]);
        assert_eq!(
            collapse_shards(&values).expect("collapse"),
            BTreeMap::from([("2026-01-01".to_owned(), 5), ("2026-01-02".to_owned(), 4),])
        );
    }
}
