//! Bounded, independently owned periodic archive sealing.
//!
//! Exactly one thread performs synchronous seals. There is no work queue and
//! missed ticks do not accumulate. Stopping joins any in-flight archive operation;
//! cancellation must never abandon an fsync or durable-marker update.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::SegmentWriter;

/// Periodic sealing independent of optional downstream consumers.
pub struct ArchiveSealTimer {
    stop: mpsc::SyncSender<()>,
    thread: Option<JoinHandle<()>>,
}

impl ArchiveSealTimer {
    /// Start one archive thread. A zero interval is invalid; disable at the caller.
    ///
    /// On archive failure, the writer's recovery latch remains authoritative and
    /// intake is stopped. The thread then exits instead of repeatedly retrying IO.
    pub fn start(
        interval: Duration,
        writer: Arc<SegmentWriter>,
        running: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        Self::start_with(interval, move || {
            if !running.load(Ordering::SeqCst) {
                return false;
            }
            match writer.seal() {
                Ok(Some(sealed)) => tracing::info!(
                    segment_number = sealed.segment_number,
                    event_count = sealed.event_count,
                    "periodically sealed authoritative archive segment"
                ),
                Ok(None) => {}
                Err(error) => {
                    running.store(false, Ordering::SeqCst);
                    tracing::error!(error = %crate::logging::compact_error(&error),
                        "periodic archive seal failed; intake stopped for recovery");
                    return false;
                }
            }
            true
        })
    }

    fn start_with<F>(interval: Duration, mut seal: F) -> std::io::Result<Self>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        if interval.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "zero seal interval",
            ));
        }
        let (stop, receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("archive-seal".to_owned())
            .spawn(move || {
                while matches!(
                    receiver.recv_timeout(interval),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if !seal() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Wake the timer and join its current seal before final shutdown sealing.
    pub fn shutdown(mut self) -> std::thread::Result<()> {
        self.join()
    }

    fn join(&mut self) -> std::thread::Result<()> {
        let _ = self.stop.try_send(());
        match self.thread.take() {
            Some(thread) => thread.join(),
            None => Ok(()),
        }
    }
}

impl Drop for ArchiveSealTimer {
    fn drop(&mut self) {
        if self.join().is_err() {
            tracing::error!("archive seal thread panicked during cleanup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DedupeIndex, EventStatus, SegmentConfig, pack_nostr_event};
    use nostr_sdk::{EventBuilder, Keys};

    #[test]
    fn seals_without_parquet_or_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let dedupe = Arc::new(DedupeIndex::open(dir.path().join("dedupe")).unwrap());
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: dir.path().join("archive"),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let event = EventBuilder::text_note("timer durability")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        writer
            .write_reserved(
                pack_nostr_event(&event).unwrap(),
                dedupe.reserve(event.id.as_bytes()).unwrap().unwrap(),
            )
            .unwrap();
        assert_ne!(
            dedupe.get_status(event.id.as_bytes()).unwrap(),
            Some(EventStatus::Archived)
        );
        let timer = ArchiveSealTimer::start(
            Duration::from_millis(10),
            writer.clone(),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while dedupe.get_status(event.id.as_bytes()).unwrap() != Some(EventStatus::Archived) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        timer.shutdown().unwrap();
        assert!(writer.seal().unwrap().is_none());
    }

    #[test]
    fn shutdown_wakes_idle_timer() {
        let timer =
            ArchiveSealTimer::start_with(Duration::from_secs(3600), || panic!("not due")).unwrap();
        timer.shutdown().unwrap();
        assert!(ArchiveSealTimer::start_with(Duration::ZERO, || true).is_err());
    }

    #[test]
    fn archive_failure_stops_intake_and_preserves_recovery_obligation() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                None,
            )
            .unwrap(),
        );
        let event = EventBuilder::text_note("failure")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        writer.write(pack_nostr_event(&event).unwrap()).unwrap();
        // A directory at the final file path makes rename fail without deleting bytes.
        std::fs::create_dir(archive.join("segment-000000000.notepack")).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let timer =
            ArchiveSealTimer::start(Duration::from_millis(10), writer.clone(), running.clone())
                .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while running.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        timer.shutdown().unwrap();
        assert!(writer.recovery_required());
        assert!(archive.join("segment-000000000.notepack.open").exists());
        assert!(SegmentWriter::check_recovery(&archive, "segment").is_err());
    }

    #[test]
    fn shutdown_joins_in_flight_work_without_queuing_ticks() {
        let (entered, ready) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let timer = ArchiveSealTimer::start_with(Duration::from_millis(1), move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            true
        })
        .unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        // Enqueue stop before releasing the seal, independently of when the
        // joiner thread is scheduled. A second callback cannot start.
        timer.stop.try_send(()).unwrap();
        let (finished, done) = mpsc::sync_channel(1);
        let joiner = thread::spawn(move || {
            timer.shutdown().unwrap();
            finished.send(()).unwrap();
        });
        assert!(done.recv_timeout(Duration::from_millis(25)).is_err());
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        joiner.join().unwrap();
    }
}
