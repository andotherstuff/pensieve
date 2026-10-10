//! Latency and storage benchmark for the compact lookups.
//!
//! Every measured query runs with `optimize_use_projections = 0` and
//! `use_query_cache = 0`, uses keys not touched earlier in the run (a fresh
//! seed by default), and is tagged with a unique `log_comment` so server-side
//! duration and rows/bytes read come from `system.query_log`. `drop_caches`
//! additionally drops the mark and uncompressed caches before each query
//! (server-wide; opt-in). The OS page cache cannot be dropped from SQL, so
//! repeated runs may still be warmer than a cold production request.
//!
//! Key sampling reads through the projection and is not measured.

use std::collections::HashMap;
use std::future::Future;
use std::time::Instant;

use anyhow::{Context, Result};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::compact;
use super::ops::{self, TableParts};
use super::plan::projection_client;
use super::target::{D_TAG_EXPR, Target, ZAP_TARGETS_EXPR};
use crate::error::PreviewError;
use crate::query;

/// Benchmark options.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// Measured calls per lookup.
    pub samples: usize,
    /// Pubkeys per batch profile lookup (the JSON route uses up to 50).
    pub batch_size: usize,
    /// Sampling seed (`None`: derive from the server clock for fresh keys).
    pub seed: Option<u64>,
    /// Also measure the legacy reads (slow without the projection).
    pub include_legacy: bool,
    /// Legacy calls per lookup.
    pub legacy_samples: usize,
    /// `max_execution_time` for legacy calls.
    pub legacy_timeout_secs: u64,
    /// Drop mark and uncompressed caches before each measured query.
    pub drop_caches: bool,
    /// Latency target for the pass/fail summary.
    pub target_ms: u64,
    /// Sample keys through `events_by_kind`.
    pub require_projection: bool,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            samples: 30,
            batch_size: 50,
            seed: None,
            include_legacy: false,
            legacy_samples: 3,
            legacy_timeout_secs: 120,
            drop_caches: false,
            target_ms: 1_000,
            require_projection: true,
        }
    }
}

/// One measured call.
#[derive(Debug, Clone, Serialize)]
pub struct Measurement {
    /// Lookup name.
    pub lookup: &'static str,
    /// `compact` or `legacy`.
    pub source: &'static str,
    /// Key (first key for batches).
    pub key: String,
    /// Client wall time.
    pub wall_ms: f64,
    /// Error, if the call failed.
    pub error: Option<String>,
    /// `log_comment` tag.
    pub tag: String,
    /// Server-side duration from `system.query_log`.
    pub server_ms: Option<u64>,
    /// Rows read.
    pub read_rows: Option<u64>,
    /// Bytes read.
    pub read_bytes: Option<u64>,
}

/// Latency summary for one lookup and source.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Lookup name.
    pub lookup: &'static str,
    /// `compact` or `legacy`.
    pub source: &'static str,
    /// Calls.
    pub runs: usize,
    /// Failed calls.
    pub failures: usize,
    /// Wall-time percentiles (ms).
    pub wall_p50_ms: f64,
    /// Wall p95.
    pub wall_p95_ms: f64,
    /// Wall p99.
    pub wall_p99_ms: f64,
    /// Wall max.
    pub wall_max_ms: f64,
    /// Server p50 (ms).
    pub server_p50_ms: Option<u64>,
    /// Server max (ms).
    pub server_max_ms: Option<u64>,
    /// Max rows read.
    pub read_rows_max: Option<u64>,
    /// Max bytes read.
    pub read_bytes_max: Option<u64>,
    /// All calls succeeded and the slowest was under the target.
    pub under_target: bool,
}

/// Storage of one projection on `events_local`.
#[derive(Debug, Clone, Serialize, Deserialize, clickhouse::Row)]
pub struct ProjectionStorage {
    /// Projection name.
    pub name: String,
    /// Rows.
    pub rows: u64,
    /// Bytes on disk.
    pub bytes_on_disk: u64,
}

