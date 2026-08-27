//! Validate production flexible-distinct leaves against exact daily metrics.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use clap::Parser;
use pensieve_analytics::{
    DistinctPubkeysPeriod, FlexibleDistinctWindow, estimate_flexible_distinct_windows,
    load_bounded_fixed_activity, load_bounded_flexible_distinct, publish_canonical_json,
};
use serde::Serialize;

const RUNNER_VERSION: &str = "pensieve-analytics-flexible-distinct-validation-v1";
const DEFAULT_TOLERANCE_PPM: u64 = 20_000;
const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug, Parser)]
#[command(about = "Compare bounded flexible distinct sketches with exact daily products")]
struct Args {
    /// Validated Slice 5 fixed-activity evidence.
    #[arg(long)]
    activity_evidence: PathBuf,
    /// Validated Slice 6 flexible-distinct evidence.
    #[arg(long)]
    flexible_evidence: PathBuf,
    /// Canonical immutable validation evidence to create.
    #[arg(long)]
    evidence: PathBuf,
    /// Maximum accepted relative error in parts per million.
    #[arg(long, default_value_t = DEFAULT_TOLERANCE_PPM)]
    tolerance_ppm: u64,
}

#[derive(Clone, Debug, Serialize)]
struct ValidationSample {
    period_start: String,
    since_epoch: u64,
    until_epoch: u64,
    kind: Option<u16>,
    exact_unique_pubkeys: u64,
    estimated_unique_pubkeys: u64,
    absolute_error: u64,
    relative_error_ppm: u64,
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct ValidationEvidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    snapshot_id: String,
    as_of_epoch: u64,
    complete_through_epoch: u64,
    activity_evidence_sha256: String,
    flexible_evidence_sha256: String,
    tolerance_ppm: u64,
    sample_count: u64,
    max_absolute_error: u64,
    max_relative_error_ppm: u64,
    samples: Vec<ValidationSample>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("flexible-distinct validation failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let activity = load_bounded_fixed_activity(&args.activity_evidence)
        .context("load validated fixed-activity evidence")?;
    let flexible = load_bounded_flexible_distinct(&args.flexible_evidence)
        .context("load validated flexible-distinct evidence")?;
    if activity.evidence.snapshot_id != flexible.evidence.snapshot_id
        || activity.evidence.as_of_epoch != flexible.evidence.as_of_epoch
        || activity.evidence_sha256 != flexible.evidence.activity_evidence_sha256
    {
        bail!("fixed-activity and flexible-distinct evidence identities differ");
    }

    let exact = select_samples(
        &activity.evidence.distinct_pubkeys,
        flexible.evidence.complete_through_epoch,
    )?;
    let windows = exact
        .iter()
        .map(|(_, since_epoch, row)| FlexibleDistinctWindow {
            since_epoch: *since_epoch,
            until_epoch: since_epoch + SECONDS_PER_DAY,
            kind: row.kind,
        })
        .collect::<Vec<_>>();
    let estimates = estimate_flexible_distinct_windows(&flexible, &windows)
        .context("estimate representative complete-day windows")?;
    let mut samples = Vec::with_capacity(exact.len());
    for (((period_start, since_epoch, row), window), estimate) in
        exact.into_iter().zip(windows).zip(estimates)
    {
        let absolute_error = row.unique_pubkeys.abs_diff(estimate);
        let relative_error_ppm = relative_error_ppm(absolute_error, row.unique_pubkeys)?;
        samples.push(ValidationSample {
            period_start,
            since_epoch,
            until_epoch: window.until_epoch,
            kind: row.kind,
            exact_unique_pubkeys: row.unique_pubkeys,
            estimated_unique_pubkeys: estimate,
            absolute_error,
            relative_error_ppm,
            accepted: relative_error_ppm <= args.tolerance_ppm,
        });
    }
    let max_absolute_error = samples
        .iter()
        .map(|sample| sample.absolute_error)
        .max()
        .unwrap_or(0);
    let max_relative_error_ppm = samples
        .iter()
        .map(|sample| sample.relative_error_ppm)
        .max()
        .unwrap_or(0);
    if samples.is_empty() || samples.iter().any(|sample| !sample.accepted) {
        bail!(
            "representative flexible-distinct error exceeds {} ppm",
            args.tolerance_ppm
        );
    }
    let evidence = ValidationEvidence {
        schema_version: 1,
        runner_version: RUNNER_VERSION,
        status: "passed",
        snapshot_id: flexible.evidence.snapshot_id.clone(),
        as_of_epoch: flexible.evidence.as_of_epoch,
        complete_through_epoch: flexible.evidence.complete_through_epoch,
        activity_evidence_sha256: activity.evidence_sha256,
        flexible_evidence_sha256: flexible.evidence_sha256,
        tolerance_ppm: args.tolerance_ppm,
        sample_count: u64::try_from(samples.len()).context("sample count exceeds u64")?,
        max_absolute_error,
        max_relative_error_ppm,
        samples,
    };
    publish_canonical_json(&args.evidence, &evidence)
        .context("publish canonical flexible-distinct validation evidence")?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn select_samples(
    rows: &[DistinctPubkeysPeriod],
    complete_through_epoch: u64,
) -> Result<Vec<(String, u64, &DistinctPubkeysPeriod)>> {
    let mut categories = [Vec::new(), Vec::new(), Vec::new()];
    for row in rows {
        if row.grain != "day" || row.unique_pubkeys == 0 {
            continue;
        }
        let since_epoch = parse_day(&row.period_start)?;
        if since_epoch + SECONDS_PER_DAY > complete_through_epoch {
            continue;
        }
        let category = match row.kind {
            None => 0,
            Some(30_023) => 2,
            Some(_) => 1,
        };
        categories[category].push((
            row.unique_pubkeys,
            row.period_start.clone(),
            since_epoch,
            row,
        ));
    }
    let mut selected = BTreeSet::new();
    let mut samples = Vec::new();
    for category in &mut categories {
        category.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.3.kind.cmp(&right.3.kind))
        });
        if category.is_empty() {
            continue;
        }
        for index in [0, category.len() / 2, category.len() - 1] {
            let (_, period_start, since_epoch, row) = &category[index];
            let key = (period_start.clone(), row.kind);
            if selected.insert(key) {
                samples.push((period_start.clone(), *since_epoch, *row));
            }
        }
    }
    samples.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.2.kind.cmp(&right.2.kind))
    });
    Ok(samples)
}

