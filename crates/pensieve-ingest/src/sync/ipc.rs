//! Bounded worker upload protocol and durable received-frame registration.
//!
//! No socket listener or archive admission is wired here. A future authenticated
//! Unix transport must impose lifecycle/idle deadlines and use a bounded blocking
//! executor for this synchronous codec and ledger. Wire credits bound payload
//! bytes, not allocator overhead. Never log frames or their lease capabilities.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use nostr_sdk::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::jobs::{JobLedger, Lease, LedgerError};

/// Upload protocol version; inventory/handshake integration is not implemented.
pub const VERSION: u32 = 1;
/// Maximum JSON payload; the four-byte big-endian length prefix is additional.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum received event frames per attempt (not distinct event IDs).
pub const MAX_EVENTS: u64 = 50_000;
/// Maximum total framed event bytes per attempt.
pub const MAX_ATTEMPT_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum events awaiting parent admission acknowledgement.
pub const MAX_IN_FLIGHT_EVENTS: usize = 16;
/// Maximum framed event bytes awaiting acknowledgement.
pub const MAX_IN_FLIGHT_BYTES: u64 = 8 * 1024 * 1024;

/// Deliberately sanitized errors: no remote payloads or tokens in diagnostics.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Transport failure, including EOF/truncation. Never implies completion.
    #[error("upload transport failure: {0}")]
    Io(#[from] std::io::Error),
    /// Payload cannot be safely decoded under this version.
    #[error("invalid upload frame")]
    Malformed,
    /// Frame, attempt, or outstanding-credit bound exceeded.
    #[error("upload resource limit exceeded")]
    Limit,
    /// Wrong attempt, sequence, deadline, or phase.
    #[error("upload identity or ordering violation")]
    State,
    /// Independent Nostr verification or leased timestamp window failed.
    #[error("invalid event for upload window")]
    Event,
    /// Registration did not yield a durable received-frame acknowledgement.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

/// Common upload identity. Not Debug: contains the lease capability.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    /// Version is checked for every frame.
    pub version: u32,
    /// Ledger job identity.
    pub job: i64,
    /// Leased attempt number.
    pub attempt: i64,
    /// Capability, exported only after future peer authentication.
    pub token: [u8; 32],
    /// Starts at one; strictly consecutive worker upload messages.
    pub sequence: u64,
}

impl Header {
    /// Construct only after the transport has authenticated the worker peer.
    /// This method does not authenticate a socket by itself.
    pub fn for_lease(lease: &Lease, sequence: u64) -> Self {
        Self {
            version: VERSION,
            job: lease.job().id,
            attempt: lease.job().attempt,
            token: lease.token(),
            sequence,
        }
    }
}

/// Bounded failure classes, never arbitrary remote error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// Connection/protocol failure, requiring same-window retry policy.
    Relay,
    /// Worker reports resource exhaustion; classification still needs validation.
    Resource,
    /// Explicit cancellation.
    Cancelled,
}

/// Worker-to-parent upload messages after the future inventory exchange.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    /// Signed candidate, verified independently by the receiver.
    Event { header: Header, event: Box<Event> },
    /// Worker callbacks drained; exact frame count/digest, not archive success.
    ProtocolDone {
        header: Header,
        count: u64,
        digest: [u8; 32],
    },
    /// Incomplete attempt. Never interpreted as successful empty output.
    AttemptFailed { header: Header, reason: Failure },
}

impl Message {
    fn header(&self) -> &Header {
        match self {
            Self::Event { header, .. }
            | Self::ProtocolDone { header, .. }
            | Self::AttemptFailed { header, .. } => header,
        }
    }
}

/// One decoded frame; raw bytes are retained only to hash the exact wire input.
/// No Debug implementation, because the bytes include a lease capability.
pub struct Frame {
    message: Message,
    payload: Vec<u8>,
}

