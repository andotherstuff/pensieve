//! Authenticated, evidence-preserving comparison of two Pensieve API backends.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::Parser;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CACHE_CONTROL};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const RUNNER_VERSION: &str = "pensieve-api-cutover-comparison-v1";
const MAX_CASES: usize = 128;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_REQUEST_TIMEOUT_SECS: u64 = 30 * 60;
const ROUTE_CONTRACTS: [&str; 24] = [
    "/api/v1/stats",
    "/api/v1/stats/events/total",
    "/api/v1/stats/pubkeys/total",
    "/api/v1/stats/kinds/total",
    "/api/v1/stats/events/earliest",
    "/api/v1/stats/events/latest",
    "/api/v1/stats/events",
    "/api/v1/stats/throughput",
    "/api/v1/stats/users/active",
    "/api/v1/stats/users/active/daily",
    "/api/v1/stats/users/active/weekly",
    "/api/v1/stats/users/active/monthly",
    "/api/v1/stats/users/retention",
    "/api/v1/stats/users/new",
    "/api/v1/stats/activity/hourly",
    "/api/v1/stats/zaps",
    "/api/v1/stats/zaps/histogram",
    "/api/v1/stats/engagement",
    "/api/v1/stats/longform",
    "/api/v1/stats/publishers",
    "/api/v1/stats/relays/distribution",
    "/api/v1/kinds",
    "/api/v1/kinds/{kind}",
    "/api/v1/kinds/{kind}/activity",
];

#[derive(Debug, Parser)]
#[command(about = "Compare authenticated ClickHouse and Postgres API candidates")]
struct Args {
    /// Base URL of the isolated ClickHouse-backed candidate.
    #[arg(long)]
    clickhouse_base_url: String,
    /// Base URL of the isolated Postgres-backed candidate.
    #[arg(long)]
    postgres_base_url: String,
    /// Bearer token. Prefer the environment so it never appears in argv.
    #[arg(long, env = "PENSIEVE_API_COMPARISON_TOKEN", hide_env_values = true)]
    bearer_token: String,
    /// Explicit comparison-case manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Canonical no-replace evidence output.
    #[arg(long)]
    output: PathBuf,
    /// Atomically selected Postgres run.
    #[arg(long)]
    run_id: String,
    /// Atomically selected Postgres snapshot.
    #[arg(long)]
    snapshot_id: String,
    /// Published Postgres query version.
    #[arg(long)]
    query_version: String,
    /// Published Postgres as-of epoch.
    #[arg(long)]
    as_of_epoch: u64,
    /// Exact source revision of this comparator.
    #[arg(long)]
    code_version: String,
    /// Canonical lower-level evidence SHA-256 identities accepted by the gate.
    #[arg(long = "accepted-evidence-sha256")]
    accepted_evidence_sha256: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    path_and_query: String,
    #[serde(default = "default_request_timeout_secs")]
    request_timeout_secs: u64,
    #[serde(default = "ok_status")]
    expected_clickhouse_status: u16,
    #[serde(default = "ok_status")]
    expected_postgres_status: u16,
    policy: Policy,
}

const fn ok_status() -> u16 {
    200
}

const fn default_request_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "classification", rename_all = "snake_case", deny_unknown_fields)]
enum Policy {
    ExactMatch,
    AcceptedApproximation {
        max_relative_error_ppm: u64,
        numeric_fields: Vec<String>,
    },
    IntentionalCorrection {
        reason: String,
        evidence_sha256: String,
        variant_fields: Vec<String>,
    },
    ExpectedRejection,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    runner_version: &'static str,
    status: &'static str,
    run_id: String,
    snapshot_id: String,
    as_of_epoch: u64,
    query_version: String,
    code_version: String,
    manifest_sha256: String,
    accepted_evidence_sha256: Vec<String>,
    started_at_epoch_ms: u128,
    completed_at_epoch_ms: u128,
    cases: Vec<CaseEvidence>,
}

