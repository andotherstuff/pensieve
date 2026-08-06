//! Pure comparison and classification primitives for shadow analytics evidence.

use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Whether an independent proof tied both stores to the same input barrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAlignment {
    /// Event-time is fixed, but the exact ClickHouse input set is not proven.
    Unproven,
    /// Independent evidence proves both stores contain the same event-ID set.
    Proven,
}

/// Ledger classification assigned to one comparison result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Both stores returned exactly the same value.
    ExactMatch,
    /// Aligned inputs returned different exact values; the new implementation is wrong.
    Bug,
    /// Different values cannot be attributed until input alignment is proven.
    OldStackUncertainty,
}

/// One exact scalar comparison.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetricComparison {
    /// Stable metric identifier.
    pub metric: String,
    /// API routes whose semantics this metric helps cover.
    pub endpoints: Vec<String>,
    /// Value published by Postgres.
    pub postgres: u64,
    /// Value computed from `events_local FINAL` at the fixed boundary.
    pub clickhouse: u64,
    /// Signed ClickHouse-minus-Postgres difference, encoded as text to avoid JSON precision loss.
    pub difference: String,
    /// Reconciliation-ledger classification.
    pub classification: Classification,
}

/// One missing or unequal keyed value from a series comparison.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DifferenceExample {
    /// Stable series key.
    pub key: String,
    /// Postgres value, absent when only ClickHouse had the key.
    pub postgres: Option<u64>,
    /// ClickHouse value, absent when only Postgres had the key.
    pub clickhouse: Option<u64>,
    /// Classification inherited from the input-alignment state.
    pub classification: Classification,
}

/// Exact comparison summary for a keyed relation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SeriesComparison {
    /// Stable series identifier.
    pub metric: String,
    /// API routes whose semantics this relation helps cover.
    pub endpoints: Vec<String>,
    /// Human-readable boundary applied to both queries.
    pub scope: String,
    /// Total union of keys compared.
    pub compared_keys: u64,
    /// Keys present with equal values in both stores.
    pub matching_keys: u64,
    /// Keys present in both stores with unequal values.
    pub mismatched_values: u64,
    /// Keys present only in Postgres.
    pub postgres_only_keys: u64,
    /// Keys present only in ClickHouse.
    pub clickhouse_only_keys: u64,
    /// SHA-256 of stable `key\0value\n` rows from Postgres.
    pub postgres_sha256: String,
    /// SHA-256 of stable `key\0value\n` rows from ClickHouse.
    pub clickhouse_sha256: String,
    /// Bounded examples; aggregate counts above always describe the full comparison.
    pub examples: Vec<DifferenceExample>,
}

impl SeriesComparison {
    /// Number of unequal or missing keys.
    #[must_use]
    pub fn difference_count(&self) -> u64 {
        self.mismatched_values + self.postgres_only_keys + self.clickhouse_only_keys
    }
}

/// Whether the current evidence can approve the shadow parity gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonGate {
    /// Aligned inputs and every exact comparison matched.
    Passed,
    /// Comparisons completed, but exact input alignment remains unproven.
    Incomplete,
    /// Aligned inputs produced at least one exact mismatch.
    Failed,
}

/// Aggregate outcome across scalar and keyed comparisons.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReconciliationSummary {
    /// Gate outcome.
    pub gate: ComparisonGate,
    /// Number of exact scalar comparisons.
    pub scalar_metrics: u64,
    /// Number of unequal scalar values.
    pub scalar_differences: u64,
    /// Number of keyed relations.
    pub series: u64,
    /// Total unequal or missing series keys.
    pub series_differences: u64,
}

impl ReconciliationSummary {
    /// Derive the gate result from complete comparison outputs.
    #[must_use]
    pub fn new(
        alignment: InputAlignment,
        metrics: &[MetricComparison],
        series: &[SeriesComparison],
    ) -> Self {
        let scalar_differences = metrics
            .iter()
            .filter(|metric| metric.classification != Classification::ExactMatch)
            .count() as u64;
        let series_differences = series.iter().map(SeriesComparison::difference_count).sum();
        let has_differences = scalar_differences != 0 || series_differences != 0;
        let gate = match (alignment, has_differences) {
            (InputAlignment::Proven, false) => ComparisonGate::Passed,
            (InputAlignment::Proven, true) => ComparisonGate::Failed,
            (InputAlignment::Unproven, _) => ComparisonGate::Incomplete,
        };
        Self {
            gate,
            scalar_metrics: metrics.len() as u64,
            scalar_differences,
            series: series.len() as u64,
            series_differences,
        }
    }
}

/// Compare one exact scalar under the selected input-alignment state.
#[must_use]
pub fn compare_metric(
    metric: impl Into<String>,
    endpoints: Vec<String>,
    postgres: u64,
    clickhouse: u64,
    alignment: InputAlignment,
) -> MetricComparison {
    MetricComparison {
        metric: metric.into(),
        endpoints,
        postgres,
        clickhouse,
        difference: (i128::from(clickhouse) - i128::from(postgres)).to_string(),
        classification: classify(postgres == clickhouse, alignment),
    }
}

