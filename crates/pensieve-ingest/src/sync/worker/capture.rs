//! Backpressured SDK callback to a bounded wire queue. Failure is sticky.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use nostr_sdk::prelude::*;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc};

use super::super::ipc::{self, Frame, Header, Message};
use super::reconcile::DownloadProof;
use super::{Assignment, WorkerError};

pub(super) struct Pending {
    pub wire: Vec<u8>,
    pub sequence: u64,
    _credit: OwnedSemaphorePermit,
}

struct State {
    sender: Option<mpsc::Sender<Pending>>,
    failed: bool,
    rejection: Option<Rejection>,
    count: u64,
    bytes: u64,
    digest: [u8; 32],
    ids: HashSet<EventId>,
}

#[derive(Clone, Copy)]
enum Rejection {
    Volume,
    EventSize,
}

impl Rejection {
    fn error(self) -> WorkerError {
        match self {
            Self::Volume => WorkerError::Volume,
            Self::EventSize => WorkerError::EventSize,
        }
    }
}

pub(super) struct Capture {
    identity: Header,
    since: u64,
    until: u64,
    credit: Arc<Semaphore>,
    state: Mutex<State>,
}

impl fmt::Debug for Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Capture")
    }
}

impl Capture {
    pub fn new(assignment: &Assignment) -> (Self, mpsc::Receiver<Pending>) {
        let (sender, receiver) = mpsc::channel(ipc::MAX_IN_FLIGHT_EVENTS - 1);
        (
            Self {
                identity: assignment.header(0),
                since: assignment.since,
                until: assignment.until,
                credit: Arc::new(Semaphore::new(ipc::MAX_IN_FLIGHT_BYTES as usize)),
                state: Mutex::new(State {
                    sender: Some(sender),
                    failed: false,
                    rejection: None,
                    count: 0,
                    bytes: 0,
                    digest: ipc::initial_digest(),
                    ids: HashSet::new(),
                }),
            },
            receiver,
        )
    }

    async fn capture(&self, event: &Event) -> Result<(), WorkerError> {
        let mut state = self.state.lock().await;
        if state.failed || state.sender.is_none() {
            return Err(state
                .rejection
                .map_or(WorkerError::Incomplete, Rejection::error));
        }
        state.failed = true;
        if event.created_at.as_secs() < self.since || event.created_at.as_secs() > self.until {
            return Err(WorkerError::Incomplete);
        }
        crate::pipeline::validate_archive_event(event).map_err(|_| WorkerError::Incomplete)?;
        let sequence = state.count + 1;
        let wire = Frame::encode(&Message::Event {
            header: Header {
                sequence,
                ..self.identity.clone()
            },
            event: Box::new(event.clone()),
        })
        .map_err(|error| match error {
            ipc::ProtocolError::Limit => {
                state.rejection = Some(Rejection::EventSize);
                WorkerError::EventSize
            }
            other => WorkerError::Protocol(other),
        })?;
        if state.count >= ipc::MAX_EVENTS
            || state.bytes + wire.len() as u64 > ipc::MAX_ATTEMPT_BYTES
        {
            state.rejection = Some(Rejection::Volume);
            return Err(WorkerError::Volume);
        }
        let credit = self
            .credit
            .clone()
            .acquire_many_owned(wire.len() as u32)
            .await
            .map_err(|_| WorkerError::Incomplete)?;
        let bytes = wire.len() as u64;
        let digest = ipc::extend_digest(state.digest, ipc::encoded_frame_digest(&wire));
        state
            .sender
            .as_ref()
            .ok_or(WorkerError::Incomplete)?
            .send(Pending {
                wire,
                sequence,
                _credit: credit,
            })
            .await
            .map_err(|_| WorkerError::Incomplete)?;
        state.bytes += bytes;
        state.count += 1;
        state.ids.insert(event.id);
        state.digest = digest;
        state.failed = false;
        Ok(())
    }

    pub async fn finish(
        &self,
        result: Option<&DownloadProof>,
    ) -> Result<(u64, [u8; 32]), WorkerError> {
        let mut state = self.state.lock().await;
        state.sender.take();
        if let Some(reason) = state.rejection {
            return Err(reason.error());
        }
        let result = result.ok_or(WorkerError::Incomplete)?;
        if state.failed
            || !result.remote.is_subset(&result.received)
            || !result.remote.is_subset(&state.ids)
        {
            return Err(WorkerError::Incomplete);
        }
        Ok((state.count, state.digest))
    }