#[derive(Debug, Serialize)]
struct CaseEvidence {
    name: String,
    path_and_query: String,
    classification: &'static str,
    request_timeout_secs: u64,
    expected_clickhouse_status: u16,
    expected_postgres_status: u16,
    passed: bool,
    reason: Option<String>,
    max_absolute_difference: Option<f64>,
    max_relative_error_ppm: Option<u64>,
    clickhouse: Observation,
    postgres: Observation,
}

#[derive(Debug, Serialize)]
struct Observation {
    observed_at_epoch_ms: u128,
    elapsed_ms: u128,
    status: Option<u16>,
    canonical_body_sha256: Option<String>,
    body: Option<Value>,
    error: Option<String>,
}

#[derive(Default)]
struct NumericDifference {
    max_absolute: f64,
    max_relative_ppm: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    args.accepted_evidence_sha256.sort_unstable();
    args.accepted_evidence_sha256.dedup();
    validate_args(&args)?;
    let manifest_bytes = fs::read(&args.manifest).context("read comparison manifest")?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("parse comparison manifest")?;
    validate_manifest(&manifest, &args.accepted_evidence_sha256)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(MAX_REQUEST_TIMEOUT_SECS))
        .connect_timeout(std::time::Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build bounded HTTP client")?;
    let started_at_epoch_ms = now_epoch_ms()?;
    let mut case_evidence = Vec::with_capacity(manifest.cases.len());
    for case in manifest.cases {
        let clickhouse = observe(
            &client,
            &args.clickhouse_base_url,
            &case.path_and_query,
            &args.bearer_token,
            case.request_timeout_secs,
        )
        .await?;
        let postgres = observe(
            &client,
            &args.postgres_base_url,
            &case.path_and_query,
            &args.bearer_token,
            case.request_timeout_secs,
        )
        .await?;
        case_evidence.push(compare_case(case, clickhouse, postgres));
    }
    let passed = case_evidence.iter().all(|case| case.passed);
    let evidence = Evidence {
        schema_version: SCHEMA_VERSION,
        runner_version: RUNNER_VERSION,
        status: if passed { "passed" } else { "failed" },
        run_id: args.run_id,
        snapshot_id: args.snapshot_id,
        as_of_epoch: args.as_of_epoch,
        query_version: args.query_version,
        code_version: args.code_version,
        manifest_sha256: sha256_hex(&manifest_bytes),
        accepted_evidence_sha256: args.accepted_evidence_sha256,
        started_at_epoch_ms,
        completed_at_epoch_ms: now_epoch_ms()?,
        cases: case_evidence,
    };
    publish_noclobber(&args.output, &evidence)?;
    let evidence_sha256 = sha256_hex(&fs::read(&args.output)?);
    println!(
        "{}",
        serde_json::json!({
            "status": evidence.status,
            "case_count": evidence.cases.len(),
            "evidence_sha256": evidence_sha256,
        })
    );
    if !passed {
        bail!("one or more API cutover comparisons failed");
    }
    Ok(())
}

fn validate_args(args: &Args) -> anyhow::Result<()> {
    if args.bearer_token.is_empty() {
        bail!("bearer token cannot be empty");
    }
    validate_base_url(&args.clickhouse_base_url)?;
    validate_base_url(&args.postgres_base_url)?;
    if args.clickhouse_base_url.trim_end_matches('/')
        == args.postgres_base_url.trim_end_matches('/')
    {
        bail!("ClickHouse and Postgres candidates must use different origins");
    }
    if args.run_id.is_empty() || args.query_version.is_empty() || args.code_version.is_empty() {
        bail!("run, query-version, and code-version identities must be nonempty");
    }
    if !args.snapshot_id.starts_with("sha256:") {
        bail!("snapshot ID must be a sha256 identity");
    }
    validate_sha256(&args.snapshot_id[7..])?;
    for sha256 in &args.accepted_evidence_sha256 {
        validate_sha256(sha256)?;
    }
    Ok(())
}