fn parse_day(value: &str) -> Result<u64> {
    let timestamp = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("parse UTC day {value}"))?
        .and_hms_opt(0, 0, 0)
        .context("construct UTC midnight")?
        .and_utc()
        .timestamp();
    u64::try_from(timestamp).context("UTC day precedes Unix epoch")
}

fn relative_error_ppm(absolute_error: u64, exact: u64) -> Result<u64> {
    if exact == 0 {
        return Ok(if absolute_error == 0 { 0 } else { u64::MAX });
    }
    let numerator = u128::from(absolute_error)
        .checked_mul(1_000_000)
        .context("relative-error numerator overflow")?;
    let rounded_up = numerator
        .checked_add(u128::from(exact) - 1)
        .context("relative-error rounding overflow")?
        / u128::from(exact);
    u64::try_from(rounded_up).context("relative error exceeds u64")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_error_is_conservative_and_exact_at_zero() {
        assert_eq!(relative_error_ppm(0, 10).expect("exact"), 0);
        assert_eq!(relative_error_ppm(1, 3).expect("rounded"), 333_334);
        assert_eq!(relative_error_ppm(0, 0).expect("both zero"), 0);
        assert_eq!(relative_error_ppm(1, 0).expect("invalid zero"), u64::MAX);
    }

    #[test]
    fn sample_selection_covers_all_kind_per_kind_and_long_form() {
        let rows = [
            row("2026-01-01", None, 10),
            row("2026-01-02", None, 100),
            row("2026-01-01", Some(1), 5),
            row("2026-01-02", Some(1), 50),
            row("2026-01-01", Some(30_023), 2),
            row("2026-01-02", Some(30_023), 20),
        ];
        let selected =
            select_samples(&rows, parse_day("2026-01-03").expect("day")).expect("select samples");
        assert_eq!(selected.len(), 6);
        assert!(selected.iter().any(|(_, _, row)| row.kind.is_none()));
        assert!(selected.iter().any(|(_, _, row)| row.kind == Some(1)));
        assert!(selected.iter().any(|(_, _, row)| row.kind == Some(30_023)));
    }

    fn row(period_start: &str, kind: Option<u16>, unique_pubkeys: u64) -> DistinctPubkeysPeriod {
        DistinctPubkeysPeriod {
            grain: "day".to_owned(),
            period_start: period_start.to_owned(),
            kind,
            unique_pubkeys,
        }
    }
}
