//! Relay quality tracking and management.
//!
//! The `RelayManager` is responsible for:
//! - Tracking per-relay statistics (events, novelty, uptime)
//! - Persisting stats to SQLite
//! - Computing quality scores
//! - Managing relay slot allocation (connect/disconnect decisions)

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::Connection;

use super::schema::{self, RelayStatus, RelayTier};
use super::scoring::{self, RelayStatsForScoring};
use super::url::{NormalizeResult, normalize_relay_url};
use crate::{Error, Result};

/// Configuration for the relay manager.
#[derive(Debug, Clone)]
pub struct RelayManagerConfig {
    /// Path to the SQLite database file.
    pub db_path: std::path::PathBuf,
    /// Maximum number of concurrent relay connections.
    pub max_relays: usize,
    /// How often to recompute scores and optimize connections (seconds).
    pub optimization_interval_secs: u64,
    /// Maximum percentage of relays to swap per optimization cycle.
    pub max_swap_percent: f64,
    /// Number of consecutive failures before blocking a relay.
    pub block_after_failures: u32,
    /// Number of "exploration" slots reserved for trying untested relays.
    /// These slots try relays that haven't been connected recently to avoid
    /// the optimizer getting stuck on the same high-scoring relays.
    pub exploration_slots: usize,
    /// Minimum time (seconds) since last connection before a relay is considered
    /// "unexplored" and eligible for exploration slots.
    pub exploration_min_age_secs: i64,
}

impl Default for RelayManagerConfig {
    fn default() -> Self {
        Self {
            db_path: std::path::PathBuf::from("./data/relay-stats.db"),
            max_relays: 100,
            optimization_interval_secs: 300, // 5 minutes
            max_swap_percent: 0.05,          // 5%
            block_after_failures: 10,
            exploration_slots: 3,            // Try 3 new relays per cycle
            exploration_min_age_secs: 86400, // 24 hours
        }
    }
}

/// Per-relay in-memory stats accumulator.
///
/// These are flushed to SQLite periodically.
#[derive(Debug, Default)]
struct RelayAccumulator {
    events_received: u64,
    events_novel: u64,
    events_duplicate: u64,
    connection_attempts: u32,
    connection_successes: u32,
    connection_seconds: u64,
    disconnects: u32,
}

/// Relay manager for quality tracking and slot optimization.
pub struct RelayManager {
    config: RelayManagerConfig,
    /// SQLite connection (protected by mutex for thread safety).
    conn: Mutex<Connection>,
    /// In-memory accumulators for the current hour.
    accumulators: Mutex<HashMap<String, RelayAccumulator>>,
    /// Current hour bucket start (Unix timestamp).
    current_hour: Mutex<i64>,
}

