//! Compare the current Postgres Slice A publication with ClickHouse at one fixed boundary.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Days, NaiveDate, Utc};
use clap::Parser;
use clickhouse::Row;
use pensieve_analytics::{
    ComparisonGate, InputAlignment, MetricComparison, ReconciliationSummary, SeriesComparison,
    compare_metric, compare_series,
};
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use postgres::{Client as PostgresClient, Config as PostgresConfig, IsolationLevel, NoTls};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPORT_SCHEMA_VERSION: u32 = 1;
const HARNESS_VERSION: &str = "slice-a-compare-v1";
const DEFAULT_CLICKHOUSE_MEMORY: u64 = 16 * 1024 * 1024 * 1024;
const SEVEN_DAYS_SECONDS: u64 = 7 * 24 * 60 * 60;
const THIRTY_DAYS_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Parser)]
#[command(about = "Diff current Postgres Slice A metrics against ClickHouse FINAL")]
struct Args {
    /// Postgres connection string for the shadow analytics serving store.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: String,
    /// Postgres password supplied separately from the connection string.
    #[arg(long, env = "POSTGRES_ANALYTICS_PASSWORD")]
    postgres_password: Option<String>,
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
    /// Immutable output path. The command refuses to replace existing evidence.
    #[arg(long)]
    output: PathBuf,
    /// Number of complete UTC days to compare at daily and daily-kind grain.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=365))]
    completed_days: u64,
    /// Maximum detailed differences retained per keyed relation.
    #[arg(long, default_value_t = 100)]
    max_difference_examples: usize,
    /// Maximum ClickHouse worker threads used by each read-only query.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=16))]
    clickhouse_max_threads: u64,
    /// Maximum ClickHouse memory per query in bytes.
    #[arg(long, default_value_t = DEFAULT_CLICKHOUSE_MEMORY)]
    clickhouse_max_memory_usage: u64,
    /// Maximum ClickHouse query execution time in seconds.
    #[arg(long, default_value_t = 21_600)]
    clickhouse_max_execution_time: u64,
    /// Assert that an independent evidence file proves exact input-set alignment.
    #[arg(long, requires = "alignment_evidence")]
    input_alignment_proven: bool,
    /// JSON proof with status=passed and the current Postgres snapshot_id.
    #[arg(long, requires = "input_alignment_proven")]
    alignment_evidence: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    harness_version: &'static str,
    generated_at: DateTime<Utc>,
    input_alignment: AlignmentReport,
    postgres_run: RunMetadata,
    clickhouse: ClickhouseMetadata,
    scope: Scope,
    metrics: Vec<MetricComparison>,
    series: Vec<SeriesComparison>,
    summary: ReconciliationSummary,
}

#[derive(Debug, Serialize)]
struct AlignmentReport {
    status: InputAlignment,
    evidence_file: Option<String>,
    evidence_sha256: Option<String>,
    clickhouse_indexed_at_max_epoch: Option<u64>,
    note: &'static str,
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
    id_keyed_equal: bool,
}

#[derive(Clone, Debug, Serialize)]
struct RunMetadata {
    run_id: String,
    snapshot_id: String,
    previous_run_id: Option<String>,
    run_kind: String,
    query_version: String,
    code_version: String,
    as_of_epoch: u64,
    published_at: DateTime<Utc>,
    physical_rows: u64,
    logical_events: u64,
    duplicate_rows: u64,
    api_representable_events: u64,
}

#[derive(Debug, Serialize)]
struct ClickhouseMetadata {
    database: String,
    table: &'static str,
    deduplication: &'static str,
    max_threads: u64,
    max_memory_usage: u64,
    max_execution_time: u64,
}

#[derive(Debug, Serialize)]
struct Scope {
    fixed_as_of_epoch: u64,
    clickhouse_indexed_at_max_epoch: Option<u64>,
    completed_days: u64,
    completed_day_start: NaiveDate,
    completed_day_end_exclusive: NaiveDate,
    note: &'static str,
}

struct PostgresSnapshot {
    run: RunMetadata,
    overview: PostgresOverview,
    daily: BTreeMap<String, u64>,
    daily_kind: BTreeMap<String, u64>,
    completed_kind: BTreeMap<String, u64>,
}

