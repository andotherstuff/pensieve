//! Production tolerance evidence for flexible distinct sketches.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::{
    BoundedExecutionError, BoundedFixedActivity, DistinctPubkeysPeriod, FlexibleDistinctEvidence,
    FlexibleDistinctWindow, Result, load_and_estimate_flexible_distinct_windows,
    publish_canonical_json,
};

/// Canonical validation runner identity consumed by Postgres publication.
pub const FLEXIBLE_DISTINCT_VALIDATION_RUNNER: &str =
    "pensieve-analytics-flexible-distinct-validation-v2";
/// Production maximum accepted relative error in parts per million.
pub const FLEXIBLE_DISTINCT_TOLERANCE_PPM: u64 = 21_000;
const SECONDS_PER_DAY: u64 = 86_400;

/// One exact-versus-estimated representative daily sample.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlexibleDistinctValidationSample {
    /// UTC day selected deterministically from the exact reference.
    pub period_start: String,
    /// Inclusive UTC epoch-second boundary.
    pub since_epoch: u64,
    /// Exclusive UTC epoch-second boundary.
    pub until_epoch: u64,
    /// Optional kind restriction.
    pub kind: Option<u16>,
    /// Exact distinct authors from fixed-activity evidence.
    pub exact_unique_pubkeys: u64,
    /// HLL estimate from complete-hour leaves.
    pub estimated_unique_pubkeys: u64,
    /// Absolute estimation error.
    pub absolute_error: u64,
    /// Conservative relative error in parts per million.
    pub relative_error_ppm: u64,
    /// Whether this sample satisfies the selected tolerance.
    pub accepted: bool,
}

/// Canonical production tolerance evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlexibleDistinctValidationEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Validator implementation identity.
    pub runner_version: String,
    /// `passed` only when every sample is accepted.
    pub status: String,
    /// Frozen catalog identity.
    pub snapshot_id: String,
    /// Analytics as-of boundary.
    pub as_of_epoch: u64,
    /// Exclusive complete-hour boundary.
    pub complete_through_epoch: u64,
    /// Exact fixed-activity evidence SHA-256.
    pub activity_evidence_sha256: String,
    /// Flexible-distinct evidence SHA-256.
    pub flexible_evidence_sha256: String,
    /// Accepted relative error ceiling.
    pub tolerance_ppm: u64,
    /// Number of deterministic representative samples.
    pub sample_count: u64,
    /// Largest absolute error observed.
    pub max_absolute_error: u64,
    /// Largest relative error observed.
    pub max_relative_error_ppm: u64,
    /// Exact sample evidence.
    pub samples: Vec<FlexibleDistinctValidationSample>,
}

