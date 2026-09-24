//! Inactive, one-at-a-time parent IPC executor. No listener or scheduler policy.
//!
//! One owned blocking thread performs inventory, SQLite and archive admission.
//! Socket deadlines cover the complete exchange; cancellation closes the socket
//! and prevents subsequent operations. An already executing disk operation cannot
//! be preempted. Its receipts remain durable and the lease is never silently reset.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::Digest;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use super::failure::FailureKind;
use super::ipc::{self, Frame, ProtocolError, UploadAction, UploadSession};
use super::jobs::{Job, JobLedger, Lease, LedgerError};
use super::worker::{self as transport, Assignment, Hello, ParentMessage};
use super::{ArchivedWindow, MAX_WINDOW_ITEMS, SyncStateDb};
use crate::{DedupeIndex, SegmentWriter};

/// Parent failures never discharge a lease or erase a received obligation.
#[derive(Debug, Error)]
pub enum ParentError {
    /// Unix credentials did not identify the expected dedicated worker account.
    #[error("unexpected worker peer")]
    Peer,
    /// Executor has stopped or its owner thread failed.
    #[error("parent executor stopped")]
    Stopped,
    /// Complete inventory exceeded the cap. No partial set was sent.
    #[error("inventory window too dense")]
    TooDense,
    /// Parent policy supplied invalid session parameters.
    #[error("invalid parent session request")]
    InvalidRequest,
    /// Local archive uncertainty blocks admission until operator recovery.
    #[error("archive recovery required")]
    RecoveryRequired,
    /// Socket or deadline failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Invalid or out-of-order worker traffic.
    #[error(transparent)]
    Protocol(ProtocolError),
    /// Durable ledger error.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// Archive inventory error.
    #[error(transparent)]
    Archive(#[from] crate::Error),
}

impl From<ProtocolError> for ParentError {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Io(error) => Self::Io(error),
            ProtocolError::Ledger(error) => Self::Ledger(error),
            ProtocolError::Archive(error) => Self::Archive(error),
            error => Self::Protocol(error),
        }
    }
}

/// Transport result, deliberately not named job completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    /// Valid summary persisted; archive reconciliation is still required.
    ProtocolDone,
    /// Authenticated failure report; caller owns durable retry/split policy.
    Failed(FailureKind),
}

type Reply<T> = oneshot::Sender<Result<T, ParentError>>;

enum Command {
    Lease {
        now: i64,
        ttl: u32,
        reply: Reply<Option<Lease>>,
    },
    Job {
        id: i64,
        reply: Reply<Job>,
    },
    Session {
        socket: UnixStream,
        lease: Lease,
        until: Instant,
        cancelled: Arc<AtomicBool>,
        reply: Reply<SessionOutcome>,
    },
}

/// A single owned database executor with a capacity-one command queue.
///
/// Construct after startup archive recovery, with the *same* dedupe index used by
/// the writer. This library does not bind a socket, launch processes, retry jobs,
/// or enable replay. Exclusive mutable methods prohibit concurrent submissions.
pub struct ParentExecutor {
    sender: Option<mpsc::Sender<Command>>,
    stopped: Option<oneshot::Receiver<JobLedger>>,
    thread: Option<JoinHandle<()>>,
}

