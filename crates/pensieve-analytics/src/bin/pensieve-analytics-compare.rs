//! Compare an immutable Postgres Slice A publication with sharded ClickHouse snapshots.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

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
const SHARD_SCHEMA_VERSION: u32 = 1;
const HARNESS_VERSION: &str = "slice-a-compare-v2";
const DEFAULT_CLICKHOUSE_MEMORY: u64 = 16 * 1024 * 1024 * 1024;
const DEFAULT_CLICKHOUSE_SHARDS: u16 = 256;
const SEVEN_DAYS_SECONDS: u64 = 7 * 24 * 60 * 60;
const THIRTY_DAYS_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Parser)]
#[command(about = "Diff Postgres Slice A metrics against checkpointed ClickHouse ID shards")]
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
    /// Compare a specific immutable Postgres run instead of the current run.
    #[arg(long)]
    postgres_run_id: Option<String>,
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
    /// Number of ordered event-ID ranges used for resumable ClickHouse work.
    #[arg(long, default_value_t = DEFAULT_CLICKHOUSE_SHARDS)]
    clickhouse_shards: u16,
    /// Directory for immutable per-shard checkpoints; defaults beside --output.
    #[arg(long)]
    clickhouse_checkpoint_dir: Option<PathBuf>,
    /// Pause between newly completed shards to limit sustained production load.
    #[arg(long, default_value_t = 0)]
    clickhouse_shard_delay_seconds: u64,
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
    shards: u16,
    resumed_shards: u16,
    checkpoint_directory: String,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClickhouseSnapshot {
    api_representable_events: u64,
    earliest_event: Option<u32>,
    latest_event: u32,
    events_7d: u64,
    kinds_30d: BTreeSet<u16>,
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
struct ClickhouseKindRow {
    kind: u16,
    event_count: u64,
    earliest_event: u32,
    latest_event: u32,
    events_7d: u64,
    events_30d: u64,
    completed_events: u64,
}

#[derive(Debug, Deserialize, Row)]
struct ClickhouseDailyRow {
    day: String,
    kind: u16,
    event_count: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ShardCheckpoint {
    schema_version: u32,
    harness_version: String,
    clickhouse_database: String,
    clickhouse_table: String,
    snapshot_id: String,
    as_of_epoch: u64,
    indexed_at_max_epoch: Option<u64>,
    completed_day_start: NaiveDate,
    completed_day_end_exclusive: NaiveDate,
    shard_index: u16,
    shard_count: u16,
    id_lower_inclusive: Option<String>,
    id_upper_exclusive: Option<String>,
    snapshot: ClickhouseSnapshot,
}

#[derive(Clone, Copy)]
struct ShardScope<'a> {
    clickhouse_database: &'a str,
    snapshot_id: &'a str,
    as_of_epoch: u64,
    indexed_at_max_epoch: Option<u64>,
    completed_day_start: NaiveDate,
    completed_day_end_exclusive: NaiveDate,
    shard_count: u16,
    shard_delay_seconds: u64,
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
    let run = load_run(&mut postgres, args.postgres_run_id.as_deref())?;
    let (completed_start, completed_end) =
        completed_day_range(run.as_of_epoch, args.completed_days)?;
    let postgres_snapshot = load_postgres_snapshot(postgres, run, completed_start, completed_end)?;
    let comparison_started_epoch =
        u64::try_from(Utc::now().timestamp()).context("comparison start timestamp is negative")?;
    let alignment = load_alignment(
        &args,
        &postgres_snapshot.run.snapshot_id,
        comparison_started_epoch,
    )?;

    let clickhouse_client = connect_clickhouse(&args);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create ClickHouse query runtime")?;
    let checkpoint_dir = args
        .clickhouse_checkpoint_dir
        .clone()
        .unwrap_or_else(|| checkpoint_directory(&args.output));
    let shard_scope = ShardScope {
        clickhouse_database: &args.clickhouse_database,
        snapshot_id: &postgres_snapshot.run.snapshot_id,
        as_of_epoch: postgres_snapshot.run.as_of_epoch,
        indexed_at_max_epoch: alignment.clickhouse_indexed_at_max_epoch,
        completed_day_start: completed_start,
        completed_day_end_exclusive: completed_end,
        shard_count: args.clickhouse_shards,
        shard_delay_seconds: args.clickhouse_shard_delay_seconds,
    };
    let (clickhouse_snapshot, resumed_shards) = runtime.block_on(load_clickhouse_snapshot(
        &clickhouse_client,
        &checkpoint_dir,
        shard_scope,
    ))?;
    let clickhouse_overview = clickhouse_snapshot.overview();

    let metrics = compare_overview(
        &postgres_snapshot.overview,
        &clickhouse_overview,
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
            deduplication: "argMax by event id at a fixed indexed_at barrier",
            max_threads: args.clickhouse_max_threads,
            max_memory_usage: args.clickhouse_max_memory_usage,
            max_execution_time: args.clickhouse_max_execution_time,
            shards: args.clickhouse_shards,
            resumed_shards,
            checkpoint_directory: checkpoint_dir.to_string_lossy().into_owned(),
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
    if args.clickhouse_shards == 0
        || args.clickhouse_shards > 256
        || 256 % args.clickhouse_shards != 0
        || !args.clickhouse_shards.is_power_of_two()
    {
        bail!("--clickhouse-shards must be a power-of-two divisor of 256");
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

fn load_run(client: &mut PostgresClient, run_id: Option<&str>) -> Result<RunMetadata> {
    let row = match run_id {
        Some(run_id) => client.query_one(
            "
            SELECT run_id, snapshot_id, previous_run_id, run_kind,
                   query_version, code_version, as_of_epoch, published_at,
                   physical_rows, logical_events, duplicate_rows,
                   api_representable_events
            FROM pensieve_analytics.runs
            WHERE run_id = $1
            ",
            &[&run_id],
        ),
        None => client.query_one(
            "
            SELECT run_id, snapshot_id, previous_run_id, run_kind,
                   query_version, code_version, as_of_epoch, published_at,
                   physical_rows, logical_events, duplicate_rows,
                   api_representable_events
            FROM pensieve_analytics.current_run_metadata
            ",
            &[],
        ),
    }
    .context("load selected Postgres analytics run")?;
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
    checkpoint_dir: &Path,
    scope: ShardScope<'_>,
) -> Result<(ClickhouseSnapshot, u16)> {
    fs::create_dir_all(checkpoint_dir).with_context(|| {
        format!(
            "create ClickHouse checkpoint directory {}",
            checkpoint_dir.display()
        )
    })?;
    let mut combined = ClickhouseSnapshot::empty();
    let mut resumed_shards = 0_u16;
    for shard_index in 0..scope.shard_count {
        let (lower, upper) = shard_bounds(shard_index, scope.shard_count);
        let checkpoint_path = checkpoint_dir.join(format!(
            "shard-{shard_index:03}-of-{:03}.json",
            scope.shard_count
        ));
        let (snapshot, resumed) = if checkpoint_path.exists() {
            let checkpoint = read_shard_checkpoint(&checkpoint_path)?;
            validate_shard_checkpoint(
                &checkpoint,
                scope,
                shard_index,
                lower.as_deref(),
                upper.as_deref(),
            )?;
            (checkpoint.snapshot, true)
        } else {
            let snapshot = query_clickhouse_shard(
                client,
                scope.as_of_epoch,
                scope.indexed_at_max_epoch,
                scope.completed_day_start,
                scope.completed_day_end_exclusive,
                lower.as_deref(),
                upper.as_deref(),
            )
            .await
            .with_context(|| {
                format!("query ClickHouse shard {shard_index}/{}", scope.shard_count)
            })?;
            let checkpoint = ShardCheckpoint {
                schema_version: SHARD_SCHEMA_VERSION,
                harness_version: HARNESS_VERSION.to_owned(),
                clickhouse_database: scope.clickhouse_database.to_owned(),
                clickhouse_table: "events_local".to_owned(),
                snapshot_id: scope.snapshot_id.to_owned(),
                as_of_epoch: scope.as_of_epoch,
                indexed_at_max_epoch: scope.indexed_at_max_epoch,
                completed_day_start: scope.completed_day_start,
                completed_day_end_exclusive: scope.completed_day_end_exclusive,
                shard_index,
                shard_count: scope.shard_count,
                id_lower_inclusive: lower.clone(),
                id_upper_exclusive: upper.clone(),
                snapshot: snapshot.clone(),
            };
            write_json_immutable(&checkpoint_path, &checkpoint, "shard checkpoint")?;
            (snapshot, false)
        };
        if resumed {
            resumed_shards = resumed_shards
                .checked_add(1)
                .context("resumed shard count overflowed u16")?;
        }
        combined.merge(snapshot)?;
        eprintln!(
            "ClickHouse shard {}/{} {}",
            shard_index + 1,
            scope.shard_count,
            if resumed { "resumed" } else { "completed" }
        );
        if !resumed && scope.shard_delay_seconds != 0 && shard_index + 1 < scope.shard_count {
            tokio::time::sleep(Duration::from_secs(scope.shard_delay_seconds)).await;
        }
    }
    Ok((combined, resumed_shards))
}

async fn query_clickhouse_shard(
    client: &clickhouse::Client,
    as_of: u64,
    indexed_at_max: Option<u64>,
    completed_start: NaiveDate,
    completed_end: NaiveDate,
    id_lower: Option<&str>,
    id_upper: Option<&str>,
) -> Result<ClickhouseSnapshot> {
    let as_of = u32::try_from(as_of).context("as_of exceeds ClickHouse DateTime domain")?;
    let indexed_at_max = indexed_at_max
        .map(u32::try_from)
        .transpose()
        .context("alignment indexed_at barrier exceeds ClickHouse DateTime domain")?
        .unwrap_or(u32::MAX);
    let seven_day_start = as_of.saturating_sub(SEVEN_DAYS_SECONDS as u32);
    let thirty_day_start = as_of.saturating_sub(THIRTY_DAYS_SECONDS as u32);
    let completed_start_epoch = midnight_epoch(completed_start)?;
    let completed_end_epoch = midnight_epoch(completed_end)?;
    let id_filter = id_filter(id_lower, id_upper);
    let kind_sql = format!(
        "
            SELECT
                kind,
                count() AS event_count,
                toUInt32(min(created_at)) AS earliest_event,
                toUInt32(maxIf(created_at, created_at <= toDateTime({{as_of:UInt32}}, 'UTC')))
                    AS latest_event,
                countIf(
                    created_at >= toDateTime({{seven_day_start:UInt32}}, 'UTC')
                    AND created_at <= toDateTime({{as_of:UInt32}}, 'UTC')
                ) AS events_7d,
                countIf(
                    created_at >= toDateTime({{thirty_day_start:UInt32}}, 'UTC')
                    AND created_at <= toDateTime({{as_of:UInt32}}, 'UTC')
                ) AS events_30d,
                countIf(created_at < toDateTime({{completed_end:UInt32}}, 'UTC'))
                    AS completed_events
            FROM (
                SELECT id,
                       argMax(created_at, indexed_at) AS created_at,
                       argMax(kind, indexed_at) AS kind
                FROM events_local
                WHERE indexed_at <= toDateTime({{indexed_at_max:UInt32}}, 'UTC'){id_filter}
                GROUP BY id
            )
            GROUP BY kind
            ORDER BY kind
            "
    );
    let mut kind_query = client
        .query(&kind_sql)
        .param("as_of", as_of)
        .param("indexed_at_max", indexed_at_max)
        .param("seven_day_start", seven_day_start)
        .param("thirty_day_start", thirty_day_start)
        .param("completed_end", completed_end_epoch);
    if let Some(lower) = id_lower {
        kind_query = kind_query.param("id_lower", lower);
    }
    if let Some(upper) = id_upper {
        kind_query = kind_query.param("id_upper", upper);
    }
    let kind_rows = kind_query
        .fetch_all::<ClickhouseKindRow>()
        .await
        .context("query exact ClickHouse kind aggregates")?;

    let daily_sql = format!(
        "
        SELECT toString(toDate(created_at, 'UTC')) AS day,
               kind, count() AS event_count
        FROM (
            SELECT id,
                   argMax(created_at, indexed_at) AS created_at,
                   argMax(kind, indexed_at) AS kind
            FROM events_local
            WHERE indexed_at <= toDateTime({{indexed_at_max:UInt32}}, 'UTC'){id_filter}
            GROUP BY id
        )
        WHERE created_at >= toDateTime({{completed_start:UInt32}}, 'UTC')
          AND created_at < toDateTime({{completed_end:UInt32}}, 'UTC')
        GROUP BY day, kind
        ORDER BY day, kind
        "
    );
    let mut daily_query = client
        .query(&daily_sql)
        .param("indexed_at_max", indexed_at_max)
        .param("completed_start", completed_start_epoch)
        .param("completed_end", completed_end_epoch);
    if let Some(lower) = id_lower {
        daily_query = daily_query.param("id_lower", lower);
    }
    if let Some(upper) = id_upper {
        daily_query = daily_query.param("id_upper", upper);
    }
    let daily_rows = daily_query
        .fetch_all::<ClickhouseDailyRow>()
        .await
        .context("query exact ClickHouse bounded daily-kind aggregates")?;

    aggregate_clickhouse_rows(kind_rows, daily_rows)
}

fn aggregate_clickhouse_rows(
    kind_rows: Vec<ClickhouseKindRow>,
    daily_rows: Vec<ClickhouseDailyRow>,
) -> Result<ClickhouseSnapshot> {
    let mut api_representable_events = 0_u64;
    let mut earliest_event = None::<u32>;
    let mut latest_event = 0_u32;
    let mut events_7d = 0_u64;
    let mut kinds_30d = BTreeSet::new();
    let mut daily = BTreeMap::new();
    let mut daily_kind = BTreeMap::new();
    let mut completed_kind = BTreeMap::new();
    for row in kind_rows {
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
        if row.completed_events != 0 {
            completed_kind.insert(row.kind.to_string(), row.completed_events);
        }
    }
    for row in daily_rows {
        NaiveDate::parse_from_str(&row.day, "%Y-%m-%d")
            .with_context(|| format!("parse ClickHouse UTC day {}", row.day))?;
        let daily_total = daily.entry(row.day.clone()).or_insert(0_u64);
        *daily_total = checked_add("ClickHouse daily events", *daily_total, row.event_count)?;
        if daily_kind
            .insert(format!("{}|{}", row.day, row.kind), row.event_count)
            .is_some()
        {
            bail!("ClickHouse returned a duplicate daily-kind group");
        }
    }
    Ok(ClickhouseSnapshot {
        api_representable_events,
        earliest_event,
        latest_event,
        events_7d,
        kinds_30d,
        daily,
        daily_kind,
        completed_kind,
    })
}

impl ClickhouseSnapshot {
    fn empty() -> Self {
        Self {
            api_representable_events: 0,
            earliest_event: None,
            latest_event: 0,
            events_7d: 0,
            kinds_30d: BTreeSet::new(),
            daily: BTreeMap::new(),
            daily_kind: BTreeMap::new(),
            completed_kind: BTreeMap::new(),
        }
    }

    fn merge(&mut self, other: Self) -> Result<()> {
        self.api_representable_events = checked_add(
            "merged ClickHouse API-representable events",
            self.api_representable_events,
            other.api_representable_events,
        )?;
        if let Some(other_earliest) = other.earliest_event {
            self.earliest_event = Some(
                self.earliest_event
                    .map_or(other_earliest, |current| current.min(other_earliest)),
            );
        }
        self.latest_event = self.latest_event.max(other.latest_event);
        self.events_7d = checked_add(
            "merged ClickHouse seven-day events",
            self.events_7d,
            other.events_7d,
        )?;
        self.kinds_30d.extend(other.kinds_30d);
        merge_map(
            "merged ClickHouse daily events",
            &mut self.daily,
            other.daily,
        )?;
        merge_map(
            "merged ClickHouse daily-kind events",
            &mut self.daily_kind,
            other.daily_kind,
        )?;
        merge_map(
            "merged ClickHouse completed kind events",
            &mut self.completed_kind,
            other.completed_kind,
        )?;
        Ok(())
    }

    fn overview(&self) -> ClickhouseOverview {
        ClickhouseOverview {
            api_representable_events: self.api_representable_events,
            earliest_event: u64::from(
                self.earliest_event
                    .unwrap_or_default()
                    .max(NOSTR_GENESIS_TIMESTAMP),
            ),
            latest_event: u64::from(self.latest_event),
            events_7d: self.events_7d,
            kinds_30d: self.kinds_30d.len() as u64,
        }
    }
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

fn load_alignment(
    args: &Args,
    snapshot_id: &str,
    comparison_started_epoch: u64,
) -> Result<AlignmentReport> {
    let Some(path) = args.alignment_evidence.as_deref() else {
        return Ok(AlignmentReport {
            status: InputAlignment::Unproven,
            evidence_file: None,
            evidence_sha256: None,
            clickhouse_indexed_at_max_epoch: Some(comparison_started_epoch),
            note: "ClickHouse is frozen at the comparison start indexed_at barrier, but exact cross-store input alignment remains unproven; mismatches are old-stack uncertainty.",
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

fn checkpoint_directory(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .map_or_else(|| "comparison".into(), |name| name.to_string_lossy());
    parent.join(format!("{name}.checkpoints"))
}

fn shard_bounds(shard_index: u16, shard_count: u16) -> (Option<String>, Option<String>) {
    let width = 256 / shard_count;
    let lower_byte = shard_index * width;
    let upper_byte = (shard_index + 1) * width;
    let lower = (lower_byte != 0).then(|| id_boundary(lower_byte));
    let upper = (upper_byte != 256).then(|| id_boundary(upper_byte));
    (lower, upper)
}

fn id_boundary(byte: u16) -> String {
    format!("{byte:02x}{}", "0".repeat(62))
}

fn id_filter(lower: Option<&str>, upper: Option<&str>) -> String {
    match (lower, upper) {
        (None, None) => String::new(),
        (Some(_), None) => " AND id >= {id_lower:String}".to_owned(),
        (None, Some(_)) => " AND id < {id_upper:String}".to_owned(),
        (Some(_), Some(_)) => " AND id >= {id_lower:String} AND id < {id_upper:String}".to_owned(),
    }
}

fn midnight_epoch(day: NaiveDate) -> Result<u32> {
    let epoch = day
        .and_hms_opt(0, 0, 0)
        .context("construct UTC midnight")?
        .and_utc()
        .timestamp();
    u32::try_from(epoch).context("UTC midnight exceeds ClickHouse DateTime domain")
}

fn validate_shard_checkpoint(
    checkpoint: &ShardCheckpoint,
    scope: ShardScope<'_>,
    shard_index: u16,
    lower: Option<&str>,
    upper: Option<&str>,
) -> Result<()> {
    if checkpoint.schema_version != SHARD_SCHEMA_VERSION
        || checkpoint.harness_version != HARNESS_VERSION
        || checkpoint.clickhouse_database != scope.clickhouse_database
        || checkpoint.clickhouse_table != "events_local"
        || checkpoint.snapshot_id != scope.snapshot_id
        || checkpoint.as_of_epoch != scope.as_of_epoch
        || checkpoint.indexed_at_max_epoch != scope.indexed_at_max_epoch
        || checkpoint.completed_day_start != scope.completed_day_start
        || checkpoint.completed_day_end_exclusive != scope.completed_day_end_exclusive
        || checkpoint.shard_index != shard_index
        || checkpoint.shard_count != scope.shard_count
        || checkpoint.id_lower_inclusive.as_deref() != lower
        || checkpoint.id_upper_exclusive.as_deref() != upper
    {
        bail!("shard checkpoint metadata does not match the requested comparison");
    }
    Ok(())
}

fn read_shard_checkpoint(path: &Path) -> Result<ShardCheckpoint> {
    let bytes =
        fs::read(path).with_context(|| format!("read shard checkpoint {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse shard checkpoint {}", path.display()))
}

fn merge_map(
    name: &str,
    target: &mut BTreeMap<String, u64>,
    source: BTreeMap<String, u64>,
) -> Result<()> {
    for (key, value) in source {
        let current = target.entry(key).or_default();
        *current = checked_add(name, *current, value)?;
    }
    Ok(())
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
    write_json_immutable(path, report, "report")
}

fn write_json_immutable<T: Serialize>(path: &Path, value: &T, label: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create {label} directory {}", parent.display()))?;
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
            .with_context(|| format!("create partial {label} {}", partial.display()))?;
        serde_json::to_writer_pretty(&mut file, value)
            .with_context(|| format!("serialize {label}"))?;
        file.write_all(b"\n")
            .with_context(|| format!("finish {label}"))?;
        file.sync_all()
            .with_context(|| format!("sync {label} contents"))?;
        fs::hard_link(&partial, path)
            .with_context(|| format!("publish immutable {label} {}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync {label} directory {}", parent.display()))?;
        Ok(())
    })();
    let _ = fs::remove_file(&partial);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_rows_derive_every_clickhouse_product() {
        let kind_rows = vec![
            ClickhouseKindRow {
                kind: 1,
                event_count: 17,
                earliest_event: 1_500_000_000,
                latest_event: 1_750_000_000,
                events_7d: 11,
                events_30d: 17,
                completed_events: 10,
            },
            ClickhouseKindRow {
                kind: 2,
                event_count: 3,
                earliest_event: 1_550_000_000,
                latest_event: 1_650_000_000,
                events_7d: 1,
                events_30d: 3,
                completed_events: 3,
            },
        ];
        let daily_rows = vec![
            ClickhouseDailyRow {
                day: "2026-08-01".to_owned(),
                kind: 1,
                event_count: 10,
            },
            ClickhouseDailyRow {
                day: "2026-08-01".to_owned(),
                kind: 2,
                event_count: 3,
            },
        ];
        let snapshot = aggregate_clickhouse_rows(kind_rows, daily_rows).unwrap();
        let overview = snapshot.overview();
        assert_eq!(overview.api_representable_events, 20);
        assert_eq!(overview.earliest_event, u64::from(NOSTR_GENESIS_TIMESTAMP));
        assert_eq!(overview.latest_event, 1_750_000_000);
        assert_eq!(overview.events_7d, 12);
        assert_eq!(overview.kinds_30d, 2);
        assert_eq!(snapshot.daily.get("2026-08-01"), Some(&13));
        assert_eq!(snapshot.daily_kind.get("2026-08-01|1"), Some(&10));
        assert_eq!(snapshot.completed_kind.get("1"), Some(&10));
        assert_eq!(snapshot.completed_kind.get("2"), Some(&3));
    }

    #[test]
    fn shard_merge_is_additive_but_kind_presence_is_distinct() {
        let mut first = ClickhouseSnapshot::empty();
        first.api_representable_events = 7;
        first.earliest_event = Some(100);
        first.latest_event = 500;
        first.events_7d = 2;
        first.kinds_30d.insert(1);
        first.daily.insert("2026-08-01".to_owned(), 2);
        first.completed_kind.insert("1".to_owned(), 5);

        let mut second = ClickhouseSnapshot::empty();
        second.api_representable_events = 11;
        second.earliest_event = Some(50);
        second.latest_event = 600;
        second.events_7d = 3;
        second.kinds_30d.extend([1, 2]);
        second.daily.insert("2026-08-01".to_owned(), 4);
        second.completed_kind.insert("1".to_owned(), 6);

        first.merge(second).unwrap();
        let overview = first.overview();
        assert_eq!(overview.api_representable_events, 18);
        assert_eq!(first.earliest_event, Some(50));
        assert_eq!(overview.latest_event, 600);
        assert_eq!(overview.events_7d, 5);
        assert_eq!(overview.kinds_30d, 2);
        assert_eq!(first.daily.get("2026-08-01"), Some(&6));
        assert_eq!(first.completed_kind.get("1"), Some(&11));
    }

    #[test]
    fn shard_bounds_cover_the_complete_string_keyspace_without_gaps() {
        let bounds = (0..256)
            .map(|index| shard_bounds(index, 256))
            .collect::<Vec<_>>();
        assert_eq!(bounds.first().unwrap().0, None);
        assert_eq!(bounds.last().unwrap().1, None);
        assert_eq!(bounds[0].1, Some(id_boundary(1)));
        assert_eq!(bounds[1].0, bounds[0].1);
        assert_eq!(bounds[127].1, bounds[128].0);
        assert_eq!(bounds[254].1, bounds[255].0);
    }

    #[test]
    fn immutable_checkpoint_is_reusable_only_for_the_exact_scope() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shard.json");
        let start = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let (lower, upper) = shard_bounds(4, 256);
        let checkpoint = ShardCheckpoint {
            schema_version: SHARD_SCHEMA_VERSION,
            harness_version: HARNESS_VERSION.to_owned(),
            clickhouse_database: "nostr".to_owned(),
            clickhouse_table: "events_local".to_owned(),
            snapshot_id: "sha256:snapshot".to_owned(),
            as_of_epoch: 1_786_000_000,
            indexed_at_max_epoch: Some(1_786_000_100),
            completed_day_start: start,
            completed_day_end_exclusive: end,
            shard_index: 4,
            shard_count: 256,
            id_lower_inclusive: lower.clone(),
            id_upper_exclusive: upper.clone(),
            snapshot: ClickhouseSnapshot::empty(),
        };
        write_json_immutable(&path, &checkpoint, "test checkpoint").unwrap();
        let loaded = read_shard_checkpoint(&path).unwrap();
        let scope = ShardScope {
            clickhouse_database: "nostr",
            snapshot_id: "sha256:snapshot",
            as_of_epoch: 1_786_000_000,
            indexed_at_max_epoch: Some(1_786_000_100),
            completed_day_start: start,
            completed_day_end_exclusive: end,
            shard_count: 256,
            shard_delay_seconds: 0,
        };
        validate_shard_checkpoint(&loaded, scope, 4, lower.as_deref(), upper.as_deref()).unwrap();
        let mismatched_scope = ShardScope {
            snapshot_id: "sha256:different",
            ..scope
        };
        assert!(
            validate_shard_checkpoint(
                &loaded,
                mismatched_scope,
                4,
                lower.as_deref(),
                upper.as_deref(),
            )
            .is_err()
        );
        assert!(write_json_immutable(&path, &checkpoint, "test checkpoint").is_err());
    }
}
