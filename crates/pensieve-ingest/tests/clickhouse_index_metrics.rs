//! The ClickHouse indexer keeps `clickhouse_index_oldest_pending_seconds`
//! current on its own, with no journal changes to trigger an update. The
//! backlog alert depends entirely on this gauge.
//!
//! A separate test binary, because it installs a process-wide recorder.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pensieve_ingest::{ClickHouseConfig, ClickHouseIndexer, IndexQueue, IndexRetryPolicy};

const AGE: &str = "clickhouse_index_oldest_pending_seconds";

/// Records the latest value of every gauge; other metrics are discarded.
#[derive(Clone, Default)]
struct GaugeLog(Arc<Mutex<HashMap<String, f64>>>);

struct LoggedGauge(GaugeLog, String);

impl metrics::GaugeFn for LoggedGauge {
    fn increment(&self, value: f64) {
        *self.0.0.lock().unwrap().entry(self.1.clone()).or_default() += value;
    }
    fn decrement(&self, value: f64) {
        *self.0.0.lock().unwrap().entry(self.1.clone()).or_default() -= value;
    }
    fn set(&self, value: f64) {
        self.0.0.lock().unwrap().insert(self.1.clone(), value);
    }
}

impl metrics::Recorder for GaugeLog {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        metrics::Counter::noop()
    }
    fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::from_arc(Arc::new(LoggedGauge(self.clone(), key.name().to_owned())))
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

impl GaugeLog {
    fn get(&self, name: &str) -> Option<f64> {
        self.0.lock().unwrap().get(name).copied()
    }
}

#[test]
fn oldest_pending_age_keeps_growing_while_the_journal_is_idle() {
    let log = GaugeLog::default();
    metrics::set_global_recorder(log.clone()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let queue = Arc::new(IndexQueue::open(dir.path().join(".clickhouse_reindex_queue")).unwrap());
    // Journaled by this run but never announced on the channel, so the indexer
    // never touches it: only the periodic refresh can move the gauge.
    queue
        .record_sealed(&dir.path().join("segment-000000001.notepack"))
        .unwrap();
    let journaled = log.get(AGE).unwrap();
    assert!(journaled < 1.0, "a fresh entry is not old: {journaled}");

    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let indexer = ClickHouseIndexer::new(ClickHouseConfig {
        url: format!("http://127.0.0.1:{closed_port}"),
        database: "nostr".to_string(),
        table: "events_local".to_string(),
        batch_size: 10,
        index_queue: Some(Arc::clone(&queue)),
        retry: IndexRetryPolicy::default(),
    })
    .unwrap();
    let (sender, receiver) = crossbeam_channel::unbounded();
    let handle = indexer.start(receiver);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let age = log.get(AGE).unwrap();
        if age >= 1.5 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{AGE} was not refreshed while the indexer ran: {age}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(log.get("clickhouse_index_queue_segments"), Some(1.0));

    drop(sender);
    handle.join().unwrap();
}