/// Storage comparison.
#[derive(Debug, Clone, Serialize)]
pub struct StorageReport {
    /// Compact tables.
    pub tables: Vec<(String, TableParts)>,
    /// `events_local` projections.
    pub projections: Vec<ProjectionStorage>,
}

/// Benchmark report.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// Run id (prefix of every `log_comment`).
    pub run_id: String,
    /// Sampling seed.
    pub seed: u64,
    /// Latency target.
    pub target_ms: u64,
    /// Whether caches were dropped before each query.
    pub drop_caches: bool,
    /// Settings applied to every measured query.
    pub settings: Vec<(String, String)>,
    /// Per-lookup summaries.
    pub summaries: Vec<Summary>,
    /// Raw measurements.
    pub measurements: Vec<Measurement>,
    /// Storage.
    pub storage: StorageReport,
}

const MEASURED_SETTINGS: [(&str, &str); 2] =
    [("optimize_use_projections", "0"), ("use_query_cache", "0")];

#[derive(Debug, Deserialize, clickhouse::Row)]
struct LogRow {
    log_comment: String,
    query_duration_ms: u64,
    read_rows: u64,
    read_bytes: u64,
}

struct Runner<'a> {
    client: &'a Client,
    run_id: String,
    options: &'a BenchOptions,
    measurements: Vec<Measurement>,
}

impl Runner<'_> {
    fn measured_client(&self, tag: &str, legacy: bool) -> Client {
        let mut client = self.client.clone();
        for (name, value) in MEASURED_SETTINGS {
            client = client.with_option(name, value);
        }
        client = client.with_option("log_comment", tag);
        if legacy {
            client = client
                .with_option(
                    "max_execution_time",
                    self.options.legacy_timeout_secs.to_string(),
                )
                .with_option("timeout_overflow_mode", "throw");
        }
        client
    }

    async fn measure<T, F, Fut>(
        &mut self,
        lookup: &'static str,
        legacy: bool,
        key: &str,
        call: F,
    ) -> Result<()>
    where
        F: FnOnce(Client) -> Fut,
        Fut: Future<Output = Result<T, PreviewError>>,
    {
        let source = if legacy { "legacy" } else { "compact" };
        let tag = format!(
            "{}:{lookup}:{source}:{}",
            self.run_id,
            self.measurements.len()
        );
        if self.options.drop_caches {
            for sql in ["SYSTEM DROP MARK CACHE", "SYSTEM DROP UNCOMPRESSED CACHE"] {
                self.client
                    .query(sql)
                    .execute()
                    .await
                    .with_context(|| sql.to_string())?;
            }
        }
        let client = self.measured_client(&tag, legacy);
        let started = Instant::now();
        let result = call(client).await;
        let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.measurements.push(Measurement {
            lookup,
            source,
            key: key.to_string(),
            wall_ms,
            error: result.err().map(|e| e.to_string()),
            tag,
            server_ms: None,
            read_rows: None,
            read_bytes: None,
        });
        Ok(())
    }
}