fn validate_base_url(value: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(value).context("parse candidate base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        bail!("candidate base URL must be a credential-free HTTP(S) origin");
    }
    Ok(())
}

fn validate_manifest(manifest: &Manifest, accepted: &[String]) -> anyhow::Result<()> {
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.cases.is_empty()
        || manifest.cases.len() > MAX_CASES
    {
        bail!("manifest must use schema version 1 and contain 1 through {MAX_CASES} cases");
    }
    let mut names = BTreeSet::new();
    let mut covered_routes = BTreeSet::new();
    for case in &manifest.cases {
        if case.name.is_empty() || !names.insert(&case.name) {
            bail!("comparison case names must be nonempty and unique");
        }
        if case.request_timeout_secs == 0 || case.request_timeout_secs > MAX_REQUEST_TIMEOUT_SECS {
            bail!("request timeout must be between 1 and {MAX_REQUEST_TIMEOUT_SECS} seconds");
        }
        if !case.path_and_query.starts_with("/api/v1/")
            || case.path_and_query.contains('#')
            || case.path_and_query.chars().any(char::is_whitespace)
        {
            bail!("comparison paths must be canonical /api/v1 paths without fragments");
        }
        StatusCode::from_u16(case.expected_clickhouse_status)
            .context("invalid expected ClickHouse HTTP status")?;
        let expected_postgres_status = StatusCode::from_u16(case.expected_postgres_status)
            .context("invalid expected Postgres HTTP status")?;
        let path = case.path_and_query.split('?').next().unwrap_or_default();
        let Some(contract) = route_contract(path) else {
            bail!("comparison case does not map to one of the 24 analytics routes");
        };
        covered_routes.insert(contract);
        match &case.policy {
            Policy::AcceptedApproximation {
                max_relative_error_ppm,
                numeric_fields: _,
            } if *max_relative_error_ppm == 0 || *max_relative_error_ppm > 1_000_000 => {
                bail!("approximation tolerance must be between 1 and 1000000 ppm");
            }
            Policy::AcceptedApproximation { numeric_fields, .. } => {
                if numeric_fields.is_empty()
                    || !numeric_fields.windows(2).all(|pair| pair[0] < pair[1])
                    || numeric_fields.iter().any(|field| {
                        field.is_empty()
                            || !field
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                    })
                {
                    bail!(
                        "approximation numeric fields must be nonempty, lowercase, sorted, and unique"
                    );
                }
            }
            Policy::IntentionalCorrection {
                reason,
                evidence_sha256,
                variant_fields,
            } => {
                if reason.trim().is_empty() {
                    bail!("intentional corrections require a nonempty reason");
                }
                validate_field_names(variant_fields, "intentional correction variant fields")?;
                validate_sha256(evidence_sha256)?;
                if !accepted.contains(evidence_sha256) {
                    bail!("intentional correction references unaccepted evidence");
                }
            }
            _ => {}
        }
        if matches!(&case.policy, Policy::ExpectedRejection) {
            if !expected_postgres_status.is_client_error() {
                bail!("expected rejections require a Postgres 4xx status");
            }
        } else if case.expected_clickhouse_status != case.expected_postgres_status
            || !expected_postgres_status.is_success()
        {
            bail!("non-rejection policies require matching successful statuses");
        }
    }
    let required_routes = ROUTE_CONTRACTS.into_iter().collect::<BTreeSet<_>>();
    if covered_routes != required_routes {
        let missing = required_routes
            .difference(&covered_routes)
            .copied()
            .collect::<Vec<_>>();
        bail!("comparison manifest does not cover all 24 analytics routes; missing={missing:?}");
    }
    Ok(())
}

