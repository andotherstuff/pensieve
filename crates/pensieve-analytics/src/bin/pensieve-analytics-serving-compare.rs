//! Preserve bounded independent Slice 9.5 comparisons against production ClickHouse.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use pensieve_analytics::{
    ServingHourlyRow, ServingKindRow, load_bounded_serving_facts, visit_serving_hourly_rows,
    visit_serving_kind_rows,
};
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-serving-comparison-v1";
const HOUR_SECONDS: u64 = 3_600;

#[derive(Debug, Parser)]
#[command(about = "Compare bounded Slice 9.5 serving facts with ClickHouse")]
struct Args {
    /// Fully validated serving-facts evidence.
    #[arg(long)]
    evidence: PathBuf,
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
    AllKindSparse,
    AllKindMidpoint,
    AllKindDense,
    PerKindSparse,
    PerKindMidpoint,
    PerKindDense,
    KindSummarySparse,
    KindSummaryMidpoint,
    KindSummaryDense,
}

#[derive(Clone, Debug, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhouseCountRow {
    event_count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhouseKindRow {
    event_count: u64,
    unique_pubkeys: u64,
    first_seen: u32,
    last_seen: u32,
    content_bytes: u64,
    content_rows: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct MetricDifference {
    metric: &'static str,
    canonical: u64,
    clickhouse: u64,
    delta: i128,
    classification: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct HourlyComparison {
    sample_class: SampleClass,
    hour_epoch: u32,
    kind: Option<u16>,
    canonical_event_count: u64,
    clickhouse_event_count: u64,
    differences: Vec<MetricDifference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct KindComparison {
    sample_class: SampleClass,
    kind: u16,
    canonical: KindMetrics,
    clickhouse: KindMetrics,
    differences: Vec<MetricDifference>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct KindMetrics {
    event_count: u64,
    unique_pubkeys: u64,
    first_seen: u64,
    last_seen: u64,
    content_bytes: u64,
    content_rows: u64,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    snapshot_id: String,
    as_of_epoch: u64,
    complete_through_epoch: u64,
    serving_evidence_sha256: String,
    hourly_artifact_sha256: String,
    kind_artifact_sha256: String,
    clickhouse_database: String,
    clickhouse_table: &'static str,
    sample_method: &'static str,
    hourly_comparisons: Vec<HourlyComparison>,
    kind_comparisons: Vec<KindComparison>,
    exact_metrics: u64,
    classified_differences: u64,
    unclassified_differences: u64,
    note: &'static str,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("serving-facts comparison failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let product = load_bounded_serving_facts(&args.evidence)
        .context("fully validate canonical serving facts")?;
    let hourly_samples = select_hourly_samples(&product)?;
    let kind_samples = select_kind_samples(&product)?;
    let client = connect_clickhouse(&args);
    let mut hourly_comparisons = Vec::with_capacity(hourly_samples.len());
    let mut kind_comparisons = Vec::with_capacity(kind_samples.len());
    let mut exact_metrics = 0_u64;
    let mut classified_differences = 0_u64;
    let mut unclassified_differences = 0_u64;

    for (sample_class, canonical) in hourly_samples {
        let clickhouse =
            query_clickhouse_hour(&client, canonical, product.evidence.as_of_epoch).await?;
        let differences = if canonical.event_count == clickhouse {
            exact_metrics = checked_add(exact_metrics, 1, "exact hourly metrics")?;
            Vec::new()
        } else {
            classified_differences =
                checked_add(classified_differences, 1, "classified hourly differences")?;
            vec![metric_difference(
                "event_count",
                canonical.event_count,
                clickhouse,
                "cross_store_population",
            )]
        };
        hourly_comparisons.push(HourlyComparison {
            sample_class,
            hour_epoch: canonical.hour_epoch,
            kind: canonical.kind,
            canonical_event_count: canonical.event_count,
            clickhouse_event_count: clickhouse,
            differences,
        });
    }

    for (sample_class, canonical) in kind_samples {
        let kind = canonical.kind;
        let clickhouse = query_clickhouse_kind(&client, kind, product.evidence.as_of_epoch).await?;
        let canonical = KindMetrics::from(canonical);
        let clickhouse = KindMetrics::from(clickhouse);
        let differences = compare_kind_metrics(&canonical, &clickhouse);
        for difference in &differences {
            if difference.classification == "unclassified" {
                unclassified_differences = checked_add(
                    unclassified_differences,
                    1,
                    "unclassified serving differences",
                )?;
            } else {
                classified_differences =
                    checked_add(classified_differences, 1, "classified serving differences")?;
            }
        }
        exact_metrics = checked_add(
            exact_metrics,
            6_u64.saturating_sub(differences.len() as u64),
            "exact kind metrics",
        )?;
        kind_comparisons.push(KindComparison {
            sample_class,
            kind,
            canonical,
            clickhouse,
            differences,
        });
    }
    if unclassified_differences != 0 {
        bail!("serving-facts comparison found {unclassified_differences} unclassified differences");
    }
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "passed",
        generated_at: Utc::now(),
        snapshot_id: product.evidence.snapshot_id,
        as_of_epoch: product.evidence.as_of_epoch,
        complete_through_epoch: product.evidence.complete_through_epoch,
        serving_evidence_sha256: product.evidence_sha256,
        hourly_artifact_sha256: product.evidence.hourly_artifact.sha256,
        kind_artifact_sha256: product.evidence.kind_artifact.sha256,
        clickhouse_database: args.clickhouse_database,
        clickhouse_table: "events_local FINAL",
        sample_method: "sparse, midpoint-density, and dense canonical rows for all-kind hours, per-kind hours, and all-time kinds",
        hourly_comparisons,
        kind_comparisons,
        exact_metrics,
        classified_differences,
        unclassified_differences,
        note: "Exact event-ID alignment was deliberately waived. Event-count deltas and dependent metric deltas are classified as cross-store population differences. When event counts match, unique-publisher, first/last, and UTF-8 byte disagreements fail closed as unclassified. Slice 6 separately gates flexible distinct estimates; these kind summaries are exact.",
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

fn select_hourly_samples(
    product: &pensieve_analytics::BoundedServingFacts,
) -> Result<Vec<(SampleClass, ServingHourlyRow)>> {
    let mut all = DensityBounds::default();
    let mut kinds = DensityBounds::default();
    visit_serving_hourly_rows(product, |row| {
        if row.kind.is_none() {
            all.observe(row, hourly_key);
        } else {
            kinds.observe(row, hourly_key);
        }
        Ok(())
    })?;
    let all = select_midpoint(product, all, true)?;
    let kinds = select_midpoint(product, kinds, false)?;
    Ok(vec![
        (SampleClass::AllKindSparse, all.0),
        (SampleClass::AllKindMidpoint, all.1),
        (SampleClass::AllKindDense, all.2),
        (SampleClass::PerKindSparse, kinds.0),
        (SampleClass::PerKindMidpoint, kinds.1),
        (SampleClass::PerKindDense, kinds.2),
    ])
}

fn select_midpoint(
    product: &pensieve_analytics::BoundedServingFacts,
    bounds: DensityBounds<ServingHourlyRow>,
    all_kinds: bool,
) -> Result<(ServingHourlyRow, ServingHourlyRow, ServingHourlyRow)> {
    let (sparse, dense) = bounds.endpoints("hourly sample group")?;
    let target = midpoint(sparse.event_count, dense.event_count);
    let mut middle = None;
    visit_serving_hourly_rows(product, |row| {
        if row.kind.is_none() == all_kinds && row != sparse && row != dense {
            observe_middle(&mut middle, row, target, hourly_key);
        }
        Ok(())
    })?;
    Ok((
        sparse,
        middle.context("hourly sample group requires at least three rows")?,
        dense,
    ))
}

fn select_kind_samples(
    product: &pensieve_analytics::BoundedServingFacts,
) -> Result<Vec<(SampleClass, ServingKindRow)>> {
    let mut bounds = DensityBounds::default();
    visit_serving_kind_rows(product, |row| {
        bounds.observe(row, kind_key);
        Ok(())
    })?;
    let (sparse, dense) = bounds.endpoints("kind summary")?;
    let target = midpoint(sparse.event_count, dense.event_count);
    let mut middle = None;
    visit_serving_kind_rows(product, |row| {
        if row != sparse && row != dense {
            observe_middle(&mut middle, row, target, kind_key);
        }
        Ok(())
    })?;
    let middle = middle.context("kind summary requires at least three rows")?;
    Ok(vec![
        (SampleClass::KindSummarySparse, sparse),
        (SampleClass::KindSummaryMidpoint, middle),
        (SampleClass::KindSummaryDense, dense),
    ])
}

#[derive(Clone, Copy, Debug)]
struct DensityBounds<T> {
    sparse: Option<T>,
    dense: Option<T>,
}

impl<T> Default for DensityBounds<T> {
    fn default() -> Self {
        Self {
            sparse: None,
            dense: None,
        }
    }
}

impl<T: Copy> DensityBounds<T> {
    fn observe<K: Ord>(&mut self, row: T, key: impl Fn(T) -> (u64, K) + Copy) {
        if self.sparse.is_none_or(|current| key(row) < key(current)) {
            self.sparse = Some(row);
        }
        if self.dense.is_none_or(|current| {
            let row_key = key(row);
            let current_key = key(current);
            row_key.0 > current_key.0 || (row_key.0 == current_key.0 && row_key.1 < current_key.1)
        }) {
            self.dense = Some(row);
        }
    }

    fn endpoints(self, name: &'static str) -> Result<(T, T)> {
        let sparse = self.sparse.with_context(|| format!("{name} is empty"))?;
        let dense = self.dense.with_context(|| format!("{name} is empty"))?;
        Ok((sparse, dense))
    }
}

fn observe_middle<T: Copy, K: Ord>(
    current: &mut Option<T>,
    row: T,
    target: u64,
    key: impl Fn(T) -> (u64, K) + Copy,
) {
    let candidate = middle_key(row, target, key);
    if current.is_none_or(|value| candidate < middle_key(value, target, key)) {
        *current = Some(row);
    }
}

fn middle_key<T: Copy, K: Ord>(row: T, target: u64, key: impl Fn(T) -> (u64, K)) -> (u64, K) {
    let (count, row_key) = key(row);
    (count.abs_diff(target), row_key)
}

fn midpoint(left: u64, right: u64) -> u64 {
    left + (right - left) / 2
}

fn hourly_key(row: ServingHourlyRow) -> (u64, (u32, Option<u16>)) {
    (row.event_count, (row.hour_epoch, row.kind))
}

fn kind_key(row: ServingKindRow) -> (u64, u16) {
    (row.event_count, row.kind)
}

async fn query_clickhouse_hour(
    client: &clickhouse::Client,
    sample: ServingHourlyRow,
    as_of_epoch: u64,
) -> Result<u64> {
    let start = u64::from(sample.hour_epoch)
        .checked_mul(HOUR_SECONDS)
        .context("sample hour overflowed")?;
    let end = start
        .checked_add(HOUR_SECONDS)
        .context("sample hour overflowed")?;
    if end > as_of_epoch {
        bail!("sampled hour is not complete at the fixed as-of");
    }
    let start = u32::try_from(start).context("sample hour start exceeds DateTime domain")?;
    let end = u32::try_from(end).context("sample hour end exceeds DateTime domain")?;
    let as_of = u32::try_from(as_of_epoch).context("serving as-of exceeds DateTime domain")?;
    let kind_filter = sample
        .kind
        .map_or_else(String::new, |kind| format!(" AND kind={kind}"));
    let sql = format!(
        "SELECT count() AS event_count FROM events_local FINAL
          WHERE created_at >= toDateTime({{genesis:UInt32}}, 'UTC')
            AND created_at >= toDateTime({{start:UInt32}}, 'UTC')
            AND created_at < toDateTime({{end:UInt32}}, 'UTC')
            AND created_at <= toDateTime({{as_of:UInt32}}, 'UTC'){kind_filter}"
    );
    let rows = client
        .query(&sql)
        .param("genesis", NOSTR_GENESIS_TIMESTAMP)
        .param("start", start)
        .param("end", end)
        .param("as_of", as_of)
        .fetch_all::<ClickhouseCountRow>()
        .await
        .context("query bounded ClickHouse hourly sample")?;
    if rows.len() != 1 {
        bail!("ClickHouse hourly query returned {} rows", rows.len());
    }
    Ok(rows[0].event_count)
}

async fn query_clickhouse_kind(
    client: &clickhouse::Client,
    kind: u16,
    as_of_epoch: u64,
) -> Result<ClickhouseKindRow> {
    let as_of = u32::try_from(as_of_epoch).context("serving as-of exceeds DateTime domain")?;
    let sql = format!(
        "SELECT count() AS event_count,uniqExact(pubkey) AS unique_pubkeys,
                toUInt32(min(created_at)) AS first_seen,
                toUInt32(max(created_at)) AS last_seen,
                toUInt64(sum(length(content))) AS content_bytes,
                count() AS content_rows
           FROM events_local FINAL
          WHERE created_at >= toDateTime({{genesis:UInt32}}, 'UTC')
            AND created_at <= toDateTime({{as_of:UInt32}}, 'UTC')
            AND kind={kind}
          GROUP BY kind"
    );
    let rows = client
        .query(&sql)
        .param("genesis", NOSTR_GENESIS_TIMESTAMP)
        .param("as_of", as_of)
        .fetch_all::<ClickhouseKindRow>()
        .await
        .with_context(|| format!("query bounded ClickHouse kind {kind}"))?;
    match rows.as_slice() {
        [] => Ok(ClickhouseKindRow::default()),
        [row] => Ok(row.clone()),
        _ => bail!("ClickHouse kind query returned {} rows", rows.len()),
    }
}

fn compare_kind_metrics(
    canonical: &KindMetrics,
    clickhouse: &KindMetrics,
) -> Vec<MetricDifference> {
    let population_differs = canonical.event_count != clickhouse.event_count;
    let dependent_classification = if population_differs {
        "cross_store_population"
    } else {
        "unclassified"
    };
    let mut output = Vec::new();
    compare_metric(
        &mut output,
        "event_count",
        canonical.event_count,
        clickhouse.event_count,
        "cross_store_population",
    );
    for (metric, left, right) in [
        (
            "unique_pubkeys",
            canonical.unique_pubkeys,
            clickhouse.unique_pubkeys,
        ),
        ("first_seen", canonical.first_seen, clickhouse.first_seen),
        ("last_seen", canonical.last_seen, clickhouse.last_seen),
        (
            "content_bytes",
            canonical.content_bytes,
            clickhouse.content_bytes,
        ),
        (
            "content_rows",
            canonical.content_rows,
            clickhouse.content_rows,
        ),
    ] {
        compare_metric(&mut output, metric, left, right, dependent_classification);
    }
    output
}

fn compare_metric(
    output: &mut Vec<MetricDifference>,
    metric: &'static str,
    canonical: u64,
    clickhouse: u64,
    classification: &'static str,
) {
    if canonical != clickhouse {
        output.push(metric_difference(
            metric,
            canonical,
            clickhouse,
            classification,
        ));
    }
}

fn metric_difference(
    metric: &'static str,
    canonical: u64,
    clickhouse: u64,
    classification: &'static str,
) -> MetricDifference {
    MetricDifference {
        metric,
        canonical,
        clickhouse,
        delta: i128::from(canonical) - i128::from(clickhouse),
        classification,
    }
}

impl From<ServingKindRow> for KindMetrics {
    fn from(row: ServingKindRow) -> Self {
        Self {
            event_count: row.event_count,
            unique_pubkeys: row.unique_pubkeys,
            first_seen: u64::from(row.first_seen),
            last_seen: u64::from(row.last_seen),
            content_bytes: row.content_bytes,
            content_rows: row.content_rows,
        }
    }
}

impl From<ClickhouseKindRow> for KindMetrics {
    fn from(row: ClickhouseKindRow) -> Self {
        Self {
            event_count: row.event_count,
            unique_pubkeys: row.unique_pubkeys,
            first_seen: u64::from(row.first_seen),
            last_seen: u64::from(row.last_seen),
            content_bytes: row.content_bytes,
            content_rows: row.content_rows,
        }
    }
}

fn checked_add(left: u64, right: u64, field: &'static str) -> Result<u64> {
    left.checked_add(right)
        .with_context(|| format!("{field} overflowed"))
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

    fn hour(hour_epoch: u32, kind: Option<u16>, event_count: u64) -> ServingHourlyRow {
        ServingHourlyRow {
            hour_epoch,
            kind,
            event_count,
        }
    }

    fn kind(kind: u16, event_count: u64) -> ServingKindRow {
        ServingKindRow {
            kind,
            event_count,
            unique_pubkeys: event_count.min(2),
            first_seen: 1,
            last_seen: 2,
            content_bytes: event_count * 4,
            content_rows: event_count,
        }
    }

    #[test]
    fn density_selection_is_bounded_and_deterministic() {
        let rows = [hour(3, None, 100), hour(1, None, 1), hour(2, None, 51)];
        let mut bounds = DensityBounds::default();
        for row in rows {
            bounds.observe(row, hourly_key);
        }
        let (sparse, dense) = bounds.endpoints("test").expect("endpoints");
        let target = midpoint(sparse.event_count, dense.event_count);
        let mut middle = None;
        for row in rows {
            if row != sparse && row != dense {
                observe_middle(&mut middle, row, target, hourly_key);
            }
        }
        assert_eq!(sparse, hour(1, None, 1));
        assert_eq!(middle, Some(hour(2, None, 51)));
        assert_eq!(dense, hour(3, None, 100));
    }

    #[test]
    fn same_count_metric_difference_fails_closed() {
        let canonical = KindMetrics::from(kind(1, 10));
        let mut clickhouse = canonical.clone();
        clickhouse.content_bytes += 1;
        let differences = compare_kind_metrics(&canonical, &clickhouse);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].classification, "unclassified");
    }

    #[test]
    fn population_difference_classifies_dependent_metrics() {
        let canonical = KindMetrics::from(kind(1, 10));
        let clickhouse = KindMetrics::from(kind(1, 9));
        let differences = compare_kind_metrics(&canonical, &clickhouse);
        assert!(!differences.is_empty());
        assert!(
            differences
                .iter()
                .all(|difference| difference.classification == "cross_store_population")
        );
    }

    #[test]
    fn midpoint_avoids_overflow() {
        assert_eq!(midpoint(u64::MAX - 2, u64::MAX), u64::MAX - 1);
    }
}