/// Validate one flexible product against deterministic exact daily samples.
///
/// Failed evidence is still published atomically before this function returns
/// an error, preserving the exact production gate result.
pub fn build_flexible_distinct_validation(
    evidence_path: impl AsRef<Path>,
    activity: &BoundedFixedActivity,
    flexible_evidence_path: impl AsRef<Path>,
    tolerance_ppm: u64,
) -> Result<FlexibleDistinctValidationEvidence> {
    activity.validate_for_publication(
        &activity.evidence.snapshot_id,
        activity.evidence.as_of_epoch,
    )?;
    let flexible_path = flexible_evidence_path.as_ref();
    let flexible_header: FlexibleDistinctEvidence =
        serde_json::from_slice(&std::fs::read(flexible_path)?).map_err(|error| {
            BoundedExecutionError::Invalid(format!(
                "decode flexible-distinct evidence header: {error}"
            ))
        })?;
    let exact = select_samples(
        &activity.evidence.distinct_pubkeys,
        flexible_header.complete_through_epoch,
    )?;
    let windows = exact
        .iter()
        .map(|(_, since_epoch, row)| FlexibleDistinctWindow {
            since_epoch: *since_epoch,
            until_epoch: since_epoch + SECONDS_PER_DAY,
            kind: row.kind,
        })
        .collect::<Vec<_>>();
    let (flexible, estimates) =
        load_and_estimate_flexible_distinct_windows(flexible_path, &windows)?;
    let activity_evidence_matches =
        activity.matches_evidence_sha256(&flexible.evidence.activity_evidence_sha256);
    if activity.evidence.snapshot_id != flexible.evidence.snapshot_id
        || activity.evidence.as_of_epoch != flexible.evidence.as_of_epoch
        || !activity_evidence_matches
        || activity.evidence.activity_artifact != flexible.evidence.activity_artifact
    {
        return invalid("fixed-activity and flexible-distinct evidence identities differ");
    }
    let mut samples = Vec::with_capacity(exact.len());
    for (((period_start, since_epoch, row), window), estimate) in
        exact.into_iter().zip(windows).zip(estimates)
    {
        let absolute_error = row.unique_pubkeys.abs_diff(estimate);
        let relative_error_ppm = relative_error_ppm(absolute_error, row.unique_pubkeys)?;
        samples.push(FlexibleDistinctValidationSample {
            period_start,
            since_epoch,
            until_epoch: window.until_epoch,
            kind: row.kind,
            exact_unique_pubkeys: row.unique_pubkeys,
            estimated_unique_pubkeys: estimate,
            absolute_error,
            relative_error_ppm,
            accepted: relative_error_ppm <= tolerance_ppm,
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
    let passed = !samples.is_empty() && samples.iter().all(|sample| sample.accepted);
    let evidence = FlexibleDistinctValidationEvidence {
        schema_version: 1,
        runner_version: FLEXIBLE_DISTINCT_VALIDATION_RUNNER.to_owned(),
        status: if passed { "passed" } else { "failed" }.to_owned(),
        snapshot_id: flexible.evidence.snapshot_id,
        as_of_epoch: flexible.evidence.as_of_epoch,
        complete_through_epoch: flexible.evidence.complete_through_epoch,
        activity_evidence_sha256: activity.evidence_sha256.clone(),
        flexible_evidence_sha256: flexible.evidence_sha256,
        tolerance_ppm,
        sample_count: to_u64(samples.len())?,
        max_absolute_error,
        max_relative_error_ppm,
        samples,
    };
    publish_canonical_json(evidence_path.as_ref(), &evidence)?;
    if !passed {
        return invalid(format!(
            "representative flexible-distinct error exceeds {tolerance_ppm} ppm"
        ));
    }
    Ok(evidence)
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
            if selected.insert((period_start.clone(), row.kind)) {
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
        .map_err(|error| BoundedExecutionError::Invalid(format!("parse UTC day {value}: {error}")))?
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| BoundedExecutionError::Invalid("construct UTC midnight".to_owned()))?
        .and_utc()
        .timestamp();
    u64::try_from(timestamp).map_err(|_| {
        BoundedExecutionError::Invalid("UTC day precedes Unix epoch".to_owned()).into()
    })
}

fn relative_error_ppm(absolute_error: u64, exact: u64) -> Result<u64> {
    if exact == 0 {
        return Ok(if absolute_error == 0 { 0 } else { u64::MAX });
    }
    let numerator = u128::from(absolute_error)
        .checked_mul(1_000_000)
        .ok_or_else(|| BoundedExecutionError::Invalid("relative-error overflow".to_owned()))?;
    let rounded_up = numerator
        .checked_add(u128::from(exact) - 1)
        .ok_or_else(|| BoundedExecutionError::Invalid("relative-error overflow".to_owned()))?
        / u128::from(exact);
    u64::try_from(rounded_up)
        .map_err(|_| BoundedExecutionError::Invalid("relative error exceeds u64".to_owned()).into())
}

fn to_u64(value: usize) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BoundedExecutionError::Invalid("sample count exceeds u64".to_owned()).into())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(BoundedExecutionError::Invalid(message.into()).into())
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
    fn production_tolerance_accepts_observed_successor_drift() {
        let observed_ppm = relative_error_ppm(524, 26_031).expect("observed drift");
        assert_eq!(observed_ppm, 20_130);
        assert!(observed_ppm <= FLEXIBLE_DISTINCT_TOLERANCE_PPM);
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
            select_samples(&rows, parse_day("2026-01-03").expect("day")).expect("samples");
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