fn route_contract(path: &str) -> Option<&'static str> {
    if let Some(contract) = ROUTE_CONTRACTS
        .into_iter()
        .find(|contract| !contract.contains('{') && *contract == path)
    {
        return Some(contract);
    }
    let components = path.split('/').collect::<Vec<_>>();
    match components.as_slice() {
        ["", "api", "v1", "kinds", kind] if kind.parse::<u16>().is_ok() => {
            Some("/api/v1/kinds/{kind}")
        }
        ["", "api", "v1", "kinds", kind, "activity"] if kind.parse::<u16>().is_ok() => {
            Some("/api/v1/kinds/{kind}/activity")
        }
        _ => None,
    }
}

async fn observe(
    client: &reqwest::Client,
    base_url: &str,
    path_and_query: &str,
    bearer_token: &str,
    request_timeout_secs: u64,
) -> anyhow::Result<Observation> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);
    let started = Instant::now();
    let observed_at_epoch_ms = now_epoch_ms()?;
    let response = match client
        .get(url)
        .timeout(std::time::Duration::from_secs(request_timeout_secs))
        .header(AUTHORIZATION, format!("Bearer {bearer_token}"))
        .header(CACHE_CONTROL, "no-cache")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return Ok(Observation {
                observed_at_epoch_ms,
                elapsed_ms: started.elapsed().as_millis(),
                status: None,
                canonical_body_sha256: None,
                body: None,
                error: Some(format!("request failed: {error}")),
            });
        }
    };
    let status = response.status().as_u16();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Observation {
                observed_at_epoch_ms,
                elapsed_ms: started.elapsed().as_millis(),
                status: Some(status),
                canonical_body_sha256: None,
                body: None,
                error: Some(format!("response body failed: {error}")),
            });
        }
    };
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Ok(Observation {
            observed_at_epoch_ms,
            elapsed_ms: started.elapsed().as_millis(),
            status: Some(status),
            canonical_body_sha256: None,
            body: None,
            error: Some(format!(
                "response exceeds the {MAX_RESPONSE_BYTES}-byte evidence bound"
            )),
        });
    }
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            return Ok(Observation {
                observed_at_epoch_ms,
                elapsed_ms: started.elapsed().as_millis(),
                status: Some(status),
                canonical_body_sha256: Some(sha256_hex(&bytes)),
                body: None,
                error: Some(format!("response is not JSON: {error}")),
            });
        }
    };
    let canonical = serde_json::to_vec(&body)?;
    Ok(Observation {
        observed_at_epoch_ms,
        elapsed_ms: started.elapsed().as_millis(),
        status: Some(status),
        canonical_body_sha256: Some(sha256_hex(&canonical)),
        body: Some(body),
        error: None,
    })
}

fn compare_case(case: Case, clickhouse: Observation, postgres: Observation) -> CaseEvidence {
    let statuses_match = clickhouse.status == Some(case.expected_clickhouse_status)
        && postgres.status == Some(case.expected_postgres_status);
    let (classification, body_passed, reason, difference) = match case.policy {
        Policy::ExactMatch => (
            "exact_match",
            clickhouse.body.is_some()
                && clickhouse.canonical_body_sha256 == postgres.canonical_body_sha256,
            None,
            None,
        ),
        Policy::AcceptedApproximation {
            max_relative_error_ppm,
            numeric_fields,
        } => {
            let mut difference = NumericDifference::default();
            let numeric_fields = numeric_fields
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let compatible = clickhouse
                .body
                .as_ref()
                .zip(postgres.body.as_ref())
                .is_some_and(|(clickhouse, postgres)| {
                    compare_approximate(
                        clickhouse,
                        postgres,
                        max_relative_error_ppm,
                        &numeric_fields,
                        None,
                        &mut difference,
                    )
                });
            ("accepted_approximation", compatible, None, Some(difference))
        }
        Policy::IntentionalCorrection {
            reason,
            evidence_sha256,
            variant_fields,
        } => {
            let variant_fields = variant_fields
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let compatible = clickhouse
                .body
                .as_ref()
                .zip(postgres.body.as_ref())
                .is_some_and(|(clickhouse, postgres)| {
                    compare_intentional_correction(clickhouse, postgres, &variant_fields, None)
                });
            (
                "intentional_correction",
                compatible,
                Some(format!("{reason}; evidence_sha256={evidence_sha256}")),
                None,
            )
        }
        Policy::ExpectedRejection => (
            "expected_rejection",
            clickhouse.body.is_some() && postgres.body.is_some(),
            None,
            None,
        ),
    };
    CaseEvidence {
        name: case.name,
        path_and_query: case.path_and_query,
        classification,
        request_timeout_secs: case.request_timeout_secs,
        expected_clickhouse_status: case.expected_clickhouse_status,
        expected_postgres_status: case.expected_postgres_status,
        passed: statuses_match && body_passed,
        reason,
        max_absolute_difference: difference.as_ref().map(|value| value.max_absolute),
        max_relative_error_ppm: difference.map(|value| value.max_relative_ppm),
        clickhouse,
        postgres,
    }
}