/// Run the benchmark. `client` should carry resource limits.
pub async fn run(client: &Client, database: &str, options: &BenchOptions) -> Result<BenchReport> {
    let now = ops::server_now(client).await?;
    let seed = options.seed.unwrap_or(u64::from(now));
    let run_id = format!("preview-lookups-bench-{now}-{seed}");
    let sampler = projection_client(client, options.require_projection);
    let n = options.samples.max(1);
    let batch = options.batch_size.max(1);

    let legacy_needed = if options.include_legacy {
        options.legacy_samples
    } else {
        0
    };
    let pubkeys: Vec<String> = sample_widening(
        &sampler,
        n + legacy_needed + n * 10 + n * batch,
        |permille, limit| {
            format!(
                "SELECT pubkey FROM {database}.events_local WHERE kind = 0 \
             AND cityHash64(pubkey, toUInt64({seed})) % 1000 < {permille} \
             GROUP BY pubkey ORDER BY cityHash64(pubkey, toUInt64({seed})) LIMIT {limit}"
            )
        },
    )
    .await
    .context("sampling pubkeys")?;
    let zap_targets: Vec<String> =
        sample_widening(&sampler, n + legacy_needed, |permille, limit| {
            format!(
                "SELECT target FROM (SELECT tags FROM {database}.events_local WHERE kind = 9735) \
             ARRAY JOIN {ZAP_TARGETS_EXPR} AS target \
             WHERE cityHash64(target, toUInt64({seed})) % 1000 < {permille} \
             GROUP BY target ORDER BY cityHash64(target, toUInt64({seed})) LIMIT {limit}"
            )
        })
        .await
        .context("sampling zap targets")?;
    let hot_zap_targets: Vec<String> = sampler
        .query(&format!(
            "SELECT target FROM (SELECT tags FROM {database}.events_local WHERE kind = 9735) \
             ARRAY JOIN {ZAP_TARGETS_EXPR} AS target \
             GROUP BY target ORDER BY count() DESC, target LIMIT {}",
            n.min(10)
        ))
        .fetch_all::<String>()
        .await
        .context("sampling hot zap targets")?;
    let addressable_kinds = Target::Addressable.kind_predicate();
    let coordinates: Vec<(String, u16, String)> =
        sample_widening(&sampler, n + legacy_needed, |permille, limit| {
            format!(
                "SELECT pubkey, kind, {D_TAG_EXPR} AS d FROM {database}.events_local \
                 WHERE {addressable_kinds} \
                 AND cityHash64(pubkey, kind, d, toUInt64({seed})) % 1000 < {permille} \
                 GROUP BY pubkey, kind, d \
                 ORDER BY cityHash64(pubkey, kind, d, toUInt64({seed})) LIMIT {limit}"
            )
        })
        .await
        .context("sampling naddr coordinates")?;

    let mut runner = Runner {
        client,
        run_id: run_id.clone(),
        options,
        measurements: Vec::new(),
    };

    // Disjoint key pools so no measured query reuses keys from another.
    let (singles, rest) = pubkeys.split_at(pubkeys.len().min(n));
    let (legacy_pool, rest) = rest.split_at(rest.len().min(legacy_needed));
    let (names_pool, batch_pool) = rest.split_at(rest.len().min(n * 10));
    if batch_pool.len() < n * batch {
        tracing::warn!(
            available = batch_pool.len(),
            wanted = n * batch,
            "fewer pubkeys than requested for batch lookups"
        );
    }

    for pk in singles {
        runner
            .measure("profile", false, pk, |c| async move {
                compact::fetch_profile(&c, pk).await
            })
            .await?;
    }
    for chunk in batch_pool.chunks(batch) {
        runner
            .measure("profiles_batch", false, &chunk[0], |c| async move {
                compact::fetch_profiles(&c, chunk).await
            })
            .await?;
    }
    for chunk in names_pool.chunks(10) {
        runner
            .measure("display_names", false, &chunk[0], |c| async move {
                compact::fetch_display_names(&c, chunk).await
            })
            .await?;
    }
    for target in zap_targets.iter().take(n) {
        runner
            .measure("zaps", false, target, |c| async move {
                compact::fetch_zap_totals(&c, target).await
            })
            .await?;
    }
    for target in &hot_zap_targets {
        runner
            .measure("zaps_hot", false, target, |c| async move {
                compact::fetch_zap_totals(&c, target).await
            })
            .await?;
    }
    for (pubkey, kind, d) in coordinates.iter().take(n) {
        let label = format!("{kind}:{pubkey}:{d}");
        runner
            .measure("naddr", false, &label, |c| async move {
                compact::fetch_replaceable(&c, *kind, pubkey, d).await
            })
            .await?;
    }

    if options.include_legacy {
        let m = options.legacy_samples;
        for pk in legacy_pool.iter().take(m) {
            runner
                .measure("profile", true, pk, |c| async move {
                    query::fetch_profile(&c, pk).await
                })
                .await?;
        }
        for target in zap_targets.iter().skip(n).take(m) {
            runner
                .measure("zaps", true, target, |c| async move {
                    query::fetch_zap_totals(&c, target).await
                })
                .await?;
        }
        for (pubkey, kind, d) in coordinates.iter().skip(n).take(m) {
            let label = format!("{kind}:{pubkey}:{d}");
            runner
                .measure("naddr", true, &label, |c| async move {
                    query::fetch_replaceable(&c, *kind, pubkey, d).await
                })
                .await?;
        }
    }

    let mut measurements = runner.measurements;
    attach_query_log(client, &run_id, &mut measurements).await?;
    let summaries = summarize(&measurements, options.target_ms);
    let storage = storage(client, database).await?;

    Ok(BenchReport {
        run_id,
        seed,
        target_ms: options.target_ms,
        drop_caches: options.drop_caches,
        settings: MEASURED_SETTINGS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        summaries,
        measurements,
        storage,
    })
}

