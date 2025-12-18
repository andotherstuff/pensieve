//! Prometheus metrics helpers for the Pensieve system.
//!
//! This module provides centralized metrics initialization and common metric
//! definitions used across Pensieve components.
//!
//! # Usage
//!
//! ```rust,ignore
//! use pensieve_core::metrics::{init_metrics, start_metrics_server};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Initialize the Prometheus recorder
//!     let handle = init_metrics();
//!
//!     // Start the HTTP server for /metrics endpoint
//!     start_metrics_server(9091, handle).await.unwrap();
//!
//!     // Now use metrics anywhere in your code
//!     use metrics::{counter, gauge};
//!     counter!("my_counter").increment(1);
//!     gauge!("my_gauge").set(42.0);
//! }
//! ```
//!
//! # Metric Naming Conventions
//!
//! All Pensieve metrics follow these conventions:
//! - Prefix: Component name (e.g., `backfill_`, `ingest_`, `clickhouse_`)
//! - Suffix: Unit or type (e.g., `_total`, `_bytes`, `_seconds`)
//! - Labels: Use sparingly to avoid cardinality explosion

use axum::{Router, routing::get};
use metrics::{describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;

/// Initialize the Prometheus metrics recorder.
///
/// This must be called once at startup before any metrics are recorded.
/// Returns a handle that can be used with [`start_metrics_server`].
///
/// # Panics
///
/// Panics if called more than once (the recorder can only be installed once).
pub fn init_metrics() -> PrometheusHandle {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    // Register all metric descriptions upfront
    register_common_metrics();

    handle
}

/// Try to initialize the Prometheus metrics recorder.
///
/// Like [`init_metrics`] but returns `None` if the recorder is already installed,
/// instead of panicking. Useful for tests or optional metrics.
pub fn try_init_metrics() -> Option<PrometheusHandle> {
    PrometheusBuilder::new().install_recorder().ok()
}

/// Start the Prometheus metrics HTTP server.
///
/// Serves the `/metrics` endpoint on the specified port.
/// This spawns a background task and returns immediately.
///
/// # Arguments
///
/// * `port` - TCP port to listen on (e.g., 9091)
/// * `handle` - Prometheus handle from [`init_metrics`]
pub async fn start_metrics_server(
    port: u16,
    handle: PrometheusHandle,
) -> Result<(), std::io::Error> {
    let app = Router::new().route(
        "/metrics",
        get(move || {
            let handle = handle.clone();
            async move { handle.render() }
        }),
    );

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Metrics server listening on http://{}/metrics", addr);

    // Spawn the server in the background
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    Ok(())
}

/// Register descriptions for common metrics used across Pensieve.
///
/// Called automatically by [`init_metrics`].
fn register_common_metrics() {
    // =========================================================================
    // Ingestion / Backfill Metrics
    // =========================================================================

    describe_counter!(
        "backfill_events_total",
        "Total number of events processed during backfill"
    );
    describe_counter!(
        "backfill_events_valid_total",
        "Number of valid events written to archive during backfill"
    );
    describe_counter!(
        "backfill_events_duplicate_total",
        "Number of duplicate events skipped during backfill"
    );
    describe_counter!(
        "backfill_events_invalid_total",
        "Number of invalid events (proto or validation errors) during backfill"
    );
    describe_counter!(
        "backfill_files_total",
        "Total number of source files processed during backfill"
    );
    describe_counter!(
        "backfill_segments_sealed_total",
        "Number of archive segments sealed during backfill"
    );
    describe_counter!(
        "backfill_bytes_total",
        "Total bytes processed during backfill (label: type)"
    );
    describe_gauge!(
        "backfill_events_per_second",
        "Current backfill processing rate (events/sec)"
    );
    describe_gauge!(
        "backfill_running",
        "Whether a backfill process is currently running (1=yes, 0=no)"
    );

    // =========================================================================
    // Live Ingestion Metrics (for future relay ingester)
    // =========================================================================

    describe_counter!("ingest_events_total", "Total events received from relays");
    describe_counter!(
        "ingest_events_valid_total",
        "Valid events written to archive"
    );
    describe_counter!("ingest_events_duplicate_total", "Duplicate events skipped");
    describe_counter!("ingest_events_invalid_total", "Invalid events rejected");
    describe_gauge!(
        "ingest_relay_connections",
        "Number of active relay connections"
    );
    describe_gauge!(
        "ingest_events_per_second",
        "Current ingestion rate (events/sec)"
    );

    // =========================================================================
    // Segment Writer Metrics
    // =========================================================================

    describe_counter!(
        "segment_events_written_total",
        "Total events written to segments"
    );
    describe_counter!(
        "segment_bytes_written_total",
        "Total uncompressed bytes written to segments"
    );
    describe_counter!(
        "segment_bytes_compressed_total",
        "Total compressed bytes written to segments"
    );
    describe_counter!("segment_sealed_total", "Number of segments sealed");
    describe_gauge!(
        "segment_current_events",
        "Events in the current unsealed segment"
    );
    describe_gauge!(
        "segment_current_bytes",
        "Bytes in the current unsealed segment"
    );

    // =========================================================================
    // Dedupe Index Metrics
    // =========================================================================

    describe_gauge!(
        "dedupe_keys_approximate",
        "Approximate number of event IDs in the dedupe index"
    );
    describe_counter!("dedupe_lookups_total", "Total dedupe index lookups");
    describe_counter!(
        "dedupe_hits_total",
        "Dedupe index hits (event already seen)"
    );
    describe_histogram!(
        "dedupe_lookup_duration_seconds",
        "Time spent on dedupe lookups"
    );

    // =========================================================================
    // ClickHouse Indexer Metrics
    // =========================================================================

    describe_counter!(
        "clickhouse_events_indexed_total",
        "Total events inserted into ClickHouse"
    );
    describe_counter!(
        "clickhouse_segments_indexed_total",
        "Segments fully indexed into ClickHouse"
    );
    describe_counter!("clickhouse_insert_errors_total", "ClickHouse insert errors");
    describe_histogram!(
        "clickhouse_insert_duration_seconds",
        "Time spent on ClickHouse batch inserts"
    );
    describe_gauge!("clickhouse_queue_depth", "Segments waiting to be indexed");

    // =========================================================================
    // Archive Sync Metrics
    // =========================================================================

    describe_counter!(
        "archive_segments_uploaded_total",
        "Segments uploaded to remote storage"
    );
    describe_counter!(
        "archive_bytes_uploaded_total",
        "Bytes uploaded to remote storage"
    );
    describe_counter!(
        "archive_segments_deleted_local_total",
        "Local segments deleted after upload"
    );
    describe_gauge!(
        "archive_local_segments",
        "Number of segments in local archive"
    );
    describe_gauge!("archive_local_bytes", "Bytes in local archive");
}

// =============================================================================
// Metric Recording Helpers
// =============================================================================

/// Record bytes metrics with a type label.
///
/// # Example
///
/// ```rust,ignore
/// use pensieve_core::metrics::record_bytes;
///
/// record_bytes("backfill_bytes_total", "proto_compressed", 1024);
/// record_bytes("backfill_bytes_total", "notepack_raw", 512);
/// ```
pub fn record_bytes(metric_name: &'static str, byte_type: &'static str, bytes: u64) {
    metrics::counter!(metric_name, "type" => byte_type).absolute(bytes);
}

/// Increment a counter with optional labels.
///
/// Convenience wrapper around `metrics::counter!`.
#[inline]
pub fn increment(name: &'static str, count: u64) {
    metrics::counter!(name).increment(count);
}

/// Set a gauge value.
///
/// Convenience wrapper around `metrics::gauge!`.
#[inline]
pub fn set_gauge(name: &'static str, value: f64) {
    metrics::gauge!(name).set(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    // Ensure metrics are initialized exactly once for all tests
    static INIT: Once = Once::new();

    fn ensure_metrics_init() {
        INIT.call_once(|| {
            let _ = try_init_metrics();
        });
    }

    // =========================================================================
    // Initialization tests
    // =========================================================================

    #[test]
    fn test_try_init_metrics_idempotent() {
        // First call may or may not succeed (depends on test order)
        let handle1 = try_init_metrics();

        // Second call should definitely return None (already installed)
        let handle2 = try_init_metrics();

        // At most one should succeed
        assert!(handle1.is_none() || handle2.is_none());
    }

    // =========================================================================
    // Helper function tests
    // =========================================================================

    #[test]
    fn test_record_bytes_does_not_panic() {
        ensure_metrics_init();
        // Should not panic even with zero bytes
        record_bytes("test_bytes_total", "test_type", 0);
        record_bytes("test_bytes_total", "test_type", 1000);
        record_bytes("test_bytes_total", "test_type", u64::MAX);
    }

    #[test]
    fn test_increment_does_not_panic() {
        ensure_metrics_init();
        increment("test_counter", 0);
        increment("test_counter", 1);
        increment("test_counter", 100);
    }

    #[test]
    fn test_set_gauge_does_not_panic() {
        ensure_metrics_init();
        set_gauge("test_gauge", 0.0);
        set_gauge("test_gauge", 42.5);
        set_gauge("test_gauge", -100.0);
        set_gauge("test_gauge", f64::MAX);
    }

    #[test]
    fn test_set_gauge_with_various_values() {
        ensure_metrics_init();
        // Test various floating point edge cases
        set_gauge("test_edge_gauge", f64::MIN);
        set_gauge("test_edge_gauge", f64::EPSILON);
        set_gauge("test_edge_gauge", std::f64::consts::PI);
    }

    // =========================================================================
    // Metric description registration
    // =========================================================================

    #[test]
    fn test_register_common_metrics_does_not_panic() {
        ensure_metrics_init();
        // This should be idempotent and not panic
        register_common_metrics();
        register_common_metrics();
    }
}