fn compare_approximate(
    left: &Value,
    right: &Value,
    tolerance_ppm: u64,
    numeric_fields: &BTreeSet<&str>,
    field: Option<&str>,
    difference: &mut NumericDifference,
) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            if !field.is_some_and(|field| numeric_fields.contains(field)) {
                return left == right;
            }
            let Some((absolute, ppm)) = numeric_difference(left, right) else {
                return false;
            };
            difference.max_absolute = difference.max_absolute.max(absolute);
            difference.max_relative_ppm = difference.max_relative_ppm.max(ppm);
            ppm <= tolerance_ppm
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    compare_approximate(
                        left,
                        right,
                        tolerance_ppm,
                        numeric_fields,
                        field,
                        difference,
                    )
                })
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right.get(key).is_some_and(|right| {
                        compare_approximate(
                            left,
                            right,
                            tolerance_ppm,
                            numeric_fields,
                            Some(key),
                            difference,
                        )
                    })
                })
        }
        _ => left == right,
    }
}

fn numeric_difference(left: &serde_json::Number, right: &serde_json::Number) -> Option<(f64, u64)> {
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        let absolute = (i128::from(left) - i128::from(right)).unsigned_abs();
        let denominator = i128::from(left).unsigned_abs();
        return Some((absolute as f64, relative_ppm(absolute, denominator)));
    }
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
        let absolute = u128::from(left.abs_diff(right));
        return Some((absolute as f64, relative_ppm(absolute, u128::from(left))));
    }
    let (left, right) = (left.as_f64()?, right.as_f64()?);
    let absolute = (left - right).abs();
    let ppm = if left == 0.0 && right == 0.0 {
        0
    } else if left == 0.0 {
        1_000_000
    } else {
        ((absolute / left.abs()) * 1_000_000.0).ceil() as u64
    };
    Some((absolute, ppm))
}

fn relative_ppm(absolute: u128, denominator: u128) -> u64 {
    if absolute == 0 {
        return 0;
    }
    if denominator == 0 {
        return 1_000_000;
    }
    let numerator = absolute * 1_000_000;
    let rounded_up = numerator.div_ceil(denominator);
    u64::try_from(rounded_up).unwrap_or(u64::MAX)
}

fn compatible_shape(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_)) => true,
        (Value::Array(left), Value::Array(right)) => match (left.first(), right.first()) {
            (Some(left), Some(right)) => compatible_shape(left, right),
            (None, None) => true,
            _ => false,
        },
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| compatible_shape(left, right))
                })
        }
        _ => false,
    }
}