impl Frame {
    /// Reject oversized lengths before allocation. EOF, even between messages,
    /// is an error; only a checked ProtocolDone closes the upload successfully.
    /// Caller must enforce read deadlines and discard the transport on error.
    pub fn read<R>(reader: &mut R) -> Result<Self, ProtocolError>
    where
        R: Read,
    {
        let mut prefix = [0; 4];
        reader.read_exact(&mut prefix)?;
        let length = u32::from_be_bytes(prefix) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(ProtocolError::Limit);
        }
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload)?;
        let message: Message =
            serde_json::from_slice(&payload).map_err(|_| ProtocolError::Malformed)?;
        if message.header().version != VERSION {
            return Err(ProtocolError::State);
        }
        Ok(Self { message, payload })
    }

    /// Encode with a bounded writer instead of building an unbounded JSON Vec.
    pub fn encode(message: &Message) -> Result<Vec<u8>, ProtocolError> {
        let mut writer = BoundedWriter(Vec::new());
        serde_json::to_writer(&mut writer, message).map_err(|_| ProtocolError::Limit)?;
        let mut wire = Vec::with_capacity(writer.0.len() + 4);
        wire.extend_from_slice(&(writer.0.len() as u32).to_be_bytes());
        wire.extend_from_slice(&writer.0);
        Ok(wire)
    }

    fn wire_bytes(&self) -> u64 {
        self.payload.len() as u64 + 4
    }

    fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update((self.payload.len() as u32).to_be_bytes());
        hash.update(&self.payload);
        hash.finalize().into()
    }
}

