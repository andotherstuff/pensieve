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
    count: u64,
    bytes: u64,
    digest: [u8; 32],
    ids: HashSet<EventId>,
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
            return Err(WorkerError::Incomplete);
        }
        state.failed = true;
        if event.created_at.as_secs() < self.since
            || event.created_at.as_secs() > self.until
            || state.count >= ipc::MAX_EVENTS
        {
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
        })?;
        if state.bytes + wire.len() as u64 > ipc::MAX_ATTEMPT_BYTES {
            return Err(WorkerError::Incomplete);
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
        let result = result.ok_or(WorkerError::Incomplete)?;
        if state.failed
            || !result.remote.is_subset(&result.received)
            || !result.remote.is_subset(&state.ids)
        {
            return Err(WorkerError::Incomplete);
        }
        Ok((state.count, state.digest))
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
