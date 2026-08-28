//! Preserve bounded independent Slice 7 comparisons against production ClickHouse.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use clickhouse::Row;
use pensieve_analytics::{
    EngagementDay, LongformDay, SemanticRollups, ZapDay, load_bounded_semantic_facts,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-analytics-semantic-comparison-v1";
const DAY_SECONDS: u64 = 86_400;

#[derive(Debug, Parser)]
#[command(about = "Compare deterministic Slice 7 days with bounded ClickHouse reads")]
struct Args {
    /// Fully validated canonical semantic evidence.
    #[arg(long)]
    evidence: PathBuf,
    /// Immutable compact semantic artifact named by the evidence.
    #[arg(long)]
    artifact: PathBuf,
    /// Immutable comparison evidence; an existing file is never replaced.
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct CanonicalDay {
    day_epoch: u64,
    original_notes: u64,
    replies: u64,
    reactions: u64,
    longform_articles: u64,
    longform_content_bytes: u64,
    zap_receipts: u64,
    accepted_zaps: u64,
    accepted_zap_msats: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhouseRawDay {
    day_epoch: u32,
    original_notes: u64,
    replies: u64,
    reactions: u64,
    longform_articles: u64,
    longform_content_bytes: u64,
    zap_receipts: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Row, serde::Deserialize)]
struct ClickhouseZapDay {
    day_epoch: u32,
    accepted_zaps: u64,
    accepted_zap_msats: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SampleClass {
    Sparse,
    Median,
    Dense,
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
struct DayComparison {
    sample_class: SampleClass,
    day_epoch: u64,
    canonical: CanonicalDay,
    clickhouse_raw: CanonicalDay,
    differences: Vec<MetricDifference>,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    generated_at: DateTime<Utc>,
    snapshot_id: String,
    as_of_epoch: u64,
    semantic_evidence_sha256: String,
    semantic_artifact_sha256: String,
    clickhouse_database: String,
    clickhouse_table: &'static str,
    sample_method: &'static str,
    comparisons: Vec<DayComparison>,
    exact_matches: u64,
    classified_differences: u64,
    unclassified_differences: u64,
    note: &'static str,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("semantic comparison failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let product = load_bounded_semantic_facts(&args.evidence, &args.artifact)
        .context("fully validate canonical semantic product")?;
    let samples = select_samples(&product.evidence.rollups)?;
    let days = samples
        .iter()
        .map(|(_, day)| day.day_epoch)
        .collect::<Vec<_>>();
    let client = connect_clickhouse(&args);
    let raw = query_raw_days(&client, &days, product.evidence.as_of_epoch).await?;
    let zaps = query_zap_days(&client, &days, product.evidence.as_of_epoch).await?;

    let mut comparisons = Vec::with_capacity(samples.len());
    let mut exact_matches = 0_u64;
    let mut classified_differences = 0_u64;
    let mut unclassified_differences = 0_u64;
    for (sample_class, canonical) in samples {
        let clickhouse = clickhouse_day(
            canonical.day_epoch,
            raw.get(&canonical.day_epoch),
            zaps.get(&canonical.day_epoch),
        );
        let differences = classify_differences(&canonical, &clickhouse);
        exact_matches = exact_matches
            .checked_add(8_u64.saturating_sub(differences.len() as u64))
            .context("exact match count overflowed")?;
        for difference in &differences {
            if difference.classification == "unclassified" {
                unclassified_differences = unclassified_differences
                    .checked_add(1)
                    .context("unclassified count overflowed")?;
            } else {
                classified_differences = classified_differences
                    .checked_add(1)
                    .context("classified count overflowed")?;
            }
        }
        comparisons.push(DayComparison {
            sample_class,
            day_epoch: canonical.day_epoch,
            canonical,
            clickhouse_raw: clickhouse,
            differences,
        });
    }
    if unclassified_differences != 0 {
        bail!("semantic comparison found {unclassified_differences} unclassified differences");
    }
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: "passed",
        generated_at: Utc::now(),
        snapshot_id: product.evidence.snapshot_id,
        as_of_epoch: product.evidence.as_of_epoch,
        semantic_evidence_sha256: product.evidence_sha256,
        semantic_artifact_sha256: product.evidence.final_artifact.sha256,
        clickhouse_database: args.clickhouse_database,
        clickhouse_table: "events_local FINAL and zap_amounts_data FINAL",
        sample_method: "sparse, median, and dense canonical UTC days by relevant event count",
        comparisons,
        exact_matches,
        classified_differences,
        unclassified_differences,
        note: "Exact event-ID alignment was deliberately waived. Engagement and long-form count deltas are classified as cross-store population differences. Zap accepted-count and amount deltas are classified as the documented canonical-versus-legacy parser domain. A same-count long-form byte mismatch or engagement split mismatch fails closed as unclassified.",
    };
    write_immutable_json(&args.output, &evidence)?;
    let sha = sha256_file(&args.output)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    println!("evidence_sha256={sha}");
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

fn select_samples(rollups: &SemanticRollups) -> Result<Vec<(SampleClass, CanonicalDay)>> {
    let days = rollups
        .engagement
        .keys()
        .chain(rollups.longform.keys())
        .chain(rollups.zaps.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut ranked = days
        .into_iter()
        .map(|day| {
            let canonical = canonical_day(rollups, day);
            let weight = relevant_events(&canonical);
            (weight, day, canonical)
        })
        .filter(|(weight, _, _)| *weight != 0)
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(weight, day, _)| (*weight, *day));
    if ranked.len() < 3 {
        bail!("semantic comparison requires at least three non-empty UTC days");
    }
    let indices = [0, ranked.len() / 2, ranked.len() - 1];
    let classes = [SampleClass::Sparse, SampleClass::Median, SampleClass::Dense];
    Ok(classes
        .into_iter()
        .zip(indices)
        .map(|(class, index)| (class, ranked[index].2.clone()))
        .collect())
}

fn canonical_day(rollups: &SemanticRollups, day_epoch: u64) -> CanonicalDay {
    let engagement = rollups
        .engagement
        .get(&day_epoch)
        .cloned()
        .unwrap_or_else(|| EngagementDay {
            day_epoch,
            ..EngagementDay::default()
        });
    let longform = rollups
        .longform
        .get(&day_epoch)
        .cloned()
        .unwrap_or_else(|| LongformDay {
            day_epoch,
            ..LongformDay::default()
        });
    let zaps = rollups
        .zaps
        .get(&day_epoch)
        .cloned()
        .unwrap_or_else(|| ZapDay {
            day_epoch,
            ..ZapDay::default()
        });
    CanonicalDay {
        day_epoch,
        original_notes: engagement.original_notes,
        replies: engagement.replies,
        reactions: engagement.reactions,
        longform_articles: longform.articles,
        longform_content_bytes: longform.content_bytes,
        zap_receipts: zaps.accepted + zaps.rejected.iter().sum::<u64>(),
        accepted_zaps: zaps.accepted,
        accepted_zap_msats: zaps.amount_msats,
    }
}

fn relevant_events(day: &CanonicalDay) -> u64 {
    day.original_notes
        .saturating_add(day.replies)
        .saturating_add(day.reactions)
        .saturating_add(day.longform_articles)
        .saturating_add(day.zap_receipts)
}

async fn query_raw_days(
    client: &clickhouse::Client,
    days: &[u64],
    as_of_epoch: u64,
) -> Result<BTreeMap<u64, ClickhouseRawDay>> {
    let day_list = clickhouse_day_list(days)?;
    let as_of = u32::try_from(as_of_epoch).context("semantic as-of exceeds DateTime domain")?;
    let sql = format!(
        "SELECT toUInt32(toUnixTimestamp(toStartOfDay(created_at, 'UTC'))) AS day_epoch,
                countIf(kind = 1 AND NOT arrayExists(t -> t[1] = 'e', tags)) AS original_notes,
                countIf(kind = 1 AND arrayExists(t -> t[1] = 'e', tags)) AS replies,
                countIf(kind = 7) AS reactions,
                countIf(kind = 30023) AS longform_articles,
                toUInt64(sumIf(length(content), kind = 30023)) AS longform_content_bytes,
                countIf(kind = 9735) AS zap_receipts
         FROM events_local FINAL
         WHERE created_at <= toDateTime({{as_of:UInt32}}, 'UTC')
           AND kind IN (1, 7, 9735, 30023)
           AND toUInt32(toUnixTimestamp(toStartOfDay(created_at, 'UTC'))) IN ({day_list})
         GROUP BY day_epoch ORDER BY day_epoch"
    );
    let rows = client
        .query(&sql)
        .param("as_of", as_of)
        .fetch_all::<ClickhouseRawDay>()
        .await
        .context("query bounded ClickHouse semantic sample days")?;
    Ok(rows
        .into_iter()
        .map(|row| (u64::from(row.day_epoch), row))
        .collect())
}

async fn query_zap_days(
    client: &clickhouse::Client,
    days: &[u64],
    as_of_epoch: u64,
) -> Result<BTreeMap<u64, ClickhouseZapDay>> {
    let day_list = clickhouse_day_list(days)?;
    let as_of = u32::try_from(as_of_epoch).context("semantic as-of exceeds DateTime domain")?;
    let sql = format!(
        "SELECT toUInt32(toUnixTimestamp(toStartOfDay(created_at, 'UTC'))) AS day_epoch,
                countIf(amount_msats > 0 AND amount_msats <= 1000000000) AS accepted_zaps,
                toUInt64(sumIf(amount_msats, amount_msats > 0 AND amount_msats <= 1000000000)) AS accepted_zap_msats
         FROM zap_amounts_data FINAL
         WHERE created_at <= toDateTime({{as_of:UInt32}}, 'UTC')
           AND toUInt32(toUnixTimestamp(toStartOfDay(created_at, 'UTC'))) IN ({day_list})
         GROUP BY day_epoch ORDER BY day_epoch"
    );
    let rows = client
        .query(&sql)
        .param("as_of", as_of)
        .fetch_all::<ClickhouseZapDay>()
        .await
        .context("query bounded ClickHouse legacy zap sample days")?;
    Ok(rows
        .into_iter()
        .map(|row| (u64::from(row.day_epoch), row))
        .collect())
}

fn clickhouse_day(
    day_epoch: u64,
    raw: Option<&ClickhouseRawDay>,
    zap: Option<&ClickhouseZapDay>,
) -> CanonicalDay {
    CanonicalDay {
        day_epoch,
        original_notes: raw.map_or(0, |row| row.original_notes),
        replies: raw.map_or(0, |row| row.replies),
        reactions: raw.map_or(0, |row| row.reactions),
        longform_articles: raw.map_or(0, |row| row.longform_articles),
        longform_content_bytes: raw.map_or(0, |row| row.longform_content_bytes),
        zap_receipts: raw.map_or(0, |row| row.zap_receipts),
        accepted_zaps: zap.map_or(0, |row| row.accepted_zaps),
        accepted_zap_msats: zap.map_or(0, |row| row.accepted_zap_msats),
    }
}

fn classify_differences(
    canonical: &CanonicalDay,
    clickhouse: &CanonicalDay,
) -> Vec<MetricDifference> {
    let engagement_population_differs = canonical.original_notes + canonical.replies
        != clickhouse.original_notes + clickhouse.replies;
    let mut differences = Vec::new();
    compare(
        &mut differences,
        "original_notes",
        canonical.original_notes,
        clickhouse.original_notes,
        if engagement_population_differs {
            "cross_store_population"
        } else {
            "unclassified"
        },
    );
    compare(
        &mut differences,
        "replies",
        canonical.replies,
        clickhouse.replies,
        if engagement_population_differs {
            "cross_store_population"
        } else {
            "unclassified"
        },
    );
    compare(
        &mut differences,
        "reactions",
        canonical.reactions,
        clickhouse.reactions,
        "cross_store_population",
    );
    compare(
        &mut differences,
        "longform_articles",
        canonical.longform_articles,
        clickhouse.longform_articles,
        "cross_store_population",
    );
    compare(
        &mut differences,
        "longform_content_bytes",
        canonical.longform_content_bytes,
        clickhouse.longform_content_bytes,
        if canonical.longform_articles != clickhouse.longform_articles {
            "cross_store_population"
        } else {
            "unclassified"
        },
    );
    compare(
        &mut differences,
        "zap_receipts",
        canonical.zap_receipts,
        clickhouse.zap_receipts,
        "cross_store_population",
    );
    compare(
        &mut differences,
        "accepted_zaps",
        canonical.accepted_zaps,
        clickhouse.accepted_zaps,
        "canonical_vs_legacy_zap_parser",
    );
    compare(
        &mut differences,
        "accepted_zap_msats",
        canonical.accepted_zap_msats,
        clickhouse.accepted_zap_msats,
        "canonical_vs_legacy_zap_parser",
    );
    differences
}

fn compare(
    output: &mut Vec<MetricDifference>,
    metric: &'static str,
    canonical: u64,
    clickhouse: u64,
    classification: &'static str,
) {
    if canonical != clickhouse {
        output.push(MetricDifference {
            metric,
            canonical,
            clickhouse,
            delta: i128::from(canonical) - i128::from(clickhouse),
            classification,
        });
    }
}

fn clickhouse_day_list(days: &[u64]) -> Result<String> {
    if days.len() != 3 || days.iter().any(|day| day % DAY_SECONDS != 0) {
        bail!("comparison requires exactly three UTC-day-aligned samples");
    }
    Ok(days
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(","))
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

    fn day(day_epoch: u64, total: u64) -> EngagementDay {
        EngagementDay {
            day_epoch,
            original_notes: total,
            replies: 0,
            reactions: 0,
        }
    }

    #[test]
    fn samples_are_sparse_median_and_dense_with_deterministic_day_ties() {
        let mut rollups = SemanticRollups::default();
        for (epoch, count) in [
            (0, 1),
            (86_400, 5),
            (172_800, 3),
            (259_200, 5),
            (345_600, 9),
        ] {
            rollups.engagement.insert(epoch, day(epoch, count));
        }
        let samples = select_samples(&rollups).expect("select samples");
        assert_eq!(
            samples
                .iter()
                .map(|(_, value)| value.day_epoch)
                .collect::<Vec<_>>(),
            vec![0, 86_400, 345_600]
        );
    }

    #[test]
    fn same_population_engagement_split_fails_closed() {
        let canonical = CanonicalDay {
            original_notes: 8,
            replies: 2,
            ..CanonicalDay::default()
        };
        let clickhouse = CanonicalDay {
            original_notes: 7,
            replies: 3,
            ..CanonicalDay::default()
        };
        let differences = classify_differences(&canonical, &clickhouse);
        assert_eq!(differences.len(), 2);
        assert!(
            differences
                .iter()
                .all(|difference| difference.classification == "unclassified")
        );
    }

    #[test]
    fn documented_population_and_parser_differences_are_classified() {
        let canonical = CanonicalDay {
            original_notes: 8,
            replies: 2,
            reactions: 4,
            longform_articles: 2,
            longform_content_bytes: 100,
            zap_receipts: 5,
            accepted_zaps: 3,
            accepted_zap_msats: 42,
            ..CanonicalDay::default()
        };
        let clickhouse = CanonicalDay {
            original_notes: 7,
            replies: 2,
            reactions: 3,
            longform_articles: 1,
            longform_content_bytes: 40,
            zap_receipts: 4,
            accepted_zaps: 2,
            accepted_zap_msats: 41,
            ..CanonicalDay::default()
        };
        let differences = classify_differences(&canonical, &clickhouse);
        assert!(
            differences
                .iter()
                .all(|difference| difference.classification != "unclassified")
        );
    }
}