    pub async fn finish_download(
        &self,
        result: Result<DownloadProof, WorkerError>,
    ) -> Result<(u64, [u8; 32]), WorkerError> {
        let summary = self.finish(result.as_ref().ok()).await;
        // The SDK suppresses the notification when our callback rejects an event.
        // Preserve the first verified local cause instead of losing it to EOSE.
        match summary {
            Err(error @ (WorkerError::Volume | WorkerError::EventSize)) => Err(error),
            summary => {
                result?;
                summary
            }
        }
    }
}

impl NostrDatabase for Capture {
    fn backend(&self) -> Backend {
        Backend::Custom("isolated-worker-capture".to_owned())
    }
    fn save_event<'a>(
        &'a self,
        event: &'a Event,
    ) -> BoxedFuture<'a, Result<SaveEventStatus, DatabaseError>> {
        Box::pin(async move {
            self.capture(event).await.map_err(DatabaseError::backend)?;
            Ok(SaveEventStatus::Success)
        })
    }
    fn check_id<'a>(
        &'a self,
        _: &'a EventId,
    ) -> BoxedFuture<'a, Result<DatabaseEventStatus, DatabaseError>> {
        Box::pin(async { Ok(DatabaseEventStatus::NotExistent) })
    }
    fn event_by_id<'a>(
        &'a self,
        _: &'a EventId,
    ) -> BoxedFuture<'a, Result<Option<Event>, DatabaseError>> {
        Box::pin(async { Ok(None) })
    }
    fn count(&self, _: Filter) -> BoxedFuture<'_, Result<usize, DatabaseError>> {
        Box::pin(async { Ok(0) })
    }
    fn query(&self, filter: Filter) -> BoxedFuture<'_, Result<Events, DatabaseError>> {
        Box::pin(async move { Ok(Events::new(&filter)) })
    }
    fn negentropy_items(
        &self,
        _: Filter,
    ) -> BoxedFuture<'_, Result<Vec<(EventId, Timestamp)>, DatabaseError>> {
        // The worker-owned diff loop supplies its exact bounded inventory directly.
        Box::pin(async {
            Err(DatabaseError::backend(std::io::Error::other(
                "worker requires explicit inventory",
            )))
        })
    }
    fn delete(&self, _: Filter) -> BoxedFuture<'_, Result<(), DatabaseError>> {
        Box::pin(async { Ok(()) })
    }
    fn wipe(&self) -> BoxedFuture<'_, Result<(), DatabaseError>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::jobs::{JobLedger, LedgerLimits};

    fn fixture() -> (Capture, mpsc::Receiver<Pending>, Event) {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger =
            JobLedger::open(&dir.path().join("jobs"), LedgerLimits::default()).unwrap();
        ledger
            .enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = ledger.lease_next(10, 600).unwrap().unwrap();
        let (capture, receiver) = Capture::new(&Assignment::for_lease(&lease));
        let event = EventBuilder::new(Kind::TextNote, "fixture")
            .custom_created_at(Timestamp::from(150))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        (capture, receiver, event)
    }

    #[tokio::test]
    async fn local_rejections_survive_eose_and_keep_the_first_cause() {
        for by_count in [false, true] {
            let (capture, mut receiver, event) = fixture();
            if by_count {
                capture.state.lock().await.count = ipc::MAX_EVENTS;
            } else {
                capture.state.lock().await.bytes = ipc::MAX_ATTEMPT_BYTES;
            }
            assert!(matches!(
                capture.capture(&event).await,
                Err(WorkerError::Volume)
            ));
            let mut invalid = event;
            invalid.content.push('x');
            assert!(matches!(
                capture.capture(&invalid).await,
                Err(WorkerError::Volume)
            ));
            assert!(matches!(
                capture.finish_download(Err(WorkerError::Unavailable)).await,
                Err(WorkerError::Volume)
            ));
            assert!(receiver.recv().await.is_none());
        }
        let (capture, mut receiver, _) = fixture();
        // An individual oversized event wins over aggregate exhaustion: splitting
        // cannot make this event fit. No payload enters the upload queue.
        capture.state.lock().await.bytes = ipc::MAX_ATTEMPT_BYTES;
        let large = EventBuilder::new(Kind::TextNote, "x".repeat(ipc::MAX_FRAME_BYTES))
            .custom_created_at(Timestamp::from(150))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert!(matches!(
            capture.capture(&large).await,
            Err(WorkerError::EventSize)
        ));
        assert!(matches!(
            capture.finish_download(Err(WorkerError::Unavailable)).await,
            Err(WorkerError::EventSize)
        ));
        assert!(receiver.recv().await.is_none());

        let (capture, _receiver, mut invalid) = fixture();
        capture.state.lock().await.bytes = ipc::MAX_ATTEMPT_BYTES;
        invalid.content.push('x');
        assert!(matches!(
            capture.capture(&invalid).await,
            Err(WorkerError::Incomplete)
        ));
        assert!(matches!(
            capture.finish_download(Err(WorkerError::Unavailable)).await,
            Err(WorkerError::Unavailable)
        ));
        assert_eq!(WorkerError::Unavailable.exit_code(), 2);
        assert_eq!(WorkerError::Volume.exit_code(), 3);
        assert_eq!(WorkerError::EventSize.exit_code(), 4);
        assert_eq!(WorkerError::Incomplete.exit_code(), 1);
        assert_eq!(WorkerError::Deadline.exit_code(), 1);
    }

    #[tokio::test]
    async fn full_queue_backpressures_and_cancelled_callback_is_sticky() {
        let (capture, mut receiver, event) = fixture();
        for _ in 0..ipc::MAX_IN_FLIGHT_EVENTS - 1 {
            capture.capture(&event).await.unwrap();
        }
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                capture.capture(&event)
            )
            .await
            .is_err()
        );
        drop(receiver.try_recv().unwrap());
        assert!(capture.capture(&event).await.is_err());
        assert!(
            capture
                .finish(Some(&DownloadProof::default()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn completion_requires_fetched_and_captured_ids_and_closes_callbacks() {
        let (capture, mut receiver, event) = fixture();
        capture.capture(&event).await.unwrap();
        let pending = receiver.try_recv().unwrap();
        let expected = ipc::extend_digest(
            ipc::initial_digest(),
            ipc::encoded_frame_digest(&pending.wire),
        );
        let result = DownloadProof {
            remote: HashSet::from([event.id]),
            received: HashSet::from([event.id]),
        };
        assert_eq!(capture.finish(Some(&result)).await.unwrap(), (1, expected));
        assert!(capture.capture(&event).await.is_err());
        for fetched in [false, true] {
            let (capture, _receiver, event) = fixture();
            let mut result = DownloadProof::default();
            result.remote.insert(event.id);
            if fetched {
                result.received.insert(event.id);
            }
            assert!(capture.finish(Some(&result)).await.is_err());
        }
        let (capture, _receiver, _) = fixture();
        assert_eq!(
            capture
                .finish(Some(&DownloadProof::default()))
                .await
                .unwrap(),
            (0, ipc::initial_digest())
        );
    }

    #[tokio::test]
    async fn event_byte_credit_attempt_bounds_and_window_are_enforced() {
        for failure in 0..5 {
            let (capture, _receiver, mut event) = fixture();
            let _held = match failure {
                0 => {
                    capture.state.lock().await.count = ipc::MAX_EVENTS;
                    None
                }
                1 => {
                    capture.state.lock().await.bytes = ipc::MAX_ATTEMPT_BYTES;
                    None
                }
                2 => Some(
                    capture
                        .credit
                        .clone()
                        .try_acquire_many_owned(ipc::MAX_IN_FLIGHT_BYTES as u32)
                        .unwrap(),
                ),
                3 => {
                    event.created_at = Timestamp::from(99);
                    None
                }
                _ => {
                    event.content.push('x');
                    None
                }
            };
            assert!(!matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    capture.capture(&event)
                )
                .await,
                Ok(Ok(()))
            ));
            assert!(
                capture
                    .finish(Some(&DownloadProof::default()))
                    .await
                    .is_err()
            );
        }
    }
}
