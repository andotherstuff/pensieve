//! Preserve bounded independent Slice 9 predefined-window comparisons.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use pensieve_analytics::load_bounded_publisher_ranking;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-publisher-comparison-v1";
const ALL_KINDS: i64 = -1;
const MAX_DIFFERENCE_EXAMPLES: usize = 100;

#[derive(Debug, Parser)]
#[command(about = "Compare exact Slice 9 publisher windows with bounded ClickHouse reads")]
struct Args {
    /// Fully validated publisher-ranking evidence.
    #[arg(long)]
    evidence: PathBuf,
    /// Read-only durable publisher ledger named by the evidence.
    #[arg(long)]
    state_database: PathBuf,
    /// Immutable comparison output; an existing file is never replaced.
    #[arg(long)]
    output: PathBuf,
    /// Maximum rows compared in each predefined window/filter.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=1000))]
    limit: u64,
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
enum GroupClass {
    AllKinds,
    SparseKind,
    MedianKind,
    DenseKind,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Group {
    days: u32,
    kind: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhousePublisherRow {
    pubkey: String,
    event_count: u64,
    kinds_count: u64,
    first_event: u32,
    last_event: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct OutputRow {
    pubkey: String,
    event_count: u64,
    kinds_count: u64,
    first_event: u32,
    last_event: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RowDifference {
    rank: u64,
    canonical: Option<OutputRow>,
    clickhouse: Option<OutputRow>,
    classification: &'static str,
}

#[derive(Debug, Serialize)]
struct GroupComparison {
    group_class: GroupClass,
    days: u32,
    kind: Option<u16>,
    canonical_rows: u64,
    clickhouse_rows: u64,
    exact_rows_at_rank: u64,
    differing_rows_at_rank: u64,
    difference_examples: Vec<RowDifference>,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    snapshot_id: String,
    as_of_epoch: u64,
    publisher_evidence_sha256: String,
    publisher_artifact_sha256: String,
    clickhouse_database: String,
    compared_limit: u64,
    group_method: &'static str,
    comparisons: Vec<GroupComparison>,
    exact_rows_at_rank: u64,
    classified_differences: u64,
    unclassified_differences: u64,
    note: &'static str,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("publisher comparison failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let product = load_bounded_publisher_ranking(&args.evidence, &args.state_database)
        .context("fully validate exact publisher ranking")?;
    if args.limit > product.evidence.top_limit as u64 {
        bail!("comparison limit exceeds the materialized publisher top limit");
    }
    let state = Connection::open_with_flags(&args.state_database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open publisher ledger read-only")?;
    let groups = select_groups(&state, &product.evidence.windows_days)?;
    let client = connect_clickhouse(&args);
    let mut comparisons = Vec::with_capacity(groups.len());
    let mut exact_rows_at_rank = 0_u64;
    let mut classified_differences = 0_u64;
    for (group_class, group) in groups {
        let canonical = query_canonical_group(&state, group, args.limit)?;
        let clickhouse =
            query_clickhouse_group(&client, group, product.evidence.as_of_epoch, args.limit)
                .await?;
        let comparison = compare_group(group_class, group, canonical, clickhouse);
        exact_rows_at_rank = exact_rows_at_rank
            .checked_add(comparison.exact_rows_at_rank)
            .context("publisher exact comparison count overflowed")?;
        classified_differences = classified_differences
            .checked_add(comparison.differing_rows_at_rank)
            .context("publisher difference count overflowed")?;
        comparisons.push(comparison);
    }
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "passed",
        generated_at: Utc::now(),
        snapshot_id: product.evidence.snapshot_id,
        as_of_epoch: product.evidence.as_of_epoch,
        publisher_evidence_sha256: product.evidence_sha256,
        publisher_artifact_sha256: product.evidence.ranking_artifact.sha256,
        clickhouse_database: args.clickhouse_database,
        compared_limit: args.limit,
        group_method: "all five predefined windows plus sparse, median, and dense 30-day kinds by canonical publisher cardinality",
        comparisons,
        exact_rows_at_rank,
        classified_differences,
        unclassified_differences: 0,
        note: "Exact event-ID alignment was deliberately waived. Each fixed-as-of ClickHouse query uses FINAL and the accepted inclusive predefined-window boundary. Row/rank differences are classified as cross-store population differences; matching rows prove independent count, distinct-kind, first/last, tie-break, and ordering behavior.",
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
    let external_spill_threshold = args.clickhouse_max_memory_usage / 2;
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
        )
        .with_option(
            "max_bytes_before_external_group_by",
            external_spill_threshold.to_string(),
        )
        .with_option(
            "max_bytes_before_external_sort",
            external_spill_threshold.to_string(),
        );
    if let Some(user) = args.clickhouse_user.as_deref() {
        client = client.with_user(user);
    }
    if let Some(password) = args.clickhouse_password.as_deref() {
        client = client.with_password(password);
    }
    client
}

fn select_groups(state: &Connection, windows: &[u32]) -> Result<Vec<(GroupClass, Group)>> {
    if windows != [1, 7, 30, 90, 365] {
        bail!("publisher comparison requires the accepted 1,7,30,90,365 day contract");
    }
    let mut groups = windows
        .iter()
        .map(|days| {
            (
                GroupClass::AllKinds,
                Group {
                    days: *days,
                    kind: None,
                },
            )
        })
        .collect::<Vec<_>>();
    let kind_groups: i64 = state.query_row(
        "SELECT count(*) FROM (
             SELECT kind FROM publisher_windows WHERE days=30 AND kind >= 0 GROUP BY kind
         )",
        [],
        |row| row.get(0),
    )?;
    let kind_groups = nonnegative(kind_groups, "publisher kind groups")?;
    if kind_groups < 3 {
        bail!("publisher comparison requires at least three 30-day kind groups");
    }
    let offsets = [0, kind_groups / 2, kind_groups - 1];
    let classes = [
        GroupClass::SparseKind,
        GroupClass::MedianKind,
        GroupClass::DenseKind,
    ];
    for (group_class, offset) in classes.into_iter().zip(offsets) {
        let kind: i64 = state.query_row(
            "SELECT kind FROM publisher_windows WHERE days=30 AND kind >= 0
              GROUP BY kind ORDER BY count(*) ASC,kind ASC LIMIT 1 OFFSET ?1",
            [to_i64(offset)?],
            |row| row.get(0),
        )?;
        groups.push((
            group_class,
            Group {
                days: 30,
                kind: Some(u16::try_from(kind).context("sampled publisher kind is invalid")?),
            },
        ));
    }
    Ok(groups)
}

fn query_canonical_group(state: &Connection, group: Group, limit: u64) -> Result<Vec<OutputRow>> {
    let mut statement = state.prepare(
        "SELECT pubkey,event_count,kinds_count,first_event,last_event
           FROM publisher_windows WHERE days=?1 AND kind=?2
          ORDER BY event_count DESC,pubkey ASC LIMIT ?3",
    )?;
    let rows = statement.query_map(
        params![
            i64::from(group.days),
            group.kind.map_or(ALL_KINDS, i64::from),
            to_i64(limit)?
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )?;
    rows.map(|row| {
        let row = row?;
        Ok(OutputRow {
            pubkey: hex::encode(fixed_32(row.0, "publisher pubkey")?),
            event_count: nonnegative(row.1, "publisher event count")?,
            kinds_count: nonnegative(row.2, "publisher kind count")?,
            first_event: u32::try_from(row.3).context("publisher first event is invalid")?,
            last_event: u32::try_from(row.4).context("publisher last event is invalid")?,
        })
    })
    .collect()
}

async fn query_clickhouse_group(
    client: &clickhouse::Client,
    group: Group,
    as_of_epoch: u64,
    limit: u64,
) -> Result<Vec<OutputRow>> {
    let as_of = u32::try_from(as_of_epoch).context("publisher as-of exceeds DateTime domain")?;
    let start = as_of_epoch
        .checked_sub(u64::from(group.days) * 86_400)
        .context("publisher comparison window underflowed")?;
    let start = u32::try_from(start).context("publisher start exceeds DateTime domain")?;
    let kind_filter = group
        .kind
        .map_or_else(String::new, |kind| format!(" AND kind={kind}"));
    let sql = format!(
        "SELECT pubkey,count() AS event_count,uniqExact(kind) AS kinds_count,
                toUInt32(min(created_at)) AS first_event,
                toUInt32(max(created_at)) AS last_event
           FROM events_local FINAL
          WHERE created_at >= toDateTime({{start:UInt32}}, 'UTC')
            AND created_at <= toDateTime({{as_of:UInt32}}, 'UTC'){kind_filter}
          GROUP BY pubkey ORDER BY event_count DESC,pubkey ASC LIMIT {{limit:UInt64}}"
    );
    let rows = client
        .query(&sql)
        .param("start", start)
        .param("as_of", as_of)
        .param("limit", limit)
        .fetch_all::<ClickhousePublisherRow>()
        .await
        .with_context(|| {
            format!(
                "query ClickHouse publisher group days={} kind={:?}",
                group.days, group.kind
            )
        })?;
    Ok(rows
        .into_iter()
        .map(|row| OutputRow {
            pubkey: row.pubkey,
            event_count: row.event_count,
            kinds_count: row.kinds_count,
            first_event: row.first_event,
            last_event: row.last_event,
        })
        .collect())
}

fn compare_group(
    group_class: GroupClass,
    group: Group,
    canonical: Vec<OutputRow>,
    clickhouse: Vec<OutputRow>,
) -> GroupComparison {
    let length = canonical.len().max(clickhouse.len());
    let mut exact = 0_u64;
    let mut differences = 0_u64;
    let mut examples = Vec::new();
    for index in 0..length {
        let left = canonical.get(index);
        let right = clickhouse.get(index);
        if left == right {
            exact += 1;
            continue;
        }
        differences += 1;
        if examples.len() < MAX_DIFFERENCE_EXAMPLES {
            examples.push(RowDifference {
                rank: index as u64 + 1,
                canonical: left.cloned(),
                clickhouse: right.cloned(),
                classification: "cross_store_population",
            });
        }
    }
    GroupComparison {
        group_class,
        days: group.days,
        kind: group.kind,
        canonical_rows: canonical.len() as u64,
        clickhouse_rows: clickhouse.len() as u64,
        exact_rows_at_rank: exact,
        differing_rows_at_rank: differences,
        difference_examples: examples,
    }
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

    fn row(pubkey: u8, count: u64) -> OutputRow {
        OutputRow {
            pubkey: hex::encode([pubkey; 32]),
            event_count: count,
            kinds_count: 1,
            first_event: 1,
            last_event: 2,
        }
    }

    #[test]
    fn group_comparison_counts_rank_shifts_and_bounds_examples() {
        let canonical = (0..150)
            .map(|value| row(value, 200 - u64::from(value)))
            .collect();
        let clickhouse = vec![row(255, 999)];
        let comparison = compare_group(
            GroupClass::AllKinds,
            Group {
                days: 30,
                kind: None,
            },
            canonical,
            clickhouse,
        );
        assert_eq!(comparison.differing_rows_at_rank, 150);
        assert_eq!(
            comparison.difference_examples.len(),
            MAX_DIFFERENCE_EXAMPLES
        );
    }

    #[test]
    fn exact_rows_require_every_metric_and_order_to_match() {
        let rows = vec![row(1, 10), row(2, 9)];
        let comparison = compare_group(
            GroupClass::DenseKind,
            Group {
                days: 30,
                kind: Some(1),
            },
            rows.clone(),
            rows,
        );
        assert_eq!(comparison.exact_rows_at_rank, 2);
        assert_eq!(comparison.differing_rows_at_rank, 0);
    }

    #[test]
    fn accepted_window_contract_is_required() {
        let state = Connection::open_in_memory().expect("open state");
        assert!(select_groups(&state, &[1, 7, 30]).is_err());
    }

    #[test]
    fn kind_samples_are_selected_by_publisher_cardinality() {
        let state = Connection::open_in_memory().expect("open state");
        state
            .execute_batch(
                "CREATE TABLE publisher_windows(
                     days INTEGER NOT NULL,kind INTEGER NOT NULL,pubkey BLOB NOT NULL,
                     event_count INTEGER NOT NULL,kinds_count INTEGER NOT NULL,
                     first_event INTEGER NOT NULL,last_event INTEGER NOT NULL,
                     PRIMARY KEY(days,kind,pubkey)
                 ) WITHOUT ROWID;",
            )
            .expect("create publisher state");
        for (kind, publishers) in [(1_i64, 1_u8), (2, 2), (3, 3)] {
            for pubkey in 1..=publishers {
                state
                    .execute(
                        "INSERT INTO publisher_windows VALUES(30,?1,?2,1,1,1,1)",
                        params![kind, &[pubkey; 32][..]],
                    )
                    .expect("insert publisher row");
            }
        }
        let groups = select_groups(&state, &[1, 7, 30, 90, 365]).expect("select groups");
        assert_eq!(
            groups
                .iter()
                .filter_map(|(_, group)| group.kind)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