fn compare_intentional_correction(
    left: &Value,
    right: &Value,
    variant_fields: &BTreeSet<&str>,
    field: Option<&str>,
) -> bool {
    if field.is_some_and(|field| variant_fields.contains(field)) {
        return compatible_shape(left, right);
    }
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    compare_intentional_correction(left, right, variant_fields, field)
                })
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right.get(key).is_some_and(|right| {
                        compare_intentional_correction(left, right, variant_fields, Some(key))
                    })
                })
        }
        _ => left == right,
    }
}

fn validate_field_names(fields: &[String], label: &str) -> anyhow::Result<()> {
    if fields.is_empty()
        || !fields.windows(2).all(|pair| pair[0] < pair[1])
        || fields.iter().any(|field| {
            field.is_empty()
                || !field
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        })
    {
        bail!("{label} must be nonempty, lowercase, sorted, and unique");
    }
    Ok(())
}

fn publish_noclobber(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("create immutable comparison evidence")?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn validate_sha256(value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("SHA-256 identities must contain 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_epoch_ms() -> anyhow::Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time predates Unix epoch")?
        .as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_manifest() -> Manifest {
        Manifest {
            schema_version: 1,
            cases: ROUTE_CONTRACTS
                .into_iter()
                .enumerate()
                .map(|(index, contract)| Case {
                    name: format!("case-{index}"),
                    path_and_query: contract.replace("{kind}", "1"),
                    request_timeout_secs: default_request_timeout_secs(),
                    expected_clickhouse_status: 200,
                    expected_postgres_status: 200,
                    policy: Policy::ExactMatch,
                })
                .collect(),
        }
    }

    #[test]
    fn approximate_comparison_is_recursive_and_bounded() {
        let left = serde_json::json!({"rows": [{"count": 1000, "name": "x"}]});
        let right = serde_json::json!({"rows": [{"count": 1019, "name": "x"}]});
        let numeric_fields = BTreeSet::from(["count"]);
        let mut difference = NumericDifference::default();
        assert!(compare_approximate(
            &left,
            &right,
            20_000,
            &numeric_fields,
            None,
            &mut difference
        ));
        assert_eq!(difference.max_relative_ppm, 19_000);
        let mut difference = NumericDifference::default();
        assert!(!compare_approximate(
            &left,
            &right,
            10_000,
            &numeric_fields,
            None,
            &mut difference
        ));
    }

    #[test]
    fn approximation_keeps_large_integer_precision() {
        let left = serde_json::json!(18_446_744_073_709_551_000_u64);
        let right = serde_json::json!(18_446_744_073_709_551_001_u64);
        let numeric_fields = BTreeSet::from(["count"]);
        let mut difference = NumericDifference::default();
        assert!(compare_approximate(
            &left,
            &right,
            1,
            &numeric_fields,
            Some("count"),
            &mut difference
        ));
        assert_eq!(difference.max_relative_ppm, 1);
    }

    #[test]
    fn approximation_rejects_drift_in_unlisted_numbers() {
        let left = serde_json::json!({"unique_pubkeys": 1000, "event_count": 2000});
        let right = serde_json::json!({"unique_pubkeys": 1010, "event_count": 2001});
        let numeric_fields = BTreeSet::from(["unique_pubkeys"]);
        let mut difference = NumericDifference::default();
        assert!(!compare_approximate(
            &left,
            &right,
            20_000,
            &numeric_fields,
            None,
            &mut difference
        ));
    }

    #[test]
    fn intentional_correction_varies_only_named_fields() {
        let fields = BTreeSet::from(["count"]);
        assert!(compare_intentional_correction(
            &serde_json::json!({"rows": [{"count": 1, "period": "x"}]}),
            &serde_json::json!({"rows": [{"count": 9000, "period": "x"}]}),
            &fields,
            None,
        ));
        assert!(!compare_intentional_correction(
            &serde_json::json!({"rows": [{"count": 1, "period": "x"}]}),
            &serde_json::json!({"rows": [{"count": 2, "period": "y"}]}),
            &fields,
            None,
        ));
        assert!(!compare_intentional_correction(
            &serde_json::json!({"rows": [{"count": 1}]}),
            &serde_json::json!({"rows": [{"count": 2}, {"count": 3}]}),
            &fields,
            None,
        ));
    }

    #[test]
    fn manifest_rejects_unaccepted_correction_evidence() {
        let mut manifest = complete_manifest();
        manifest.cases[0].policy = Policy::IntentionalCorrection {
            reason: "canonical population".into(),
            evidence_sha256: "a".repeat(64),
            variant_fields: vec!["count".into()],
        };
        assert!(validate_manifest(&manifest, &["b".repeat(64)]).is_err());
        assert!(validate_manifest(&manifest, &["a".repeat(64)]).is_ok());
    }

    #[test]
    fn expected_rejection_supports_backend_specific_statuses() {
        let mut manifest = complete_manifest();
        manifest.cases[0].policy = Policy::ExpectedRejection;
        manifest.cases[0].expected_postgres_status = 400;
        assert!(validate_manifest(&manifest, &[]).is_ok());

        manifest.cases[0].expected_postgres_status = 200;
        assert!(validate_manifest(&manifest, &[]).is_err());
        manifest.cases[0].policy = Policy::ExactMatch;
        manifest.cases[0].expected_postgres_status = 201;
        assert!(validate_manifest(&manifest, &[]).is_err());
    }

    #[test]
    fn request_timeouts_are_explicitly_bounded() {
        let mut manifest = complete_manifest();
        manifest.cases[0].request_timeout_secs = MAX_REQUEST_TIMEOUT_SECS;
        assert!(validate_manifest(&manifest, &[]).is_ok());
        manifest.cases[0].request_timeout_secs = 0;
        assert!(validate_manifest(&manifest, &[]).is_err());
        manifest.cases[0].request_timeout_secs = MAX_REQUEST_TIMEOUT_SECS + 1;
        assert!(validate_manifest(&manifest, &[]).is_err());
    }

    #[test]
    fn manifest_requires_all_24_route_contracts() {
        let mut manifest = complete_manifest();
        assert!(validate_manifest(&manifest, &[]).is_ok());
        manifest.cases.pop();
        assert!(validate_manifest(&manifest, &[]).is_err());
    }

    #[test]
    fn checked_in_manifest_covers_every_route_fail_closed() {
        let manifest: Manifest = serde_json::from_str(include_str!(
            "../../../../ops/api-cutover-cases.example.json"
        ))
        .expect("parse checked-in comparison manifest");
        assert!(manifest.cases.len() > 24);
        assert!(validate_manifest(&manifest, &[]).is_ok());
        assert!(
            manifest
                .cases
                .iter()
                .take(24)
                .all(|case| matches!(case.policy, Policy::ExactMatch))
        );
    }

    #[test]
    fn transport_failure_is_a_failed_case_not_missing_evidence() {
        let failed = Observation {
            observed_at_epoch_ms: 1,
            elapsed_ms: 2,
            status: None,
            canonical_body_sha256: None,
            body: None,
            error: Some("request failed".into()),
        };
        let ok = Observation {
            observed_at_epoch_ms: 1,
            elapsed_ms: 2,
            status: Some(200),
            canonical_body_sha256: Some("a".repeat(64)),
            body: Some(serde_json::json!({"count": 1})),
            error: None,
        };
        let evidence = compare_case(
            Case {
                name: "total".into(),
                path_and_query: "/api/v1/stats/events/total".into(),
                request_timeout_secs: default_request_timeout_secs(),
                expected_clickhouse_status: 200,
                expected_postgres_status: 200,
                policy: Policy::ExactMatch,
            },
            failed,
            ok,
        );
        assert!(!evidence.passed);
        assert_eq!(evidence.clickhouse.error.as_deref(), Some("request failed"));
    }
}