impl ParentExecutor {
    /// Move the sole ledger connection to a dedicated blocking owner thread.
    pub fn new(
        ledger: JobLedger,
        inventory: Arc<SyncStateDb>,
        dedupe: Arc<DedupeIndex>,
        writer: Arc<SegmentWriter>,
    ) -> Result<Self, ParentError> {
        if !writer.uses_dedupe(&dedupe) {
            return Err(ParentError::Archive(crate::Error::Config(
                "parent dedupe differs from writer authority".to_owned(),
            )));
        }
        let (sender, mut receiver) = mpsc::channel(1);
        let (done, stopped) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("negentropy-parent".to_owned())
            .spawn(move || {
                let mut ledger = ledger;
                let mut consumed_attempt = None;
                while let Some(command) = receiver.blocking_recv() {
                    match command {
                        Command::Lease { now, ttl, reply } => {
                            let _ = reply.send(ledger.lease_next(now, ttl).map_err(Into::into));
                        }
                        Command::Job { id, reply } => {
                            let _ = reply.send(ledger.get(id).map_err(Into::into));
                        }
                        Command::Session {
                            socket,
                            lease,
                            until,
                            cancelled,
                            reply,
                        } => {
                            let identity = (lease.job().id, lease.job().attempt);
                            if let Err(error) = ledger.verify_active(&lease, now()) {
                                let _ = reply.send(Err(error.into()));
                                continue;
                            }
                            if consumed_attempt == Some(identity) {
                                let _ = reply.send(Err(ParentError::InvalidRequest));
                                continue;
                            }
                            if cancelled.load(Ordering::Acquire) {
                                let _ = reply.send(Err(ParentError::Io(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionAborted,
                                    "queued parent session cancelled",
                                ))));
                                continue;
                            }
                            if Instant::now() >= until {
                                let _ = reply.send(Err(ParentError::Io(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "queued parent session deadline",
                                ))));
                                continue;
                            }
                            consumed_attempt = Some(identity);
                            let result = exchange(
                                socket,
                                lease,
                                until,
                                cancelled.clone(),
                                &mut ledger,
                                &inventory,
                                &dedupe,
                                &writer,
                            );
                            let _ = reply.send(result);
                        }
                    }
                }
                let _ = done.send(ledger);
            })?;
        Ok(Self {
            sender: Some(sender),
            stopped: Some(stopped),
            thread: Some(thread),
        })
    }

    async fn request<T>(
        &mut self,
        command: Command,
        response: oneshot::Receiver<Result<T, ParentError>>,
    ) -> Result<T, ParentError> {
        self.sender
            .as_ref()
            .ok_or(ParentError::Stopped)?
            .send(command)
            .await
            .map_err(|_| ParentError::Stopped)?;
        response.await.map_err(|_| ParentError::Stopped)?
    }

    /// Acquire one lease using existing oldest-eligible policy, not a scheduler.
    /// Cancellation may leave a committed lease; its expiry preserves recovery.
    pub async fn lease_next(&mut self, now: i64, ttl: u32) -> Result<Option<Lease>, ParentError> {
        let (reply, response) = oneshot::channel();
        self.request(Command::Lease { now, ttl, reply }, response)
            .await
    }

    /// Read one bounded job record without exposing its lease capability.
    pub async fn job(&mut self, id: i64) -> Result<Job, ParentError> {
        let (reply, response) = oneshot::channel();
        self.request(Command::Job { id, reply }, response).await
    }

    /// Authenticate before exporting a lease, then perform exactly one exchange.
    ///
    /// `timeout` includes queue wait, inventory and all socket traffic, capped at
    /// nine minutes. The lease's wall-clock expiry also fences every operation.
    /// The owner polls this absolute deadline every 100 ms during socket work.
    /// Already-running disk operations cannot be preempted: the await may outlast
    /// the deadline and return a committed ProtocolDone or Failed result.
    /// Dropping this future closes its socket but cannot undo committed work;
    /// reread durable state before recovery after an external cancellation.
    pub async fn serve(
        &mut self,
        socket: tokio::net::UnixStream,
        expected_uid: u32,
        lease: Lease,
        timeout: Duration,
    ) -> Result<SessionOutcome, ParentError> {
        if socket.peer_cred()?.uid() != expected_uid {
            return Err(ParentError::Peer);
        }
        if timeout.is_zero() || timeout > Duration::from_secs(540) {
            return Err(ParentError::InvalidRequest);
        }
        let until = Instant::now() + timeout;
        let socket = socket.into_std()?;
        socket.set_nonblocking(false)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let _guard = Cancel {
            socket: socket.try_clone()?,
            cancelled: cancelled.clone(),
        };
        let (reply, response) = oneshot::channel();
        let command = Command::Session {
            socket,
            lease,
            until,
            cancelled,
            reply,
        };
        // The owner enforces the deadline and preserves its committed outcome.
        // A competing timer shutting down this socket would turn a stalled read
        // into EOF, incorrectly attributing the deadline to a peer disconnect.
        self.request(command, response).await
    }

    /// Close the queue, await the owner and return the ledger for recovery.
    /// Disk calls are not abortable; this wait never blocks an async reactor.
    pub async fn shutdown(mut self) -> Result<JobLedger, ParentError> {
        self.sender.take();
        let result = self
            .stopped
            .take()
            .ok_or(ParentError::Stopped)?
            .await
            .map_err(|_| ParentError::Stopped);
        if let Some(thread) = self.thread.take() {
            while !thread.is_finished() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            thread.join().map_err(|_| ParentError::Stopped)?;
        }
        result
    }
}