struct BoundedWriter(Vec<u8>);

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_FRAME_BYTES {
            return Err(std::io::Error::other("frame limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Empty event-stream digest, also used for explicitly successful zero-event jobs.
pub fn initial_digest() -> [u8; 32] {
    Sha256::digest(b"pensieve-negentropy-upload-v1\0").into()
}

/// Incremental ordered chain: SHA256(previous chain || SHA256(exact framed event)).
/// Workers must hash their encoded bytes, not reserialize a parsed event.
pub fn extend_digest(previous: [u8; 32], frame_digest: [u8; 32]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(previous);
    hash.update(frame_digest);
    hash.finalize().into()
}

/// Hash one already-encoded frame for the worker's incremental summary.
pub fn encoded_frame_digest(wire: &[u8]) -> [u8; 32] {
    Sha256::digest(wire).into()
}

pub(super) struct ReceiptRecord {
    pub sequence: u64,
    pub event_id: [u8; 32],
    pub created_at: u64,
    pub frame_bytes: u64,
    pub frame_digest: [u8; 32],
}

/// An event whose received metadata committed to the ledger. This does not prove
/// archive admission, durable archival, or novelty. Cannot be constructed by a worker.
pub struct RegisteredEvent {
    event: Box<Event>,
    header: Header,
    digest: [u8; 32],
}

impl RegisteredEvent {
    /// Candidate for the future shared archive-admission path.
    pub fn event(&self) -> &Event {
        &self.event
    }
}

/// Parent acknowledgement: received/admission ownership only, never archived.
/// The future parent transport must frame this under its own sequence contract.
#[derive(Serialize)]
pub struct Accepted {
    header: Header,
}

/// Result of processing one upload message. No variant means job complete.
pub enum UploadAction {
    /// Registered first; caller must establish admission ownership before ACK.
    Event(RegisteredEvent),
    /// Persisted matching worker summary, still awaiting archive integration.
    ProtocolDone,
    /// Caller applies explicit retry policy; receipts are retained.
    Failed(Failure),
}

/// One non-resumable attempt upload. After error, drop it and apply retry policy.
/// The transport must allow only one authenticated session for the active lease.
pub struct UploadSession {
    lease: Lease,
    count: u64,
    outstanding: BTreeMap<u64, (u64, [u8; 32])>,
    outstanding_bytes: u64,
    closed: bool,
}

impl UploadSession {
    /// Begin after authentication, assignment and bounded inventory exchange.
    /// Reconnection must expire/retry the attempt, not resume a partial stream.
    pub fn new(lease: Lease) -> Self {
        Self {
            lease,
            count: 0,
            outstanding: BTreeMap::new(),
            outstanding_bytes: 0,
            closed: false,
        }
    }

    /// Read and process a frame, poisoning the session on decode errors or EOF.
    /// The caller must still put a deadline on the underlying blocking read.
    pub fn read_receive<R, C>(
        &mut self,
        ledger: &mut JobLedger,
        reader: &mut R,
        clock: C,
    ) -> Result<UploadAction, ProtocolError>
    where
        R: Read,
        C: FnOnce() -> i64,
    {
        if self.closed {
            return Err(ProtocolError::State);
        }
        match Frame::read(reader) {
            Ok(frame) => self.receive(ledger, frame, clock()),
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }

    /// Validate and durably register before returning a candidate for admission.
    /// All errors poison the session; no further ACKs or completion are accepted.
    pub fn receive(
        &mut self,
        ledger: &mut JobLedger,
        frame: Frame,
        now: i64,
    ) -> Result<UploadAction, ProtocolError> {
        if self.closed {
            return Err(ProtocolError::State);
        }
        self.closed = true;
        let header = frame.message.header();
        if header.version != VERSION
            || header.job != self.lease.job().id
            || header.attempt != self.lease.job().attempt
            || header.token != self.lease.token()
            || header.sequence != self.count + 1
            || now < 0
            || now >= self.lease.expires_at()
        {
            return Err(ProtocolError::State);
        }
        let frame_bytes = frame.wire_bytes();
        let frame_digest = frame.digest();
        match frame.message {
            Message::Event { header, event } => {
                if self.outstanding.len() >= MAX_IN_FLIGHT_EVENTS
                    || self.outstanding_bytes + frame_bytes > MAX_IN_FLIGHT_BYTES
                {
                    return Err(ProtocolError::Limit);
                }
                let timestamp = event.created_at.as_secs();
                if timestamp < self.lease.job().since as u64
                    || timestamp > self.lease.job().until as u64
                    || event.verify().is_err()
                {
                    return Err(ProtocolError::Event);
                }
                ledger.register_received(
                    &self.lease,
                    &ReceiptRecord {
                        sequence: header.sequence,
                        event_id: event.id.to_bytes(),
                        created_at: timestamp,
                        frame_bytes,
                        frame_digest,
                    },
                    now,
                )?;
                self.count += 1;
                self.outstanding
                    .insert(header.sequence, (frame_bytes, frame_digest));
                self.outstanding_bytes += frame_bytes;
                self.closed = false;
                Ok(UploadAction::Event(RegisteredEvent {
                    event,
                    header,
                    digest: frame_digest,
                }))
            }
            Message::ProtocolDone {
                header,
                count,
                digest,
            } => {
                if !self.outstanding.is_empty() {
                    return Err(ProtocolError::State);
                }
                ledger.record_protocol_done(&self.lease, header.sequence, count, digest, now)?;
                Ok(UploadAction::ProtocolDone)
            }
            Message::AttemptFailed { reason, .. } => {
                ledger.verify_active(&self.lease, now)?;
                Ok(UploadAction::Failed(reason))
            }
        }
    }

    /// Release credit only after the caller establishes admission ownership.
    /// Receipt registration alone cannot establish that future runtime guarantee.
    /// ACKs are ordered, single-use, and do not imply archival or completion.
    pub fn accepted(
        &mut self,
        ledger: &mut JobLedger,
        event: RegisteredEvent,
        now: i64,
    ) -> Result<Accepted, ProtocolError> {
        if self.closed
            || now < 0
            || now >= self.lease.expires_at()
            || event.header.token != self.lease.token()
            || event.header.job != self.lease.job().id
            || event.header.attempt != self.lease.job().attempt
        {
            self.closed = true;
            return Err(ProtocolError::State);
        }
        if let Err(error) = ledger.verify_active(&self.lease, now) {
            self.closed = true;
            return Err(error.into());
        }
        let entry = self.outstanding.first_key_value();
        if !matches!(entry, Some((&seq, &(_, digest))) if seq == event.header.sequence && digest == event.digest)
        {
            self.closed = true;
            return Err(ProtocolError::State);
        }
        let (bytes, _) = self
            .outstanding
            .remove(&event.header.sequence)
            .ok_or(ProtocolError::State)?;
        self.outstanding_bytes -= bytes;
        Ok(Accepted {
            header: event.header,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::jobs::{JobState, LedgerLimits, RetryReason};
    use super::*;
    use nostr_sdk::{EventBuilder, Keys, Kind, Timestamp};

    fn setup() -> (tempfile::TempDir, JobLedger, Lease, UploadSession) {
        let dir = tempfile::tempdir().unwrap();
        let mut db =
            JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = db.lease_next(10, 600).unwrap().unwrap();
        let session = UploadSession::new(lease.clone());
        (dir, db, lease, session)
    }

    fn event(content: &str, timestamp: u64) -> Event {
        EventBuilder::new(Kind::TextNote, content)
            .custom_created_at(Timestamp::from(timestamp))
            .sign_with_keys(&Keys::generate())
            .unwrap()
    }

    fn wire(lease: &Lease, sequence: u64, event: Event) -> Vec<u8> {
        Frame::encode(&Message::Event {
            header: Header::for_lease(lease, sequence),
            event: Box::new(event),
        })
        .unwrap()
    }

    fn frame(wire: &[u8]) -> Frame {
        Frame::read(&mut &wire[..]).unwrap()
    }

    fn done(lease: &Lease, sequence: u64, count: u64, digest: [u8; 32]) -> Frame {
        frame(
            &Frame::encode(&Message::ProtocolDone {
                header: Header::for_lease(lease, sequence),
                count,
                digest,
            })
            .unwrap(),
        )
    }

    fn registered(action: UploadAction) -> RegisteredEvent {
        match action {
            UploadAction::Event(e) => e,
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn length_is_rejected_before_reading_or_allocating_payload() {
        struct PrefixOnly(std::io::Cursor<[u8; 4]>);
        impl Read for PrefixOnly {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                assert!(
                    self.0.position() < 4,
                    "must not read the advertised payload"
                );
                self.0.read(buf)
            }
        }
        for length in [0, MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
            assert!(matches!(
                Frame::read(&mut PrefixOnly(std::io::Cursor::new(length.to_be_bytes()))),
                Err(ProtocolError::Limit)
            ));
        }
        assert!(matches!(
            Frame::encode(&Message::Event {
                header: Header {
                    version: VERSION,
                    job: 1,
                    attempt: 1,
                    token: [0; 32],
                    sequence: 1
                },
                event: Box::new(event(&"x".repeat(MAX_FRAME_BYTES), 100)),
            }),
            Err(ProtocolError::Limit)
        ));
    }

    #[test]
    fn truncated_malformed_unknown_and_wrong_version_frames_fail_closed() {
        let (_dir, mut db, lease, mut session) = setup();
        let wire = wire(&lease, 1, event("valid", 100));
        for end in [0, 1, 3, 4, wire.len() - 1] {
            assert!(Frame::read(&mut &wire[..end]).is_err());
        }
        for payload in [b"{}".as_slice(), b"null", b"[]", b"{\"type\":\"surprise\"}"] {
            let mut malformed = (payload.len() as u32).to_be_bytes().to_vec();
            malformed.extend_from_slice(payload);
            assert!(matches!(
                Frame::read(&mut malformed.as_slice()),
                Err(ProtocolError::Malformed)
            ));
        }
        let mut message = Message::ProtocolDone {
            header: Header::for_lease(&lease, 1),
            count: 0,
            digest: initial_digest(),
        };
        if let Message::ProtocolDone { header, .. } = &mut message {
            header.version += 1;
        }
        assert!(matches!(
            Frame::read(&mut Frame::encode(&message).unwrap().as_slice()),
            Err(ProtocolError::State)
        ));
        assert!(
            session
                .read_receive(&mut db, &mut &wire[..3], || 11)
                .is_err()
        );
        assert!(matches!(
            session.receive(&mut db, frame(&wire), 11),
            Err(ProtocolError::State)
        ));
        assert_eq!(db.attempt_progress(lease.job().id, 1).unwrap().received, 0);
    }

    #[test]
    fn received_before_ack_summary_survives_reopen_but_job_is_not_complete() {
        let (dir, mut db, lease, mut session) = setup();
        let event = event("same ID twice", 100);
        let mut digest = initial_digest();
        let mut total_bytes = 0;
        for sequence in 1..=2 {
            let wire = wire(&lease, sequence, event.clone());
            total_bytes += wire.len() as u64;
            digest = extend_digest(digest, encoded_frame_digest(&wire));
            let received = registered(session.receive(&mut db, frame(&wire), 11).unwrap());
            assert_eq!(received.event().id, event.id);
            assert_eq!(
                db.attempt_progress(lease.job().id, 1).unwrap().received,
                sequence
            );
            session.accepted(&mut db, received, 12).unwrap();
        }
        assert!(matches!(
            session
                .receive(&mut db, done(&lease, 3, 2, digest), 13)
                .unwrap(),
            UploadAction::ProtocolDone
        ));
        assert!(matches!(
            session.receive(&mut db, done(&lease, 3, 2, digest), 13),
            Err(ProtocolError::State)
        ));
        drop(db);
        let db = JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        let progress = db.attempt_progress(lease.job().id, 1).unwrap();
        assert_eq!(
            (
                progress.received,
                progress.bytes,
                progress.digest,
                progress.protocol_done
            ),
            (2, total_bytes, digest, true)
        );
        assert_eq!(db.get(lease.job().id).unwrap().state, JobState::Leased);
    }

    #[test]
    fn zero_event_summary_is_explicit_and_lost_worker_keeps_gap() {
        let (dir, mut db, lease, mut session) = setup();
        session
            .receive(&mut db, done(&lease, 1, 0, initial_digest()), 11)
            .unwrap();
        drop(db);
        let mut db =
            JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        assert!(
            db.attempt_progress(lease.job().id, 1)
                .unwrap()
                .protocol_done
        );
        assert!(db.expire(lease.expires_at(), 60).unwrap());
        assert_eq!(db.get(lease.job().id).unwrap().state, JobState::RetryWait);
        assert!(
            db.attempt_progress(lease.job().id, 1)
                .unwrap()
                .protocol_done
        );
    }

    #[test]
    fn invalid_identity_sequence_signature_timestamp_and_expiry_are_rejected() {
        for case in 0..7 {
            let (_dir, mut db, lease, mut session) = setup();
            let mut header = Header::for_lease(&lease, 1);
            let mut candidate = event("valid", if case == 5 { 201 } else { 100 });
            match case {
                0 => header.token[0] ^= 1,
                1 => header.attempt += 1,
                2 => header.job += 1,
                3 => header.sequence += 1,
                4 => candidate.content.push_str("tampered"),
                _ => {}
            }
            let wire = Frame::encode(&Message::Event {
                header,
                event: Box::new(candidate),
            })
            .unwrap();
            assert!(
                session
                    .receive(
                        &mut db,
                        frame(&wire),
                        if case == 6 { lease.expires_at() } else { 11 }
                    )
                    .is_err()
            );
            assert_eq!(db.attempt_progress(lease.job().id, 1).unwrap().received, 0);
            assert!(session.closed);
        }
    }

    #[test]
    fn malformed_summary_or_unacknowledged_events_never_end_protocol() {
        for case in 0..3 {
            let (_dir, mut db, lease, mut session) = setup();
            let received = registered(
                session
                    .receive(&mut db, frame(&wire(&lease, 1, event("one", 100))), 11)
                    .unwrap(),
            );
            if case != 0 {
                session.accepted(&mut db, received, 12).unwrap();
            }
            let count = if case == 1 { 0 } else { 1 };
            let digest = if case == 2 {
                [0; 32]
            } else {
                db.attempt_progress(lease.job().id, 1).unwrap().digest
            };
            assert!(
                session
                    .receive(&mut db, done(&lease, 2, count, digest), 13)
                    .is_err()
            );
            assert!(
                !db.attempt_progress(lease.job().id, 1)
                    .unwrap()
                    .protocol_done
            );
        }
    }

    #[test]
    fn duplicate_frame_and_reconnected_partial_attempt_fail_closed() {
        let (_dir, mut db, lease, mut session) = setup();
        let wire = wire(&lease, 1, event("one", 100));
        let received = registered(session.receive(&mut db, frame(&wire), 11).unwrap());
        session.accepted(&mut db, received, 12).unwrap();
        assert!(session.receive(&mut db, frame(&wire), 13).is_err());
        let mut reconnected = UploadSession::new(lease.clone());
        assert!(reconnected.receive(&mut db, frame(&wire), 14).is_err());
        assert_eq!(db.attempt_progress(lease.job().id, 1).unwrap().received, 1);
        // A new session cannot summarize the old session's receipts, even if it
        // knows their exact digest and has no in-memory outstanding credit.
        let digest = db.attempt_progress(lease.job().id, 1).unwrap().digest;
        for sequence in [1, 2] {
            let mut reconnected = UploadSession::new(lease.clone());
            assert!(
                reconnected
                    .receive(&mut db, done(&lease, sequence, 1, digest), 15)
                    .is_err()
            );
            assert!(
                !db.attempt_progress(lease.job().id, 1)
                    .unwrap()
                    .protocol_done
            );
        }
    }

    #[test]
    fn count_and_byte_credit_exhaustion_do_not_register_excess_frame() {
        for content_size in [0, 700_000] {
            let (_dir, mut db, lease, mut session) = setup();
            let event = event(&"x".repeat(content_size), 100);
            let mut received = 0;
            loop {
                let wire = wire(&lease, received + 1, event.clone());
                match session.receive(&mut db, frame(&wire), 11) {
                    Ok(UploadAction::Event(_)) => received += 1,
                    Err(ProtocolError::Limit) => break,
                    _ => panic!("unexpected result"),
                }
            }
            assert_eq!(
                db.attempt_progress(lease.job().id, 1).unwrap().received,
                received
            );
            if content_size == 0 {
                assert_eq!(received, MAX_IN_FLIGHT_EVENTS as u64);
            } else {
                assert!(received < MAX_IN_FLIGHT_EVENTS as u64);
            }
        }
    }

    #[test]
    fn acknowledgements_return_credit_but_reordered_or_revoked_acks_fail() {
        let (_dir, mut db, lease, mut session) = setup();
        for sequence in 1..=20 {
            let received = registered(
                session
                    .receive(
                        &mut db,
                        frame(&wire(&lease, sequence, event("ok", 100))),
                        11,
                    )
                    .unwrap(),
            );
            session.accepted(&mut db, received, 12).unwrap();
            assert_eq!(session.outstanding_bytes, 0);
        }
        let received = registered(
            session
                .receive(&mut db, frame(&wire(&lease, 21, event("last", 100))), 13)
                .unwrap(),
        );
        db.retry(&lease, 14, 60, RetryReason::Cancelled).unwrap();
        assert!(session.accepted(&mut db, received, 15).is_err());
        let (_dir, mut db, lease, mut session) = setup();
        let _first = registered(
            session
                .receive(&mut db, frame(&wire(&lease, 1, event("first", 100))), 11)
                .unwrap(),
        );
        let second = registered(
            session
                .receive(&mut db, frame(&wire(&lease, 2, event("second", 100))), 11)
                .unwrap(),
        );
        assert!(session.accepted(&mut db, second, 12).is_err());
        assert!(session.closed);
    }

    #[test]
    fn attempt_limits_apply_even_with_available_credit() {
        for by_bytes in [false, true] {
            let (dir, mut db, lease, mut session) = setup();
            let received = registered(
                session
                    .receive(&mut db, frame(&wire(&lease, 1, event("one", 100))), 11)
                    .unwrap(),
            );
            session.accepted(&mut db, received, 11).unwrap();
            let seed = rusqlite::Connection::open(dir.path().join("jobs.sqlite")).unwrap();
            let (count, bytes) = if by_bytes {
                (1, MAX_ATTEMPT_BYTES)
            } else {
                session.count = MAX_EVENTS;
                (MAX_EVENTS, 500)
            };
            seed.execute(
                "UPDATE attempts SET received=?1,bytes=?2",
                rusqlite::params![count, bytes],
            )
            .unwrap();
            let wire = wire(&lease, session.count + 1, event("excess", 100));
            assert!(matches!(
                session.receive(&mut db, frame(&wire), 11),
                Err(ProtocolError::Ledger(LedgerError::Invalid(_)))
            ));
            assert_eq!(
                db.attempt_progress(lease.job().id, 1).unwrap().received,
                count
            );
        }
    }

    #[test]
    fn failure_and_eof_keep_receipts_and_never_claim_success() {
        for eof in [false, true] {
            let (_dir, mut db, lease, mut session) = setup();
            let _received = session
                .receive(&mut db, frame(&wire(&lease, 1, event("one", 100))), 11)
                .unwrap();
            if eof {
                assert!(session.read_receive(&mut db, &mut &b""[..], || 12).is_err());
            } else {
                let failed = frame(
                    &Frame::encode(&Message::AttemptFailed {
                        header: Header::for_lease(&lease, 2),
                        reason: Failure::Relay,
                    })
                    .unwrap(),
                );
                assert!(matches!(
                    session.receive(&mut db, failed, 12).unwrap(),
                    UploadAction::Failed(Failure::Relay)
                ));
            }
            let progress = db.attempt_progress(lease.job().id, 1).unwrap();
            assert_eq!(progress.received, 1);
            assert!(!progress.protocol_done);
            assert!(session.closed);
        }
    }

    #[test]
    fn read_checks_clock_after_payload_arrives() {
        let (_dir, mut db, lease, mut session) = setup();
        let wire = wire(&lease, 1, event("late", 100));
        assert!(matches!(
            session.read_receive(&mut db, &mut wire.as_slice(), || lease.expires_at()),
            Err(ProtocolError::State)
        ));
        assert_eq!(db.attempt_progress(lease.job().id, 1).unwrap().received, 0);
    }
}