/// Compare complete stable maps while retaining only a bounded set of examples.
#[must_use]
pub fn compare_series(
    metric: impl Into<String>,
    endpoints: Vec<String>,
    scope: impl Into<String>,
    postgres: &BTreeMap<String, u64>,
    clickhouse: &BTreeMap<String, u64>,
    alignment: InputAlignment,
    max_examples: usize,
) -> SeriesComparison {
    let mut matching_keys = 0_u64;
    let mut mismatched_values = 0_u64;
    let mut postgres_only_keys = 0_u64;
    let mut clickhouse_only_keys = 0_u64;
    let mut examples = Vec::new();

    for (key, postgres_value) in postgres {
        match clickhouse.get(key) {
            Some(clickhouse_value) if clickhouse_value == postgres_value => matching_keys += 1,
            Some(clickhouse_value) => {
                mismatched_values += 1;
                push_example(
                    &mut examples,
                    max_examples,
                    key,
                    Some(*postgres_value),
                    Some(*clickhouse_value),
                    alignment,
                );
            }
            None => {
                postgres_only_keys += 1;
                push_example(
                    &mut examples,
                    max_examples,
                    key,
                    Some(*postgres_value),
                    None,
                    alignment,
                );
            }
        }
    }
    for (key, clickhouse_value) in clickhouse {
        if !postgres.contains_key(key) {
            clickhouse_only_keys += 1;
            push_example(
                &mut examples,
                max_examples,
                key,
                None,
                Some(*clickhouse_value),
                alignment,
            );
        }
    }

    SeriesComparison {
        metric: metric.into(),
        endpoints,
        scope: scope.into(),
        compared_keys: matching_keys
            + mismatched_values
            + postgres_only_keys
            + clickhouse_only_keys,
        matching_keys,
        mismatched_values,
        postgres_only_keys,
        clickhouse_only_keys,
        postgres_sha256: digest_series(postgres),
        clickhouse_sha256: digest_series(clickhouse),
        examples,
    }
}

fn classify(matches: bool, alignment: InputAlignment) -> Classification {
    if matches {
        Classification::ExactMatch
    } else if alignment == InputAlignment::Proven {
        Classification::Bug
    } else {
        Classification::OldStackUncertainty
    }
}

fn push_example(
    examples: &mut Vec<DifferenceExample>,
    maximum: usize,
    key: &str,
    postgres: Option<u64>,
    clickhouse: Option<u64>,
    alignment: InputAlignment,
) {
    if examples.len() < maximum {
        examples.push(DifferenceExample {
            key: key.to_owned(),
            postgres,
            clickhouse,
            classification: classify(false, alignment),
        });
    }
}

fn digest_series(series: &BTreeMap<String, u64>) -> String {
    let mut digest = Sha256::new();
    for (key, value) in series {
        digest.update(key.as_bytes());
        digest.update([0]);
        digest.update(value.to_string().as_bytes());
        digest.update(b"\n");
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(rows: &[(&str, u64)]) -> BTreeMap<String, u64> {
        rows.iter()
            .map(|(key, value)| ((*key).to_owned(), *value))
            .collect()
    }

    #[test]
    fn unproven_scalar_difference_is_uncertainty() {
        let comparison = compare_metric("events_7d", Vec::new(), 10, 11, InputAlignment::Unproven);
        assert_eq!(
            comparison.classification,
            Classification::OldStackUncertainty
        );
        assert_eq!(comparison.difference, "1");
    }

    #[test]
    fn proven_scalar_difference_is_bug() {
        let comparison = compare_metric("events_7d", Vec::new(), 11, 10, InputAlignment::Proven);
        assert_eq!(comparison.classification, Classification::Bug);
        assert_eq!(comparison.difference, "-1");
    }

    #[test]
    fn series_counts_every_difference_but_bounds_examples() {
        let postgres = map(&[("a", 1), ("b", 2), ("c", 3)]);
        let clickhouse = map(&[("a", 1), ("b", 9), ("d", 4)]);
        let comparison = compare_series(
            "daily",
            Vec::new(),
            "fixture",
            &postgres,
            &clickhouse,
            InputAlignment::Proven,
            2,
        );
        assert_eq!(comparison.compared_keys, 4);
        assert_eq!(comparison.matching_keys, 1);
        assert_eq!(comparison.mismatched_values, 1);
        assert_eq!(comparison.postgres_only_keys, 1);
        assert_eq!(comparison.clickhouse_only_keys, 1);
        assert_eq!(comparison.difference_count(), 3);
        assert_eq!(comparison.examples.len(), 2);
        assert!(
            comparison
                .examples
                .iter()
                .all(|example| example.classification == Classification::Bug)
        );
    }

    #[test]
    fn gate_requires_alignment_even_when_values_match() {
        let metric = compare_metric("latest_event", Vec::new(), 10, 10, InputAlignment::Unproven);
        let summary = ReconciliationSummary::new(InputAlignment::Unproven, &[metric], &[]);
        assert_eq!(summary.gate, ComparisonGate::Incomplete);
        assert_eq!(summary.scalar_differences, 0);
    }

    #[test]
    fn stable_digest_is_independent_of_insertion_order() {
        let first = map(&[("a", 1), ("b", 2)]);
        let second = map(&[("b", 2), ("a", 1)]);
        assert_eq!(digest_series(&first), digest_series(&second));
    }
}