struct Cancel {
    socket: UnixStream,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Cancel {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

struct DeadlineSocket {
    socket: UnixStream,
    until: Instant,
    expires_at: i64,
    cancelled: Arc<AtomicBool>,
}
impl DeadlineSocket {
    fn check(&self) -> std::io::Result<()> {
        let wall = self.expires_at.checked_sub(now()).filter(|n| *n > 0);
        if self.cancelled.load(Ordering::Acquire) || wall.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "parent session cancelled or expired",
            ));
        }
        self.until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .map(|_| ())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "parent session deadline")
            })
    }
    fn send(&mut self, message: &ParentMessage) -> Result<(), ParentError> {
        self.write_all(&ipc::encode_value(message)?)?;
        Ok(())
    }
}
impl Read for DeadlineSocket {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.check()?;
            match self.socket.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}
impl Write for DeadlineSocket {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        loop {
            self.check()?;
            match self.socket.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.check()?;
        self.socket.flush()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(-1)
}

#[allow(clippy::too_many_arguments)]
fn exchange(
    socket: UnixStream,
    lease: Lease,
    until: Instant,
    cancelled: Arc<AtomicBool>,
    ledger: &mut JobLedger,
    inventory: &SyncStateDb,
    dedupe: &DedupeIndex,
    writer: &SegmentWriter,
) -> Result<SessionOutcome, ParentError> {
    // Configure once before the greeting. On macOS, changing SO_RCVTIMEO after
    // peer exit fails EINVAL even while its valid terminal frame remains buffered.
    // Short fixed polls retain absolute deadlines without resetting socket options.
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    socket.set_write_timeout(Some(Duration::from_millis(100)))?;
    let mut socket = DeadlineSocket {
        socket,
        until,
        expires_at: lease.expires_at(),
        cancelled,
    };
    socket.check()?;
    ledger.verify_active(&lease, now())?;
    if lease.job().since < 0
        || lease.job().until < lease.job().since
        || lease.job().until - lease.job().since >= 900
    {
        return Err(ParentError::InvalidRequest);
    }
    if writer.recovery_required() {
        return Err(ParentError::RecoveryRequired);
    }
    let mut length = [0; 4];
    socket.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    // The greeting has no variable payload. Reject oversized input before allocation.
    if length == 0 || length > 128 {
        return Err(ParentError::Protocol(ProtocolError::Limit));
    }
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes)?;
    let hello: Hello = serde_json::from_slice(&bytes).map_err(|_| ProtocolError::Malformed)?;
    if hello.version != ipc::VERSION {
        return Err(ParentError::Protocol(ProtocolError::State));
    }
    socket.check()?;
    ledger.verify_active(&lease, now())?;
    if writer.recovery_required() {
        return Err(ParentError::RecoveryRequired);
    }
    let items = match inventory.archived_window(
        lease.job().since as u64,
        lease.job().until as u64,
        MAX_WINDOW_ITEMS,
        dedupe,
    )? {
        ArchivedWindow::Complete(items) => items,
        ArchivedWindow::TooDense => return Err(ParentError::TooDense),
    };
    socket.check()?;
    ledger.verify_active(&lease, now())?;
    if writer.recovery_required() {
        return Err(ParentError::RecoveryRequired);
    }
    let assignment = Assignment::for_lease(&lease);
    socket.send(&ParentMessage::Job {
        assignment: Assignment::for_lease(&lease),
    })?;
    let mut hasher = transport::inventory_hasher();
    let mut sequence = 1;
    for chunk in items.chunks(transport::INVENTORY_CHUNK_ITEMS) {
        for (id, timestamp) in chunk {
            transport::hash_item(&mut hasher, id, *timestamp);
        }
        socket.send(&ParentMessage::InventoryChunk {
            header: assignment.header(sequence),
            items: chunk.to_vec(),
        })?;
        sequence += 1;
    }
    socket.send(&ParentMessage::InventoryEnd {
        header: assignment.header(sequence),
        count: items.len(),
        digest: hasher.finalize().into(),
    })?;
    drop(items);
    let mut upload = UploadSession::new(lease);
    loop {
        let frame = Frame::read(&mut socket)?;
        socket.check()?;
        match upload.receive(ledger, frame, now())? {
            UploadAction::Event(event) => {
                socket.check()?;
                let admission = upload.admit_and_accept(ledger, event, dedupe, writer, now);
                // Live ingestion can latch the shared writer after inventory,
                // or this admission itself can latch an uncertain write fault.
                // The received obligation remains; neither case blames the peer.
                if writer.recovery_required() {
                    return Err(ParentError::RecoveryRequired);
                }
                let accepted = admission?;
                socket.send(&ParentMessage::accepted(accepted))?;
            }
            UploadAction::ProtocolDone => return Ok(SessionOutcome::ProtocolDone),
            UploadAction::Failed(reason) => return Ok(SessionOutcome::Failed(reason)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ipc::{Header, Message};
    use super::super::jobs::{JobState, LedgerLimits};
    use super::*;
    use crate::SegmentConfig;
    use nostr_sdk::{EventBuilder, Keys, Timestamp};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Harness {
        _root: tempfile::TempDir,
        executor: ParentExecutor,
        writer: Arc<SegmentWriter>,
        dedupe: Arc<DedupeIndex>,
        inventory: Arc<SyncStateDb>,
        job: i64,
    }
    impl Harness {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
            let writer = Arc::new(
                SegmentWriter::new(
                    SegmentConfig {
                        output_dir: root.path().join("archive"),
                        compress: false,
                        ..SegmentConfig::default()
                    },
                    None,
                    Some(dedupe.clone()),
                )
                .unwrap(),
            );
            let state = Arc::new(SyncStateDb::open(root.path().join("inventory")).unwrap());
            let mut ledger =
                JobLedger::open(&root.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
            let job = ledger
                .enqueue("sweep", "wss://relay.example.com", 100, 110)
                .unwrap()
                .id;
            let executor =
                ParentExecutor::new(ledger, state.clone(), dedupe.clone(), writer.clone()).unwrap();
            Self {
                _root: root,
                executor,
                writer,
                dedupe,
                inventory: state,
                job,
            }
        }
        async fn lease(&mut self) -> Lease {
            self.executor.lease_next(now(), 60).await.unwrap().unwrap()
        }
    }

    #[test]
    fn constructor_rejects_missing_or_different_writer_dedupe() {
        for missing in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let authority = Arc::new(DedupeIndex::open(root.path().join("authority")).unwrap());
            let wrong = Arc::new(DedupeIndex::open(root.path().join("wrong")).unwrap());
            let writer = Arc::new(
                SegmentWriter::new(
                    SegmentConfig {
                        output_dir: root.path().join("archive"),
                        ..SegmentConfig::default()
                    },
                    None,
                    if missing { None } else { Some(authority) },
                )
                .unwrap(),
            );
            let inventory = Arc::new(SyncStateDb::open(root.path().join("inventory")).unwrap());
            let ledger =
                JobLedger::open(&root.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
            assert!(matches!(
                ParentExecutor::new(ledger, inventory, wrong, writer),
                Err(ParentError::Archive(crate::Error::Config(_)))
            ));
        }
    }

    #[test]
    fn deadline_socket_drains_buffered_bytes_after_peer_exits() {
        let (parent, mut peer) = UnixStream::pair().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        peer.write_all(b"terminal").unwrap();
        drop(peer);
        let mut socket = DeadlineSocket {
            socket: parent,
            until: Instant::now() + Duration::from_secs(1),
            expires_at: now() + 2,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let mut terminal = [0; 8];
        socket.read_exact(&mut terminal).unwrap();
        assert_eq!(&terminal, b"terminal");
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    }
    async fn greeting(socket: &mut tokio::net::UnixStream) -> Assignment {
        transport::write_message(
            socket,
            &Hello {
                version: ipc::VERSION,
            },
        )
        .await
        .unwrap();
        let ParentMessage::Job { assignment } = transport::read_message(socket).await.unwrap()
        else {
            panic!("assignment expected")
        };
        let ParentMessage::InventoryEnd { count: 0, .. } =
            transport::read_message(socket).await.unwrap()
        else {
            panic!("empty complete inventory expected")
        };
        assignment
    }

    #[tokio::test]
    async fn terminal_frame_is_processed_when_worker_immediately_closes() {
        let mut h = Harness::new();
        let lease = h.lease().await;
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let uid = parent.peer_cred().unwrap().uid();
        let client = async move {
            let assignment = greeting(&mut worker).await;
            worker
                .write_all(
                    &Frame::encode(&Message::ProtocolDone {
                        header: assignment.header(1),
                        count: 0,
                        digest: ipc::initial_digest(),
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
            client
        );
        assert_eq!(result.unwrap(), SessionOutcome::ProtocolDone);
        assert_eq!(
            h.executor.job(h.job).await.unwrap().state,
            JobState::AwaitingDurability
        );
        h.executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn nonempty_multichunk_inventory_passes_real_worker_parser() {
        let mut h = Harness::new();
        let mut expected = Vec::new();
        for n in 0..300 {
            let event = EventBuilder::text_note(format!("inventory {n}"))
                .custom_created_at(Timestamp::from(105))
                .sign_with_keys(&Keys::generate())
                .unwrap();
            h.writer
                .write_reserved(
                    crate::pack_nostr_event(&event).unwrap(),
                    h.dedupe.reserve(event.id.as_bytes()).unwrap().unwrap(),
                )
                .unwrap();
            expected.push((event.id.to_bytes(), 105));
        }
        h.writer.seal().unwrap();
        h.inventory
            .record_batch(expected.iter().map(|(id, timestamp)| (id, *timestamp)))
            .unwrap();
        expected.sort();
        let lease = h.lease().await;
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let uid = parent.peer_cred().unwrap().uid();
        let client = async move {
            transport::write_message(
                &mut worker,
                &Hello {
                    version: ipc::VERSION,
                },
            )
            .await
            .unwrap();
            // This is the production worker parser, including chunk sequence,
            // ordering, uniqueness, count and final digest checks.
            let (assignment, inventory) = transport::inventory(&mut worker).await.unwrap();
            assert_eq!(
                inventory
                    .iter()
                    .map(|(id, stamp)| (id.to_bytes(), stamp.as_secs()))
                    .collect::<Vec<_>>(),
                expected
            );
            worker
                .write_all(
                    &Frame::encode(&Message::ProtocolDone {
                        header: assignment.header(1),
                        count: 0,
                        digest: ipc::initial_digest(),
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
            client
        );
        assert_eq!(result.unwrap(), SessionOutcome::ProtocolDone);
        h.executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_admission_ack_precedes_only_protocol_not_archive_completion() {
        let mut h = Harness::new();
        let lease = h.lease().await;
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let uid = parent.peer_cred().unwrap().uid();
        let event = EventBuilder::text_note("parent test")
            .custom_created_at(Timestamp::from(105))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        let id = event.id.to_bytes();
        let dedupe = h.dedupe.clone();
        let client = async move {
            let assignment = greeting(&mut worker).await;
            let bytes = Frame::encode(&Message::Event {
                header: assignment.header(1),
                event: Box::new(event),
            })
            .unwrap();
            let digest =
                ipc::extend_digest(ipc::initial_digest(), ipc::encoded_frame_digest(&bytes));
            worker.write_all(&bytes).await.unwrap();
            let ParentMessage::Accepted { header } =
                transport::read_message(&mut worker).await.unwrap()
            else {
                panic!("accepted expected")
            };
            assert_eq!(header.sequence, 1);
            assert!(dedupe.has_archive_owner(&id).unwrap());
            worker
                .write_all(
                    &Frame::encode(&Message::ProtocolDone {
                        header: assignment.header(2),
                        count: 1,
                        digest,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
            client
        );
        assert_eq!(result.unwrap(), SessionOutcome::ProtocolDone);
        assert_eq!(
            h.executor.job(h.job).await.unwrap().state,
            JobState::AwaitingDurability
        );
        let mut ledger = h.executor.shutdown().await.unwrap();
        assert!(
            !ledger
                .reconcile_archived(h.job, &h.dedupe, &h.writer, 256)
                .unwrap()
                .complete
        );
        h.writer.seal().unwrap();
        assert!(
            ledger
                .reconcile_archived(h.job, &h.dedupe, &h.writer, 256)
                .unwrap()
                .complete
        );
    }

    #[tokio::test]
    async fn wrong_uid_exports_nothing_and_preserves_lease() {
        let mut h = Harness::new();
        let lease = h.lease().await;
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let wrong = parent.peer_cred().unwrap().uid().wrapping_add(1);
        assert!(matches!(
            h.executor
                .serve(parent, wrong, lease, Duration::from_secs(1))
                .await,
            Err(ParentError::Peer)
        ));
        assert_eq!(
            worker.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert_eq!(h.executor.job(h.job).await.unwrap().state, JobState::Leased);
        h.executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stalled_greeting_and_dropped_session_do_not_complete_or_orphan_executor() {
        for cancel in [false, true] {
            let mut h = Harness::new();
            let lease = h.lease().await;
            let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
            let uid = parent.peer_cred().unwrap().uid();
            worker.write_all(&20u32.to_be_bytes()).await.unwrap();
            if cancel {
                let mut future =
                    Box::pin(h.executor.serve(parent, uid, lease, Duration::from_secs(5)));
                tokio::select! { result = &mut future => panic!("unexpected {result:?}"), () = tokio::time::sleep(Duration::from_millis(25)) => {} }
                drop(future);
            } else {
                assert!(matches!(
                    h.executor
                        .serve(parent, uid, lease, Duration::from_millis(30))
                        .await,
                    Err(ParentError::Io(error))
                        if error.kind() == std::io::ErrorKind::TimedOut
                ));
            }
            let ledger = tokio::time::timeout(Duration::from_secs(3), h.executor.shutdown())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(ledger.get(h.job).unwrap().state, JobState::Leased);
        }
    }

    #[tokio::test]
    async fn peer_eof_during_greeting_is_not_a_deadline() {
        let mut h = Harness::new();
        let lease = h.lease().await;
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let uid = parent.peer_cred().unwrap().uid();
        worker.write_all(&20u32.to_be_bytes()).await.unwrap();
        worker.shutdown().await.unwrap();
        assert!(matches!(
            h.executor.serve(parent, uid, lease, Duration::from_secs(5)).await,
            Err(ParentError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        let ledger = h.executor.shutdown().await.unwrap();
        assert_eq!(ledger.get(h.job).unwrap().state, JobState::Leased);
    }

    #[tokio::test]
    async fn admission_recovery_latch_is_local_and_preserves_unacked_receipt() {
        for latch_before_admission in [false, true] {
            let mut h = Harness::new();
            let lease = h.lease().await;
            let attempt = lease.job().attempt;
            let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
            let uid = parent.peer_cred().unwrap().uid();
            let path = h
                ._root
                .path()
                .join("archive/segment-000000000.notepack.open");
            let writer = h.writer.clone();
            let dedupe = h.dedupe.clone();
            let client = async move {
                let assignment = greeting(&mut worker).await;
                // Local temporary-path fault, after inventory completed. In one
                // case a live-writer call latches first; in the other the upload
                // admission itself encounters the fault.
                std::fs::create_dir(path).unwrap();
                if latch_before_admission {
                    let live = EventBuilder::text_note("live writer fault")
                        .custom_created_at(Timestamp::from(105))
                        .sign_with_keys(&Keys::generate())
                        .unwrap();
                    assert!(
                        writer
                            .write_reserved(
                                crate::pack_nostr_event(&live).unwrap(),
                                dedupe.reserve(live.id.as_bytes()).unwrap().unwrap()
                            )
                            .is_err()
                    );
                    assert!(writer.recovery_required());
                }
                let event = EventBuilder::text_note("received but not admitted")
                    .custom_created_at(Timestamp::from(105))
                    .sign_with_keys(&Keys::generate())
                    .unwrap();
                worker
                    .write_all(
                        &Frame::encode(&Message::Event {
                            header: assignment.header(1),
                            event: Box::new(event),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                // No Accepted frame may escape on either recovery path.
                assert_eq!(
                    worker.read_u8().await.unwrap_err().kind(),
                    std::io::ErrorKind::UnexpectedEof
                );
            };
            let (result, ()) = tokio::join!(
                h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
                client
            );
            assert!(matches!(result, Err(ParentError::RecoveryRequired)));
            assert!(h.writer.recovery_required());
            let ledger = h.executor.shutdown().await.unwrap();
            let progress = ledger.attempt_progress(h.job, attempt).unwrap();
            assert_eq!(
                (progress.received, progress.archived, progress.protocol_done),
                (1, 0, false)
            );
            assert_eq!(ledger.get(h.job).unwrap().state, JobState::Leased);
        }
    }

    #[test]
    fn nested_local_upload_faults_do_not_become_worker_protocol_errors() {
        assert!(matches!(
            ParentError::from(ProtocolError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut))),
            ParentError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut
        ));
        assert!(matches!(
            ParentError::from(ProtocolError::Ledger(LedgerError::Budget)),
            ParentError::Ledger(LedgerError::Budget)
        ));
        assert!(matches!(
            ParentError::from(ProtocolError::Archive(crate::Error::Config(
                "local".to_owned()
            ))),
            ParentError::Archive(crate::Error::Config(_))
        ));
        assert!(matches!(
            ParentError::from(ProtocolError::Malformed),
            ParentError::Protocol(ProtocolError::Malformed)
        ));
    }

    #[tokio::test]
    async fn eof_after_ack_keeps_receipt_for_retry() {
        let mut h = Harness::new();
        let lease = h.lease().await;
        let attempt = lease.job().attempt;
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let uid = parent.peer_cred().unwrap().uid();
        let client = async move {
            let assignment = greeting(&mut worker).await;
            let event = EventBuilder::text_note("lost worker")
                .custom_created_at(Timestamp::from(105))
                .sign_with_keys(&Keys::generate())
                .unwrap();
            worker
                .write_all(
                    &Frame::encode(&Message::Event {
                        header: assignment.header(1),
                        event: Box::new(event),
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
            let _: ParentMessage = transport::read_message(&mut worker).await.unwrap();
        };
        let (result, ()) = tokio::join!(
            h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
            client
        );
        assert!(matches!(
            result,
            Err(ParentError::Io(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        let ledger = h.executor.shutdown().await.unwrap();
        assert_eq!(ledger.get(h.job).unwrap().state, JobState::Leased);
        assert_eq!(ledger.attempt_progress(h.job, attempt).unwrap().received, 1);
    }

    #[tokio::test]
    async fn forged_summary_and_bad_greeting_fail_closed() {
        for malformed in [false, true] {
            let mut h = Harness::new();
            let lease = h.lease().await;
            let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
            let uid = parent.peer_cred().unwrap().uid();
            let client = async move {
                if malformed {
                    worker.write_all(&129u32.to_be_bytes()).await.unwrap();
                    return;
                }
                let assignment = greeting(&mut worker).await;
                let mut header: Header = assignment.header(1);
                header.sequence = 9;
                worker
                    .write_all(
                        &Frame::encode(&Message::ProtocolDone {
                            header,
                            count: 0,
                            digest: ipc::initial_digest(),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            };
            let (result, ()) = tokio::join!(
                h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
                client
            );
            assert!(result.is_err());
            let ledger = h.executor.shutdown().await.unwrap();
            assert_eq!(ledger.get(h.job).unwrap().state, JobState::Leased);
        }
    }

    #[tokio::test]
    async fn authenticated_failure_is_not_completion_and_attempt_cannot_reconnect() {
        let mut h = Harness::new();
        let lease = h.lease().await;
        let duplicate = lease.clone();
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        let uid = parent.peer_cred().unwrap().uid();
        let client = async move {
            let assignment = greeting(&mut worker).await;
            worker
                .write_all(
                    &Frame::encode(&Message::AttemptFailed {
                        header: assignment.header(1),
                        report: super::super::failure::FailureDiagnostic {
                            kind: FailureKind::Relay,
                            missing_count: 0,
                            sample: Vec::new(),
                        },
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
            client
        );
        assert_eq!(result.unwrap(), SessionOutcome::Failed(FailureKind::Relay));
        assert_eq!(h.executor.job(h.job).await.unwrap().state, JobState::Leased);
        let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
        assert!(matches!(
            h.executor
                .serve(parent, uid, duplicate, Duration::from_secs(1))
                .await,
            Err(ParentError::InvalidRequest)
        ));
        assert_eq!(
            worker.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        h.executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_or_expired_queued_command_does_not_consume_attempt() {
        for cancelled in [false, true] {
            let mut h = Harness::new();
            let lease = h.lease().await;
            let (socket, _peer) = UnixStream::pair().unwrap();
            let (reply, response) = oneshot::channel();
            let until = if cancelled {
                Instant::now() + Duration::from_secs(5)
            } else {
                Instant::now()
            };
            let command = Command::Session {
                socket,
                lease: lease.clone(),
                until,
                cancelled: Arc::new(AtomicBool::new(cancelled)),
                reply,
            };
            assert!(matches!(
                h.executor.request(command, response).await,
                Err(ParentError::Io(error)) if error.kind() == if cancelled {
                    std::io::ErrorKind::ConnectionAborted
                } else {
                    std::io::ErrorKind::TimedOut
                }
            ));
            let (parent, mut worker) = tokio::net::UnixStream::pair().unwrap();
            let uid = parent.peer_cred().unwrap().uid();
            let client = async move {
                let assignment = greeting(&mut worker).await;
                worker
                    .write_all(
                        &Frame::encode(&Message::ProtocolDone {
                            header: assignment.header(1),
                            count: 0,
                            digest: ipc::initial_digest(),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            };
            let (result, ()) = tokio::join!(
                h.executor.serve(parent, uid, lease, Duration::from_secs(5)),
                client
            );
            assert_eq!(result.unwrap(), SessionOutcome::ProtocolDone);
            h.executor.shutdown().await.unwrap();
        }
    }
}