struct PostgresOverview {
    api_representable_events: u64,
    earliest_event: u64,
    latest_event: u64,
    events_7d: u64,
    kinds_30d: u64,
}

struct ClickhouseSnapshot {
    overview: ClickhouseOverview,
    daily: BTreeMap<String, u64>,
    daily_kind: BTreeMap<String, u64>,
    completed_kind: BTreeMap<String, u64>,
}

struct ClickhouseOverview {
    api_representable_events: u64,
    earliest_event: u64,
    latest_event: u64,
    events_7d: u64,
    kinds_30d: u64,
}

#[derive(Debug, Deserialize, Row)]
struct ClickhouseAggregateRow {
    day: String,
    kind: u16,
    event_count: u64,
    earliest_event: u32,
    latest_event: u32,
    events_7d: u64,
    events_30d: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(ComparisonGate::Passed) => ExitCode::SUCCESS,
        Ok(ComparisonGate::Incomplete | ComparisonGate::Failed) => ExitCode::from(2),
        Err(error) => {
            eprintln!("analytics comparison failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ComparisonGate> {
    let args = Args::parse();
    validate_args(&args)?;

    let mut postgres = connect_postgres(&args)?;
    let run = load_current_run(&mut postgres)?;
    let (completed_start, completed_end) =
        completed_day_range(run.as_of_epoch, args.completed_days)?;
    let postgres_snapshot = load_postgres_snapshot(postgres, run, completed_start, completed_end)?;
    let alignment = load_alignment(&args, &postgres_snapshot.run.snapshot_id)?;

    let clickhouse_client = connect_clickhouse(&args);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create ClickHouse query runtime")?;
    let clickhouse_snapshot = runtime.block_on(load_clickhouse_snapshot(
        &clickhouse_client,
        postgres_snapshot.run.as_of_epoch,
        alignment.clickhouse_indexed_at_max_epoch,
        completed_start,
        completed_end,
    ))?;

    let metrics = compare_overview(
        &postgres_snapshot.overview,
        &clickhouse_snapshot.overview,
        alignment.status,
    );
    let scope_label = format!("complete UTC days [{completed_start}, {completed_end})");
    let series = vec![
        compare_series(
            "event_daily",
            endpoints(&["/api/v1/stats/events"]),
            &scope_label,
            &postgres_snapshot.daily,
            &clickhouse_snapshot.daily,
            alignment.status,
            args.max_difference_examples,
        ),
        compare_series(
            "event_daily_kind",
            endpoints(&["/api/v1/stats/events", "/api/v1/kinds/{kind}/activity"]),
            &scope_label,
            &postgres_snapshot.daily_kind,
            &clickhouse_snapshot.daily_kind,
            alignment.status,
            args.max_difference_examples,
        ),
        compare_series(
            "kind_completed_day_totals",
            endpoints(&["/api/v1/kinds", "/api/v1/kinds/{kind}"]),
            format!("all complete UTC days before {completed_end}"),
            &postgres_snapshot.completed_kind,
            &clickhouse_snapshot.completed_kind,
            alignment.status,
            args.max_difference_examples,
        ),
    ];
    let summary = ReconciliationSummary::new(alignment.status, &metrics, &series);
    let gate = summary.gate;
    let fixed_as_of_epoch = postgres_snapshot.run.as_of_epoch;
    let clickhouse_indexed_at_max_epoch = alignment.clickhouse_indexed_at_max_epoch;
    let report = Report {
        schema_version: REPORT_SCHEMA_VERSION,
        harness_version: HARNESS_VERSION,
        generated_at: Utc::now(),
        input_alignment: alignment,
        postgres_run: postgres_snapshot.run,
        clickhouse: ClickhouseMetadata {
            database: args.clickhouse_database,
            table: "events_local",
            deduplication: "ReplacingMergeTree FINAL by event id",
            max_threads: args.clickhouse_max_threads,
            max_memory_usage: args.clickhouse_max_memory_usage,
            max_execution_time: args.clickhouse_max_execution_time,
        },
        scope: Scope {
            fixed_as_of_epoch,
            clickhouse_indexed_at_max_epoch,
            completed_days: args.completed_days,
            completed_day_start: completed_start,
            completed_day_end_exclusive: completed_end,
            note: "Daily relations exclude the partial as-of UTC day; rolling metrics use the exact run as_of.",
        },
        metrics,
        series,
        summary,
    };
    write_report(&args.output, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(gate)
}

fn validate_args(args: &Args) -> Result<()> {
    if args.max_difference_examples == 0 {
        bail!("--max-difference-examples must be positive");
    }
    if args.clickhouse_max_memory_usage == 0 {
        bail!("--clickhouse-max-memory-usage must be positive");
    }
    if args.clickhouse_max_execution_time == 0 {
        bail!("--clickhouse-max-execution-time must be positive");
    }
    if args.output.exists() {
        bail!(
            "refusing to replace existing evidence: {}",
            args.output.display()
        );
    }
    Ok(())
}

fn connect_postgres(args: &Args) -> Result<PostgresClient> {
    let mut config: PostgresConfig = args
        .postgres_url
        .parse()
        .context("parse Postgres connection")?;
    if let Some(password) = args.postgres_password.as_deref() {
        config.password(password);
    }
    config
        .connect(NoTls)
        .context("connect to Postgres without TLS")
}

fn load_current_run(client: &mut PostgresClient) -> Result<RunMetadata> {
    let row = client
        .query_one(
            "
            SELECT run_id, snapshot_id, previous_run_id, run_kind,
                   query_version, code_version, as_of_epoch, published_at,
                   physical_rows, logical_events, duplicate_rows,
                   api_representable_events
            FROM pensieve_analytics.current_run_metadata
            ",
            &[],
        )
        .context("load current Postgres analytics run")?;
    Ok(RunMetadata {
        run_id: row.get(0),
        snapshot_id: row.get(1),
        previous_run_id: row.get(2),
        run_kind: row.get(3),
        query_version: row.get(4),
        code_version: row.get(5),
        as_of_epoch: nonnegative("as_of_epoch", row.get(6))?,
        published_at: row.get(7),
        physical_rows: nonnegative("physical_rows", row.get(8))?,
        logical_events: nonnegative("logical_events", row.get(9))?,
        duplicate_rows: nonnegative("duplicate_rows", row.get(10))?,
        api_representable_events: nonnegative("api_representable_events", row.get(11))?,
    })
}

fn load_postgres_snapshot(
    mut client: PostgresClient,
    run: RunMetadata,
    completed_start: NaiveDate,
    completed_end: NaiveDate,
) -> Result<PostgresSnapshot> {
    let mut transaction = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .context("start read-only Postgres snapshot")?;
    let overview_row = transaction
        .query_one(
            "
            SELECT api_representable_events, earliest_event, latest_event,
                   events_7d, kinds_30d
            FROM pensieve_analytics.overview
            WHERE run_id = $1
            ",
            &[&run.run_id],
        )
        .context("load Postgres overview")?;
    let overview = PostgresOverview {
        api_representable_events: nonnegative("api_representable_events", overview_row.get(0))?,
        earliest_event: nonnegative("earliest_event", overview_row.get(1))?,
        latest_event: nonnegative("latest_event", overview_row.get(2))?,
        events_7d: nonnegative("events_7d", overview_row.get(3))?,
        kinds_30d: nonnegative("kinds_30d", overview_row.get(4))?,
    };
    let daily = transaction
        .query(
            "
            SELECT day::text, event_count
            FROM pensieve_analytics.event_daily
            WHERE run_id = $1 AND day >= $2 AND day < $3
            ORDER BY day
            ",
            &[&run.run_id, &completed_start, &completed_end],
        )
        .context("load Postgres daily rows")?
        .into_iter()
        .map(|row| {
            Ok((
                row.get(0),
                nonnegative("event_daily.event_count", row.get(1))?,
            ))
        })
        .collect::<Result<_>>()?;
    let daily_kind = transaction
        .query(
            "
            SELECT day::text, kind, event_count
            FROM pensieve_analytics.event_daily_kind
            WHERE run_id = $1 AND day >= $2 AND day < $3
            ORDER BY day, kind
            ",
            &[&run.run_id, &completed_start, &completed_end],
        )
        .context("load Postgres daily-kind rows")?
        .into_iter()
        .map(|row| {
            let day: String = row.get(0);
            let kind: i32 = row.get(1);
            let value = nonnegative("event_daily_kind.event_count", row.get(2))?;
            Ok((format!("{day}|{kind}"), value))
        })
        .collect::<Result<_>>()?;
    let completed_kind = transaction
        .query(
            "
            SELECT kind, sum(event_count)::bigint
            FROM pensieve_analytics.event_daily_kind
            WHERE run_id = $1 AND day < $2
            GROUP BY kind
            ORDER BY kind
            ",
            &[&run.run_id, &completed_end],
        )
        .context("load Postgres completed-day kind totals")?
        .into_iter()
        .map(|row| {
            let kind: i32 = row.get(0);
            let value = nonnegative("completed kind event_count", row.get(1))?;
            Ok((kind.to_string(), value))
        })
        .collect::<Result<_>>()?;
    transaction.commit().context("finish Postgres snapshot")?;
    Ok(PostgresSnapshot {
        run,
        overview,
        daily,
        daily_kind,
        completed_kind,
    })
}

fn connect_clickhouse(args: &Args) -> clickhouse::Client {
    let mut client = clickhouse::Client::default()
        .with_url(&args.clickhouse_url)
        .with_database(&args.clickhouse_database)
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

async fn load_clickhouse_snapshot(
    client: &clickhouse::Client,
    as_of: u64,
    indexed_at_max: Option<u64>,
    completed_start: NaiveDate,
    completed_end: NaiveDate,
) -> Result<ClickhouseSnapshot> {
    let as_of = u32::try_from(as_of).context("as_of exceeds ClickHouse DateTime domain")?;
    let indexed_at_max = indexed_at_max
        .map(u32::try_from)
        .transpose()
        .context("alignment indexed_at barrier exceeds ClickHouse DateTime domain")?
        .unwrap_or(u32::MAX);
    let seven_day_start = as_of.saturating_sub(SEVEN_DAYS_SECONDS as u32);
    let thirty_day_start = as_of.saturating_sub(THIRTY_DAYS_SECONDS as u32);

    let rows = client
        .query(
            "
            SELECT
                toString(toDate(created_at, 'UTC')) AS day,
                kind,
                count() AS event_count,
                toUInt32(min(created_at)) AS earliest_event,
                toUInt32(maxIf(created_at, created_at <= toDateTime({as_of:UInt32}, 'UTC')))
                    AS latest_event,
                countIf(
                    created_at >= toDateTime({seven_day_start:UInt32}, 'UTC')
                    AND created_at <= toDateTime({as_of:UInt32}, 'UTC')
                ) AS events_7d,
                countIf(
                    created_at >= toDateTime({thirty_day_start:UInt32}, 'UTC')
                    AND created_at <= toDateTime({as_of:UInt32}, 'UTC')
                ) AS events_30d
            FROM events_local FINAL
            WHERE indexed_at <= toDateTime({indexed_at_max:UInt32}, 'UTC')
            GROUP BY day, kind
            ORDER BY day, kind
            ",
        )
        .param("as_of", as_of)
        .param("indexed_at_max", indexed_at_max)
        .param("seven_day_start", seven_day_start)
        .param("thirty_day_start", thirty_day_start)
        .fetch_all::<ClickhouseAggregateRow>()
        .await
        .context("query exact ClickHouse aggregates from one events_local FINAL scan")?;
    aggregate_clickhouse_rows(rows, completed_start, completed_end)
}

fn aggregate_clickhouse_rows(
    rows: Vec<ClickhouseAggregateRow>,
    completed_start: NaiveDate,
    completed_end: NaiveDate,
) -> Result<ClickhouseSnapshot> {
    let mut api_representable_events = 0_u64;
    let mut earliest_event = None::<u32>;
    let mut latest_event = 0_u32;
    let mut events_7d = 0_u64;
    let mut kinds_30d = BTreeSet::new();
    let mut daily = BTreeMap::new();
    let mut daily_kind = BTreeMap::new();
    let mut completed_kind = BTreeMap::new();
    for row in rows {
        let day = NaiveDate::parse_from_str(&row.day, "%Y-%m-%d")
            .with_context(|| format!("parse ClickHouse UTC day {}", row.day))?;
        api_representable_events = checked_add(
            "ClickHouse API-representable events",
            api_representable_events,
            row.event_count,
        )?;
        earliest_event = Some(earliest_event.map_or(row.earliest_event, |current| {
            current.min(row.earliest_event)
        }));
        latest_event = latest_event.max(row.latest_event);
        events_7d = checked_add("ClickHouse seven-day events", events_7d, row.events_7d)?;
        if row.events_30d != 0 {
            kinds_30d.insert(row.kind);
        }
        if day >= completed_start && day < completed_end {
            let daily_total = daily.entry(row.day.clone()).or_insert(0_u64);
            *daily_total = checked_add("ClickHouse daily events", *daily_total, row.event_count)?;
            if daily_kind
                .insert(format!("{}|{}", row.day, row.kind), row.event_count)
                .is_some()
            {
                bail!("ClickHouse returned a duplicate daily-kind group");
            }
        }
        if day < completed_end {
            let kind_total = completed_kind.entry(row.kind.to_string()).or_insert(0_u64);
            *kind_total = checked_add(
                "ClickHouse completed-day kind events",
                *kind_total,
                row.event_count,
            )?;
        }
    }
    let kinds_30d =
        u64::try_from(kinds_30d.len()).context("ClickHouse 30-day kind count overflowed u64")?;
    let overview = ClickhouseOverview {
        api_representable_events,
        earliest_event: u64::from(
            earliest_event
                .unwrap_or_default()
                .max(NOSTR_GENESIS_TIMESTAMP),
        ),
        latest_event: u64::from(latest_event),
        events_7d,
        kinds_30d,
    };
    Ok(ClickhouseSnapshot {
        overview,
        daily,
        daily_kind,
        completed_kind,
    })
}

fn compare_overview(
    postgres: &PostgresOverview,
    clickhouse: &ClickhouseOverview,
    alignment: InputAlignment,
) -> Vec<MetricComparison> {
    vec![
        compare_metric(
            "api_representable_events",
            endpoints(&["/api/v1/stats/events/total"]),
            postgres.api_representable_events,
            clickhouse.api_representable_events,
            alignment,
        ),
        compare_metric(
            "earliest_event",
            endpoints(&["/api/v1/stats/events/earliest"]),
            postgres.earliest_event,
            clickhouse.earliest_event,
            alignment,
        ),
        compare_metric(
            "latest_event_at_as_of",
            endpoints(&["/api/v1/stats/events/latest"]),
            postgres.latest_event,
            clickhouse.latest_event,
            alignment,
        ),
        compare_metric(
            "events_7d",
            endpoints(&["/api/v1/stats/throughput"]),
            postgres.events_7d,
            clickhouse.events_7d,
            alignment,
        ),
        compare_metric(
            "kinds_30d",
            endpoints(&["/api/v1/stats/kinds/total"]),
            postgres.kinds_30d,
            clickhouse.kinds_30d,
            alignment,
        ),
    ]
}

fn load_alignment(args: &Args, snapshot_id: &str) -> Result<AlignmentReport> {
    let Some(path) = args.alignment_evidence.as_deref() else {
        return Ok(AlignmentReport {
            status: InputAlignment::Unproven,
            evidence_file: None,
            evidence_sha256: None,
            clickhouse_indexed_at_max_epoch: None,
            note: "A fixed event-time does not prove exact input-set alignment; mismatches are old-stack uncertainty.",
        });
    };
    let bytes =
        fs::read(path).with_context(|| format!("read alignment evidence {}", path.display()))?;
    let evidence: AlignmentEvidence = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse alignment evidence {}", path.display()))?;
    if evidence.schema_version != 1 {
        bail!("alignment evidence schema_version must be 1");
    }
    if evidence.evidence_type != "pensieve-clickhouse-parquet-id-parity-v1" {
        bail!("alignment evidence has an unsupported evidence_type");
    }
    if evidence.status != "passed" || !evidence.id_keyed_equal {
        bail!("alignment evidence must be passed with id_keyed_equal=true");
    }
    if evidence.snapshot_id != snapshot_id {
        bail!("alignment evidence snapshot_id does not match the current Postgres run");
    }
    if evidence.clickhouse_database != args.clickhouse_database
        || evidence.clickhouse_table != "events_local"
    {
        bail!("alignment evidence does not describe the selected ClickHouse table");
    }
    if evidence.clickhouse_indexed_at_max_epoch == 0 {
        bail!("alignment evidence indexed_at barrier must be positive");
    }
    if !args.input_alignment_proven {
        bail!("alignment evidence requires --input-alignment-proven");
    }
    Ok(AlignmentReport {
        status: InputAlignment::Proven,
        evidence_file: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        evidence_sha256: Some(hex::encode(Sha256::digest(&bytes))),
        clickhouse_indexed_at_max_epoch: Some(evidence.clickhouse_indexed_at_max_epoch),
        note: "Independent evidence attests exact event-ID input-set alignment for this snapshot.",
    })
}

fn completed_day_range(as_of: u64, days: u64) -> Result<(NaiveDate, NaiveDate)> {
    let as_of = i64::try_from(as_of).context("as_of does not fit i64")?;
    let end = DateTime::<Utc>::from_timestamp(as_of, 0)
        .context("as_of is not a valid Unix timestamp")?
        .date_naive();
    let start = end
        .checked_sub_days(Days::new(days))
        .context("completed-day range underflow")?;
    Ok((start, end))
}

fn nonnegative(name: &str, value: i64) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("{name} must be non-negative"))
}

fn checked_add(name: &str, left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .with_context(|| format!("{name} overflowed u64"))
}

fn endpoints(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn write_report(path: &Path, report: &Report) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create report directory {}", parent.display()))?;
    let partial = parent.join(format!(
        ".{}.partial.{}",
        path.file_name()
            .context("report output must have a file name")?
            .to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .with_context(|| format!("create partial report {}", partial.display()))?;
        serde_json::to_writer_pretty(&mut file, report).context("serialize report")?;
        file.write_all(b"\n").context("finish report")?;
        file.sync_all().context("sync report contents")?;
        fs::hard_link(&partial, path)
            .with_context(|| format!("publish immutable report {}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync report directory {}", parent.display()))?;
        Ok(())
    })();
    let _ = fs::remove_file(&partial);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_grouped_scan_derives_every_clickhouse_product() {
        let rows = vec![
            ClickhouseAggregateRow {
                day: "2026-08-01".to_owned(),
                kind: 1,
                event_count: 10,
                earliest_event: 1_500_000_000,
                latest_event: 1_600_000_000,
                events_7d: 4,
                events_30d: 10,
            },
            ClickhouseAggregateRow {
                day: "2026-08-01".to_owned(),
                kind: 2,
                event_count: 3,
                earliest_event: 1_550_000_000,
                latest_event: 1_650_000_000,
                events_7d: 1,
                events_30d: 3,
            },
            ClickhouseAggregateRow {
                day: "2026-08-03".to_owned(),
                kind: 1,
                event_count: 7,
                earliest_event: 1_700_000_000,
                latest_event: 1_750_000_000,
                events_7d: 7,
                events_30d: 7,
            },
        ];
        let snapshot = aggregate_clickhouse_rows(
            rows,
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.overview.api_representable_events, 20);
        assert_eq!(
            snapshot.overview.earliest_event,
            u64::from(NOSTR_GENESIS_TIMESTAMP)
        );
        assert_eq!(snapshot.overview.latest_event, 1_750_000_000);
        assert_eq!(snapshot.overview.events_7d, 12);
        assert_eq!(snapshot.overview.kinds_30d, 2);
        assert_eq!(snapshot.daily.get("2026-08-01"), Some(&13));
        assert_eq!(snapshot.daily_kind.get("2026-08-01|1"), Some(&10));
        assert_eq!(snapshot.completed_kind.get("1"), Some(&10));
        assert_eq!(snapshot.completed_kind.get("2"), Some(&3));
    }
}