impl RelayManager {
    /// Open or create a relay manager with the given configuration.
    pub fn open(config: RelayManagerConfig) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = config.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }

        let conn = Connection::open(&config.db_path)
            .map_err(|e| Error::Database(format!("Failed to open SQLite: {}", e)))?;

        // Enable WAL mode for better concurrency
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| Error::Database(format!("Failed to set PRAGMA: {}", e)))?;

        // Initialize schema
        schema::init_schema(&conn)
            .map_err(|e| Error::Database(format!("Failed to init schema: {}", e)))?;

        let current_hour = Self::current_hour_start();

        Ok(Self {
            config,
            conn: Mutex::new(conn),
            accumulators: Mutex::new(HashMap::new()),
            current_hour: Mutex::new(current_hour),
        })
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let config = RelayManagerConfig {
            db_path: std::path::PathBuf::from(":memory:"),
            ..Default::default()
        };

        let conn = Connection::open_in_memory()
            .map_err(|e| Error::Database(format!("Failed to open in-memory SQLite: {}", e)))?;

        schema::init_schema(&conn)
            .map_err(|e| Error::Database(format!("Failed to init schema: {}", e)))?;

        let current_hour = Self::current_hour_start();

        Ok(Self {
            config,
            conn: Mutex::new(conn),
            accumulators: Mutex::new(HashMap::new()),
            current_hour: Mutex::new(current_hour),
        })
    }

    /// Get the configuration.
    pub fn config(&self) -> &RelayManagerConfig {
        &self.config
    }

    /// Register a relay (if not already known).
    ///
    /// The URL is normalized before registration to prevent duplicates from
    /// trailing slashes, case differences, etc. Returns an error if the URL
    /// is invalid or blocked.
    pub fn register_relay(&self, url: &str, tier: RelayTier) -> Result<()> {
        // Normalize the URL
        let normalized = match normalize_relay_url(url) {
            NormalizeResult::Ok(u) => u,
            NormalizeResult::Invalid(reason) => {
                return Err(Error::Validation(format!(
                    "Invalid relay URL '{}': {}",
                    url, reason
                )));
            }
            NormalizeResult::Blocked(reason) => {
                tracing::debug!("Blocked relay URL '{}': {}", url, reason);
                return Ok(()); // Silently skip blocked URLs
            }
        };

        let now = Self::unix_now();
        let conn = self.conn.lock();

        conn.execute(
            "INSERT OR IGNORE INTO relays (url, first_seen_at, tier, status)
             VALUES (?, ?, ?, ?)",
            rusqlite::params![normalized, now, tier.as_str(), RelayStatus::Pending.as_str()],
        )
        .map_err(|e| Error::Database(format!("Failed to register relay: {}", e)))?;

        Ok(())
    }

    /// Register multiple relays from a seed list.
    pub fn register_seed_relays(&self, urls: &[String]) -> Result<()> {
        for url in urls {
            self.register_relay(url, RelayTier::Seed)?;
        }
        Ok(())
    }

    /// Import relays from a JSON discovery results file.
    ///
    /// Expected format: JSON object with `functioning_relays` array of URL strings.
    /// This is the format produced by relay discovery tools.
    ///
    /// URLs are normalized and validated before import. Invalid or blocked URLs
    /// are silently skipped.
    pub fn import_from_json(&self, path: &Path) -> Result<usize> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "Failed to read JSON file: {}",
                e
            )))
        })?;

        // Parse as a generic JSON value first
        let json: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|e| Error::Serialization(format!("Failed to parse JSON: {}", e)))?;

        // Extract the functioning_relays array
        let relays = json
            .get("functioning_relays")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                Error::Serialization("JSON missing 'functioning_relays' array".to_string())
            })?;

        let mut count = 0;
        for relay in relays {
            if let Some(url) = relay.as_str() {
                // register_relay handles normalization and blocklist checking
                if self.register_relay(url, RelayTier::Discovered).is_ok() {
                    count += 1;
                }
            }
        }

        tracing::info!("Imported {} relays from JSON", count);
        Ok(count)
    }

    /// Load relay URLs from a JSON discovery results file.
    ///
    /// Returns a Vec of normalized relay URLs suitable for use as seed relays.
    /// This is a static method that doesn't require a RelayManager instance.
    /// Invalid and blocked URLs are silently filtered out.
    pub fn load_relays_from_json(path: &Path) -> Result<Vec<String>> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "Failed to read JSON file: {}",
                e
            )))
        })?;

        let json: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|e| Error::Serialization(format!("Failed to parse JSON: {}", e)))?;

        let relays = json
            .get("functioning_relays")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                Error::Serialization("JSON missing 'functioning_relays' array".to_string())
            })?;

        let urls: Vec<String> = relays
            .iter()
            .filter_map(|relay| relay.as_str())
            .filter_map(|url| normalize_relay_url(url).ok())
            .collect();

        Ok(urls)
    }

    /// Record an event received from a relay.
    ///
    /// Call this after the dedupe check to attribute novelty.
    pub fn record_event(&self, relay_url: &str, is_novel: bool) {
        self.maybe_flush_hour();

        let mut accumulators = self.accumulators.lock();
        let acc = accumulators.entry(relay_url.to_string()).or_default();

        acc.events_received += 1;
        if is_novel {
            acc.events_novel += 1;
        } else {
            acc.events_duplicate += 1;
        }
    }

    /// Record a connection attempt.
    pub fn record_connection_attempt(&self, relay_url: &str, success: bool) {
        self.maybe_flush_hour();

        let mut accumulators = self.accumulators.lock();
        let acc = accumulators.entry(relay_url.to_string()).or_default();

        acc.connection_attempts += 1;
        if success {
            acc.connection_successes += 1;
        }

        // Update relay status and failure count
        drop(accumulators);

        if let Err(e) = self.update_connection_status(relay_url, success) {
            tracing::warn!(
                "Failed to update connection status for {}: {}",
                relay_url,
                e
            );
        }
    }

    /// Record a disconnection.
    pub fn record_disconnect(&self, relay_url: &str, connected_seconds: u64) {
        self.maybe_flush_hour();

        let mut accumulators = self.accumulators.lock();
        let acc = accumulators.entry(relay_url.to_string()).or_default();

        acc.disconnects += 1;
        acc.connection_seconds += connected_seconds;
    }

    /// Update relay status after connection attempt.
    fn update_connection_status(&self, relay_url: &str, success: bool) -> Result<()> {
        let conn = self.conn.lock();
        let now = Self::unix_now();

        if success {
            conn.execute(
                "UPDATE relays SET status = ?, consecutive_failures = 0, last_connected_at = ?
                 WHERE url = ?",
                rusqlite::params![RelayStatus::Active.as_str(), now, relay_url],
            )
            .map_err(|e| Error::Database(e.to_string()))?;
        } else {
            // Increment failure count
            conn.execute(
                "UPDATE relays SET consecutive_failures = consecutive_failures + 1 WHERE url = ?",
                rusqlite::params![relay_url],
            )
            .map_err(|e| Error::Database(e.to_string()))?;

            // Check if we should block
            let failures: u32 = conn
                .query_row(
                    "SELECT consecutive_failures FROM relays WHERE url = ?",
                    rusqlite::params![relay_url],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            if failures >= self.config.block_after_failures {
                conn.execute(
                    "UPDATE relays SET status = ?, blocked_reason = ? WHERE url = ?",
                    rusqlite::params![
                        RelayStatus::Blocked.as_str(),
                        format!("Blocked after {} consecutive failures", failures),
                        relay_url
                    ],
                )
                .map_err(|e| Error::Database(e.to_string()))?;

                tracing::warn!(
                    "Blocked relay {} after {} consecutive failures",
                    relay_url,
                    failures
                );
            } else {
                conn.execute(
                    "UPDATE relays SET status = ? WHERE url = ?",
                    rusqlite::params![RelayStatus::Failing.as_str(), relay_url],
                )
                .map_err(|e| Error::Database(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Flush accumulators if the hour has changed.
    fn maybe_flush_hour(&self) {
        let current_hour = Self::current_hour_start();
        let mut hour_lock = self.current_hour.lock();

        if current_hour > *hour_lock {
            let old_hour = *hour_lock;
            *hour_lock = current_hour;
            drop(hour_lock);

            // Flush to database
            if let Err(e) = self.flush_accumulators(old_hour) {
                tracing::error!("Failed to flush hour stats: {}", e);
            }
        }
    }

    /// Flush in-memory accumulators to SQLite.
    fn flush_accumulators(&self, hour_start: i64) -> Result<()> {
        let mut accumulators = self.accumulators.lock();
        let stats: Vec<(String, RelayAccumulator)> = accumulators.drain().collect();
        drop(accumulators);

        if stats.is_empty() {
            return Ok(());
        }

        let conn = self.conn.lock();

        for (relay_url, acc) in stats {
            conn.execute(
                "INSERT INTO relay_stats_hourly
                 (relay_url, hour_start, events_received, events_novel, events_duplicate,
                  connection_attempts, connection_successes, connection_seconds, disconnects)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(relay_url, hour_start) DO UPDATE SET
                    events_received = events_received + excluded.events_received,
                    events_novel = events_novel + excluded.events_novel,
                    events_duplicate = events_duplicate + excluded.events_duplicate,
                    connection_attempts = connection_attempts + excluded.connection_attempts,
                    connection_successes = connection_successes + excluded.connection_successes,
                    connection_seconds = connection_seconds + excluded.connection_seconds,
                    disconnects = disconnects + excluded.disconnects",
                rusqlite::params![
                    relay_url,
                    hour_start,
                    acc.events_received as i64,
                    acc.events_novel as i64,
                    acc.events_duplicate as i64,
                    acc.connection_attempts as i64,
                    acc.connection_successes as i64,
                    acc.connection_seconds as i64,
                    acc.disconnects as i64,
                ],
            )
            .map_err(|e| Error::Database(format!("Failed to flush stats: {}", e)))?;
        }

        tracing::debug!("Flushed hourly stats for hour {}", hour_start);
        Ok(())
    }

    /// Force flush current accumulators (call on shutdown).
    pub fn flush(&self) -> Result<()> {
        let hour_start = *self.current_hour.lock();
        self.flush_accumulators(hour_start)
    }

    /// Compute and update scores for all relays.
    pub fn recompute_scores(&self) -> Result<()> {
        let now = Self::unix_now();
        let hour_ago = now - 3600;
        let day_ago = now - 86400;
        let week_ago = now - 604800;

        let conn = self.conn.lock();

        // Get all active/idle relays with their stats
        let mut stmt = conn
            .prepare(
                "SELECT r.url, r.tier,
                    COALESCE(h1.novel, 0) as novel_1h,
                    COALESCE(h24.novel, 0) as novel_24h,
                    COALESCE(h24.hours, 1) as hours_24h,
                    COALESCE(h7d.novel, 0) as novel_7d,
                    COALESCE(h7d.hours, 1) as hours_7d,
                    COALESCE(h24.attempts, 0) as attempts_24h,
                    COALESCE(h24.successes, 0) as successes_24h,
                    COALESCE(h7d.attempts, 0) as attempts_7d,
                    COALESCE(h7d.successes, 0) as successes_7d
                 FROM relays r
                 LEFT JOIN (
                    SELECT relay_url, SUM(events_novel) as novel
                    FROM relay_stats_hourly
                    WHERE hour_start >= ?
                    GROUP BY relay_url
                 ) h1 ON r.url = h1.relay_url
                 LEFT JOIN (
                    SELECT relay_url,
                           SUM(events_novel) as novel,
                           COUNT(DISTINCT hour_start) as hours,
                           SUM(connection_attempts) as attempts,
                           SUM(connection_successes) as successes
                    FROM relay_stats_hourly
                    WHERE hour_start >= ?
                    GROUP BY relay_url
                 ) h24 ON r.url = h24.relay_url
                 LEFT JOIN (
                    SELECT relay_url,
                           SUM(events_novel) as novel,
                           COUNT(DISTINCT hour_start) as hours,
                           SUM(connection_attempts) as attempts,
                           SUM(connection_successes) as successes
                    FROM relay_stats_hourly
                    WHERE hour_start >= ?
                    GROUP BY relay_url
                 ) h7d ON r.url = h7d.relay_url
                 WHERE r.status != 'blocked'",
            )
            .map_err(|e| Error::Database(e.to_string()))?;

        let rows: Vec<(String, RelayStatsForScoring)> = stmt
            .query_map(rusqlite::params![hour_ago, day_ago, week_ago], |row| {
                let url: String = row.get(0)?;
                let tier_str: String = row.get(1)?;
                let novel_1h: f64 = row.get(2)?;
                let novel_24h: f64 = row.get(3)?;
                let hours_24h: f64 = row.get(4)?;
                let novel_7d: f64 = row.get(5)?;
                let hours_7d: f64 = row.get(6)?;
                let attempts_24h: f64 = row.get(7)?;
                let successes_24h: f64 = row.get(8)?;
                let attempts_7d: f64 = row.get(9)?;
                let successes_7d: f64 = row.get(10)?;

                let tier = tier_str.parse().unwrap_or(RelayTier::Discovered);

                Ok((
                    url,
                    RelayStatsForScoring {
                        novel_rate_1h: novel_1h, // Already per-hour
                        novel_rate_24h: if hours_24h > 0.0 {
                            novel_24h / hours_24h
                        } else {
                            0.0
                        },
                        novel_rate_7d: if hours_7d > 0.0 {
                            novel_7d / hours_7d
                        } else {
                            0.0
                        },
                        uptime_24h: if attempts_24h > 0.0 {
                            successes_24h / attempts_24h
                        } else {
                            0.0
                        },
                        uptime_7d: if attempts_7d > 0.0 {
                            successes_7d / attempts_7d
                        } else {
                            0.0
                        },
                        tier,
                    },
                ))
            })
            .map_err(|e| Error::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        drop(stmt);

        // Compute median novel rate
        let mut novel_rates: Vec<f64> = rows.iter().map(|(_, s)| s.novel_rate_7d).collect();
        let median = scoring::compute_median(&mut novel_rates);

        // Compute and store scores
        for (url, stats) in &rows {
            let score = scoring::compute_score(stats, median);

            conn.execute(
                "INSERT INTO relay_scores
                 (relay_url, score, novel_rate_1h, novel_rate_24h, novel_rate_7d,
                  uptime_24h, uptime_7d, last_computed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(relay_url) DO UPDATE SET
                    score = excluded.score,
                    novel_rate_1h = excluded.novel_rate_1h,
                    novel_rate_24h = excluded.novel_rate_24h,
                    novel_rate_7d = excluded.novel_rate_7d,
                    uptime_24h = excluded.uptime_24h,
                    uptime_7d = excluded.uptime_7d,
                    last_computed_at = excluded.last_computed_at",
                rusqlite::params![
                    url,
                    score.score,
                    score.novel_rate_1h,
                    score.novel_rate_24h,
                    score.novel_rate_7d,
                    score.uptime_24h,
                    score.uptime_7d,
                    now,
                ],
            )
            .map_err(|e| Error::Database(e.to_string()))?;
        }

        tracing::debug!(
            "Recomputed scores for {} relays (median novel rate: {:.1}/h)",
            rows.len(),
            median
        );

        Ok(())
    }

    /// Get relays that should be connected, ordered by score.
    ///
    /// Returns up to `limit` relays, preferring:
    /// 1. Seed relays (always included)
    /// 2. Highest-scoring discovered relays
    pub fn get_relays_to_connect(&self, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock();

        let mut stmt = conn
            .prepare(
                "SELECT r.url FROM relays r
                 LEFT JOIN relay_scores s ON r.url = s.relay_url
                 WHERE r.status != 'blocked'
                 ORDER BY
                    CASE WHEN r.tier = 'seed' THEN 0 ELSE 1 END,
                    COALESCE(s.score, 0) DESC
                 LIMIT ?",
            )
            .map_err(|e| Error::Database(e.to_string()))?;

        let urls: Vec<String> = stmt
            .query_map([limit as i64], |row| row.get(0))
            .map_err(|e| Error::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(urls)
    }

    /// Get optimization suggestions: relays to disconnect and connect.
    ///
    /// Returns `(to_disconnect, to_connect, exploration_relays)` based on score comparisons.
    /// - `to_disconnect`: Low-scoring relays to disconnect
    /// - `to_connect`: High-scoring relays to connect in their place
    /// - `exploration_relays`: Untested/stale relays to try (exploration slots)
    pub fn get_optimization_suggestions(
        &self,
        currently_connected: &[String],
    ) -> Result<OptimizationSuggestions> {
        let conn = self.conn.lock();
        let now = Self::unix_now();

        // Get scores for connected relays (excluding seeds)
        let mut connected_scores: Vec<(String, f64)> = Vec::new();
        for url in currently_connected {
            let score: f64 = conn
                .query_row(
                    "SELECT COALESCE(s.score, 0), r.tier FROM relays r
                     LEFT JOIN relay_scores s ON r.url = s.relay_url
                     WHERE r.url = ?",
                    [url],
                    |row| {
                        let score: f64 = row.get(0)?;
                        let tier: String = row.get(1)?;
                        // Don't suggest disconnecting seed relays
                        if tier == "seed" {
                            Ok(f64::MAX)
                        } else {
                            Ok(score)
                        }
                    },
                )
                .unwrap_or(0.0);

            connected_scores.push((url.clone(), score));
        }

        // Sort connected by score (lowest first for disconnection candidates)
        connected_scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // Get highest-scoring unconnected relays
        let connected_set: std::collections::HashSet<&str> =
            currently_connected.iter().map(|s| s.as_str()).collect();

        let unconnected_scores: Vec<(String, f64)> = conn
            .prepare(
                "SELECT r.url, COALESCE(s.score, 0) as score FROM relays r
                 LEFT JOIN relay_scores s ON r.url = s.relay_url
                 WHERE r.status NOT IN ('blocked', 'active')
                 ORDER BY score DESC
                 LIMIT 100",
            )
            .map_err(|e| Error::Database(e.to_string()))?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .map_err(|e| Error::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .filter(|(url, _)| !connected_set.contains(url.as_str()))
            .collect();

        // Calculate how many swaps we can make (5% of connected)
        let max_swaps = ((currently_connected.len() as f64 * self.config.max_swap_percent).ceil()
            as usize)
            .max(1);

        let mut to_disconnect = Vec::new();
        let mut to_connect = Vec::new();

        // Compare lowest connected vs highest unconnected
        let threshold = 0.1; // Minimum score improvement to swap
        for i in 0..max_swaps
            .min(connected_scores.len())
            .min(unconnected_scores.len())
        {
            let (connected_url, connected_score) = &connected_scores[i];
            let (unconnected_url, unconnected_score) = &unconnected_scores[i];

            // Skip if connected is a seed (score = MAX)
            if *connected_score == f64::MAX {
                continue;
            }

            if unconnected_score > &(connected_score + threshold) {
                to_disconnect.push(connected_url.clone());
                to_connect.push(unconnected_url.clone());
            }
        }

        // =========================================================================
        // Exploration: Try relays that haven't been connected recently
        // =========================================================================
        let mut exploration_relays = Vec::new();
        if self.config.exploration_slots > 0 {
            let cutoff = now - self.config.exploration_min_age_secs;

            // Find relays that are:
            // 1. Not currently connected
            // 2. Not blocked
            // 3. Either never connected or last connected > 24h ago
            // Order randomly to avoid always picking the same ones
            let candidates: Vec<String> = conn
                .prepare(
                    "SELECT r.url FROM relays r
                     WHERE r.status NOT IN ('blocked', 'active')
                       AND (r.last_connected_at IS NULL OR r.last_connected_at < ?)
                     ORDER BY RANDOM()
                     LIMIT ?",
                )
                .map_err(|e| Error::Database(e.to_string()))?
                .query_map(
                    rusqlite::params![cutoff, self.config.exploration_slots as i64],
                    |row| row.get(0),
                )
                .map_err(|e| Error::Database(e.to_string()))?
                .filter_map(|r| r.ok())
                .filter(|url: &String| !connected_set.contains(url.as_str()))
                .filter(|url| !to_connect.contains(url))
                .collect();

            exploration_relays = candidates;
        }

        Ok(OptimizationSuggestions {
            to_disconnect,
            to_connect,
            exploration_relays,
        })
    }

    /// Get aggregate statistics for Prometheus metrics.
    pub fn get_aggregate_stats(&self) -> Result<AggregateRelayStats> {
        let conn = self.conn.lock();

        let total_relays: i64 = conn
            .query_row("SELECT COUNT(*) FROM relays", [], |row| row.get(0))
            .unwrap_or(0);

        let active_relays: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM relays WHERE status = 'active'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let blocked_relays: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM relays WHERE status = 'blocked'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let seed_relays: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM relays WHERE tier = 'seed'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let discovered_relays: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM relays WHERE tier = 'discovered'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Last hour stats
        let hour_ago = Self::unix_now() - 3600;
        let (events_novel, events_duplicate): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(events_novel), 0), COALESCE(SUM(events_duplicate), 0)
                 FROM relay_stats_hourly WHERE hour_start >= ?",
                [hour_ago],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or((0, 0));

        // Average score
        let avg_score: f64 = conn
            .query_row(
                "SELECT COALESCE(AVG(score), 0) FROM relay_scores",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0.0);

        Ok(AggregateRelayStats {
            total_relays: total_relays as u64,
            active_relays: active_relays as u64,
            blocked_relays: blocked_relays as u64,
            seed_relays: seed_relays as u64,
            discovered_relays: discovered_relays as u64,
            events_novel_1h: events_novel as u64,
            events_duplicate_1h: events_duplicate as u64,
            avg_score,
        })
    }

    /// Get the current Unix timestamp.
    fn unix_now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs() as i64
    }

    /// Get the start of the current hour (Unix timestamp).
    fn current_hour_start() -> i64 {
        let now = Self::unix_now();
        now - (now % 3600)
    }

    // =========================================================================
    // Checkpoint methods for catch-up processing
    // =========================================================================

    /// Get the last archived event timestamp for catch-up processing.
    ///
    /// Returns `None` if no checkpoint has been recorded yet (fresh start).
    pub fn get_last_archived_timestamp(&self) -> Result<Option<u64>> {
        let conn = self.conn.lock();

        let result: Option<i64> = conn
            .query_row(
                "SELECT value FROM ingestion_checkpoint WHERE key = 'last_archived_timestamp'",
                [],
                |row| row.get(0),
            )
            .ok();

        Ok(result.map(|v| v as u64))
    }

    /// Update the last archived event timestamp.
    ///
    /// Call this periodically during ingestion (e.g., after each segment seal)
    /// to track the high-water mark of successfully archived events.
    pub fn update_last_archived_timestamp(&self, timestamp: u64) -> Result<()> {
        let now = Self::unix_now();
        let conn = self.conn.lock();

        conn.execute(
            "INSERT INTO ingestion_checkpoint (key, value, updated_at)
             VALUES ('last_archived_timestamp', ?, ?)
             ON CONFLICT(key) DO UPDATE SET
                value = MAX(value, excluded.value),
                updated_at = excluded.updated_at",
            rusqlite::params![timestamp as i64, now],
        )
        .map_err(|e| Error::Database(format!("Failed to update checkpoint: {}", e)))?;

        Ok(())
    }

    /// Get all checkpoints (for debugging/introspection).
    pub fn get_checkpoints(&self) -> Result<Vec<(String, u64, i64)>> {
        let conn = self.conn.lock();

        let mut stmt = conn
            .prepare("SELECT key, value, updated_at FROM ingestion_checkpoint ORDER BY key")
            .map_err(|e| Error::Database(e.to_string()))?;

        let rows: Vec<(String, u64, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| Error::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }
}

/// Aggregate statistics for Prometheus metrics.
#[derive(Debug, Clone, Default)]
pub struct AggregateRelayStats {
    pub total_relays: u64,
    pub active_relays: u64,
    pub blocked_relays: u64,
    pub seed_relays: u64,
    pub discovered_relays: u64,
    pub events_novel_1h: u64,
    pub events_duplicate_1h: u64,
    pub avg_score: f64,
}

/// Optimization suggestions from the relay manager.
#[derive(Debug, Clone, Default)]
pub struct OptimizationSuggestions {
    /// Low-scoring relays to disconnect.
    pub to_disconnect: Vec<String>,
    /// High-scoring relays to connect in their place.
    pub to_connect: Vec<String>,
    /// Exploration relays: untested/stale relays to try.
    /// These help prevent the optimizer from getting stuck on the same relays.
    pub exploration_relays: Vec<String>,
}

impl OptimizationSuggestions {
    /// Check if there are any suggestions.
    pub fn is_empty(&self) -> bool {
        self.to_disconnect.is_empty()
            && self.to_connect.is_empty()
            && self.exploration_relays.is_empty()
    }

    /// Total number of new connections suggested.
    pub fn total_connects(&self) -> usize {
        self.to_connect.len() + self.exploration_relays.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let manager = RelayManager::open_in_memory().unwrap();
        assert_eq!(manager.config().max_relays, 100);
    }

    #[test]
    fn test_register_relay() {
        let manager = RelayManager::open_in_memory().unwrap();
        manager
            .register_relay("wss://relay.example.com", RelayTier::Seed)
            .unwrap();

        let relays = manager.get_relays_to_connect(10).unwrap();
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0], "wss://relay.example.com");
    }

    #[test]
    fn test_record_event() {
        let manager = RelayManager::open_in_memory().unwrap();
        manager
            .register_relay("wss://relay.example.com", RelayTier::Discovered)
            .unwrap();

        // Record some events
        manager.record_event("wss://relay.example.com", true);
        manager.record_event("wss://relay.example.com", true);
        manager.record_event("wss://relay.example.com", false);

        // Check accumulator
        let accumulators = manager.accumulators.lock();
        let acc = accumulators.get("wss://relay.example.com").unwrap();
        assert_eq!(acc.events_received, 3);
        assert_eq!(acc.events_novel, 2);
        assert_eq!(acc.events_duplicate, 1);
    }

    #[test]
    fn test_connection_blocking() {
        let config = RelayManagerConfig {
            block_after_failures: 3,
            ..Default::default()
        };

        let manager = RelayManager::open_in_memory().unwrap();
        // Override config for test
        let manager = RelayManager { config, ..manager };

        manager
            .register_relay("wss://bad.relay.com", RelayTier::Discovered)
            .unwrap();

        // Fail 3 times
        manager.record_connection_attempt("wss://bad.relay.com", false);
        manager.record_connection_attempt("wss://bad.relay.com", false);
        manager.record_connection_attempt("wss://bad.relay.com", false);

        // Should be blocked now
        let relays = manager.get_relays_to_connect(10).unwrap();
        assert!(relays.is_empty());
    }

    #[test]
    fn test_aggregate_stats() {
        let manager = RelayManager::open_in_memory().unwrap();
        manager
            .register_relay("wss://seed.relay.com", RelayTier::Seed)
            .unwrap();
        manager
            .register_relay("wss://discovered.relay.com", RelayTier::Discovered)
            .unwrap();

        let stats = manager.get_aggregate_stats().unwrap();
        assert_eq!(stats.total_relays, 2);
        assert_eq!(stats.seed_relays, 1);
    }

    #[test]
    fn test_checkpoint_fresh_db() {
        let manager = RelayManager::open_in_memory().unwrap();

        // Fresh database should have no checkpoint
        let checkpoint = manager.get_last_archived_timestamp().unwrap();
        assert_eq!(checkpoint, None);
    }

    #[test]
    fn test_checkpoint_update_and_read() {
        let manager = RelayManager::open_in_memory().unwrap();

        // Update checkpoint
        manager.update_last_archived_timestamp(1700000000).unwrap();

        // Read it back
        let checkpoint = manager.get_last_archived_timestamp().unwrap();
        assert_eq!(checkpoint, Some(1700000000));

        // Update to a higher value
        manager.update_last_archived_timestamp(1700001000).unwrap();
        let checkpoint = manager.get_last_archived_timestamp().unwrap();
        assert_eq!(checkpoint, Some(1700001000));
    }

    #[test]
    fn test_checkpoint_only_increases() {
        let manager = RelayManager::open_in_memory().unwrap();

        // Set initial checkpoint
        manager.update_last_archived_timestamp(1700001000).unwrap();

        // Try to update with a lower value - should be ignored (MAX semantics)
        manager.update_last_archived_timestamp(1700000000).unwrap();

        // Should still be the higher value
        let checkpoint = manager.get_last_archived_timestamp().unwrap();
        assert_eq!(checkpoint, Some(1700001000));
    }

    #[test]
    fn test_get_checkpoints() {
        let manager = RelayManager::open_in_memory().unwrap();

        // Initially empty
        let checkpoints = manager.get_checkpoints().unwrap();
        assert!(checkpoints.is_empty());

        // Add a checkpoint
        manager.update_last_archived_timestamp(1700000000).unwrap();

        // Should have one entry
        let checkpoints = manager.get_checkpoints().unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].0, "last_archived_timestamp");
        assert_eq!(checkpoints[0].1, 1700000000);
    }
}