/// Sample up to `limit` keys, widening the hashed fraction (per mille) until
/// enough keys are found or every key is eligible.
async fn sample_widening<T>(
    client: &Client,
    limit: usize,
    sql: impl Fn(u32, usize) -> String,
) -> Result<Vec<T>>
where
    T: clickhouse::Row + serde::de::DeserializeOwned,
{
    let mut keys = Vec::new();
    for permille in [5, 50, 500, 1000] {
        keys = client.query(&sql(permille, limit)).fetch_all::<T>().await?;
        if keys.len() >= limit {
            break;
        }
    }
    Ok(keys)
}

async fn attach_query_log(
    client: &Client,
    run_id: &str,
    measurements: &mut [Measurement],
) -> Result<()> {
    let expected = measurements.len();
    let mut rows: HashMap<String, LogRow> = HashMap::new();
    for _ in 0..30 {
        ops::flush_logs(client).await;
        rows = client
            .query(
                "SELECT log_comment, query_duration_ms, read_rows, read_bytes \
                 FROM system.query_log \
                 WHERE event_date >= yesterday() AND type != 'QueryStart' \
                 AND startsWith(log_comment, ?)",
            )
            .bind(format!("{run_id}:"))
            .fetch_all::<LogRow>()
            .await
            .context("reading system.query_log")?
            .into_iter()
            .map(|r| (r.log_comment.clone(), r))
            .collect();
        if rows.len() >= expected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    for m in measurements.iter_mut() {
        if let Some(row) = rows.get(&m.tag) {
            m.server_ms = Some(row.query_duration_ms);
            m.read_rows = Some(row.read_rows);
            m.read_bytes = Some(row.read_bytes);
        }
    }
    Ok(())
}

/// Nearest-rank percentile of sorted values.
fn percentile<T: Copy>(sorted: &[T], p: f64) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

fn summarize(measurements: &[Measurement], target_ms: u64) -> Vec<Summary> {
    let mut groups: Vec<((&'static str, &'static str), Vec<&Measurement>)> = Vec::new();
    for m in measurements {
        match groups.iter_mut().find(|(k, _)| *k == (m.lookup, m.source)) {
            Some((_, v)) => v.push(m),
            None => groups.push(((m.lookup, m.source), vec![m])),
        }
    }
    groups
        .into_iter()
        .map(|((lookup, source), ms)| {
            let mut wall: Vec<f64> = ms.iter().map(|m| m.wall_ms).collect();
            wall.sort_by(f64::total_cmp);
            let mut server: Vec<u64> = ms.iter().filter_map(|m| m.server_ms).collect();
            server.sort_unstable();
            let failures = ms.iter().filter(|m| m.error.is_some()).count();
            let wall_max = wall.last().copied().unwrap_or(0.0);
            Summary {
                lookup,
                source,
                runs: ms.len(),
                failures,
                wall_p50_ms: percentile(&wall, 50.0).unwrap_or(0.0),
                wall_p95_ms: percentile(&wall, 95.0).unwrap_or(0.0),
                wall_p99_ms: percentile(&wall, 99.0).unwrap_or(0.0),
                wall_max_ms: wall_max,
                server_p50_ms: percentile(&server, 50.0),
                server_max_ms: server.last().copied(),
                read_rows_max: ms.iter().filter_map(|m| m.read_rows).max(),
                read_bytes_max: ms.iter().filter_map(|m| m.read_bytes).max(),
                under_target: failures == 0 && wall_max < target_ms as f64,
            }
        })
        .collect()
}

async fn storage(client: &Client, database: &str) -> Result<StorageReport> {
    let mut tables = Vec::new();
    for target in Target::ALL {
        let parts = ops::table_parts(client, database, target.table()).await?;
        tables.push((target.table().to_string(), parts));
    }
    tables.push((
        "events_local".to_string(),
        ops::table_parts(client, database, "events_local").await?,
    ));
    let projections = client
        .query(
            "SELECT name, sum(rows) AS rows, sum(bytes_on_disk) AS bytes_on_disk \
             FROM system.projection_parts \
             WHERE database = ? AND table = 'events_local' AND active \
             GROUP BY name ORDER BY name",
        )
        .bind(database)
        .fetch_all::<ProjectionStorage>()
        .await
        .context("reading system.projection_parts")?;
    Ok(StorageReport {
        tables,
        projections,
    })
}

impl BenchReport {
    /// Text summary.
    pub fn render(&self) -> String {
        let mut out = format!(
            "run {} (seed {}), settings {:?}, drop caches: {}, target {} ms\n\n",
            self.run_id, self.seed, self.settings, self.drop_caches, self.target_ms
        );
        out.push_str(
            "lookup          source   runs fail  p50 ms   p95 ms   p99 ms   max ms   srv p50  srv max  max rows read  under target\n",
        );
        for s in &self.summaries {
            out.push_str(&format!(
                "{:<15} {:<8} {:>4} {:>4} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8} {:>8} {:>14} {}\n",
                s.lookup,
                s.source,
                s.runs,
                s.failures,
                s.wall_p50_ms,
                s.wall_p95_ms,
                s.wall_p99_ms,
                s.wall_max_ms,
                s.server_p50_ms
                    .map_or_else(|| "-".into(), |v| v.to_string()),
                s.server_max_ms
                    .map_or_else(|| "-".into(), |v| v.to_string()),
                s.read_rows_max
                    .map_or_else(|| "-".into(), |v| v.to_string()),
                if s.under_target { "yes" } else { "NO" },
            ));
        }
        out.push_str("\nstorage:\n");
        for (table, parts) in &self.storage.tables {
            out.push_str(&format!(
                "  {:<24} {:>14} rows {:>12} on disk ({} parts)\n",
                table,
                parts.rows,
                ops::human_bytes(parts.bytes_on_disk),
                parts.parts
            ));
        }
        for p in &self.storage.projections {
            out.push_str(&format!(
                "  events_local.{:<11} {:>14} rows {:>12} on disk (projection)\n",
                p.name,
                p.rows,
                ops::human_bytes(p.bytes_on_disk)
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(lookup: &'static str, wall_ms: f64, error: bool) -> Measurement {
        Measurement {
            lookup,
            source: "compact",
            key: String::new(),
            wall_ms,
            error: error.then(|| "boom".to_string()),
            tag: String::new(),
            server_ms: Some(wall_ms as u64),
            read_rows: Some(10),
            read_bytes: Some(100),
        }
    }

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 95.0), Some(95));
        assert_eq!(percentile(&v, 100.0), Some(100));
        assert_eq!(percentile(&[7u64], 99.0), Some(7));
        assert_eq!(percentile::<u64>(&[], 50.0), None);
    }

    #[test]
    fn summary_flags_failures_and_slow_calls() {
        let ms = vec![
            m("a", 10.0, false),
            m("a", 20.0, false),
            m("b", 5.0, true),
            m("c", 1500.0, false),
        ];
        let s = summarize(&ms, 1000);
        let get = |l: &str| s.iter().find(|x| x.lookup == l).unwrap();
        assert!(get("a").under_target);
        assert_eq!(get("a").runs, 2);
        assert_eq!(get("a").wall_max_ms, 20.0);
        assert!(!get("b").under_target);
        assert_eq!(get("b").failures, 1);
        assert!(!get("c").under_target);
    }
}
