//! Real SQLite/RocksDB/notepack integration for isolated-worker archive receipts.
//! No relay, network listener, Parquet consumer, or production storage is used.

use std::sync::Arc;

use nostr_sdk::{Event, EventBuilder, Keys, Kind, Timestamp};
use pensieve_ingest::sync::ipc::{
    Frame, Header, Message, RegisteredEvent, UploadAction, UploadSession,
};
use pensieve_ingest::sync::jobs::{
    JobLedger, JobState, Lease, LedgerLimits, RetryReason, SplitOutcome,
};
use pensieve_ingest::{DedupeIndex, EventStatus, SegmentConfig, SegmentWriter, pack_nostr_event};

struct Harness {
    ledger: JobLedger,
    writer: SegmentWriter,
    dedupe: Arc<DedupeIndex>,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::with_legacy_pending(None)
    }

    fn with_legacy_pending(legacy_id: Option<&[u8; 32]>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(id) = legacy_id {
            let db = rocksdb::DB::open_default(dir.path().join("dedupe")).unwrap();
            let mut options = rocksdb::WriteOptions::default();
            options.set_sync(true);
            db.put_opt(id, [1], &options).unwrap();
        }
        let dedupe = Arc::new(DedupeIndex::open(dir.path().join("dedupe")).unwrap());
        let writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: dir.path().join("segments"),
                compress: false,
                ..SegmentConfig::default()
            },
            None,
            Some(dedupe.clone()),
        )
        .unwrap();
        let ledger = JobLedger::open(
            &dir.path().join("jobs.sqlite"),
            LedgerLimits {
                max_receipts: 4,
                ..LedgerLimits::default()
            },
        )
        .unwrap();
        Self {
            ledger,
            writer,
            dedupe,
            dir,
        }
    }

    fn lease(&mut self, sweep: &str, now: i64) -> Lease {
        self.ledger
            .enqueue(sweep, "wss://relay.example.com", 0, 200)
            .unwrap();
        self.ledger.lease_next(now, 60).unwrap().unwrap()
    }

    fn receive(
        &mut self,
        session: &mut UploadSession,
        lease: &Lease,
        seq: u64,
        event: &Event,
        now: i64,
    ) -> RegisteredEvent {
        let action = session
            .receive(
                &mut self.ledger,
                frame(Message::Event {
                    header: Header::for_lease(lease, seq),
                    event: Box::new(event.clone()),
                }),
                now,
            )
            .unwrap();
        match action {
            UploadAction::Event(event) => event,
            _ => panic!("not an event"),
        }
    }

    fn admit(&mut self, session: &mut UploadSession, event: RegisteredEvent, now: i64) {
        session
            .admit_and_accept(&mut self.ledger, event, &self.dedupe, &self.writer, || now)
            .unwrap();
    }

    fn done(&mut self, session: &mut UploadSession, lease: &Lease, now: i64) {
        let progress = self
            .ledger
            .attempt_progress(lease.job().id, lease.job().attempt)
            .unwrap();
        assert!(matches!(
            session
                .receive(
                    &mut self.ledger,
                    frame(Message::ProtocolDone {
                        header: Header::for_lease(lease, progress.received + 1),
                        count: progress.received,
                        digest: progress.digest,
                    }),
                    now
                )
                .unwrap(),
            UploadAction::ProtocolDone
        ));
    }

    fn reconcile(
        &mut self,
        job: i64,
        limit: u32,
    ) -> pensieve_ingest::sync::jobs::ReceiptReconciliation {
        self.ledger
            .reconcile_archived(job, &self.dedupe, &self.writer, limit)
            .unwrap()
    }

    fn live_archive(&self, event: &Event) {
        let claim = self.dedupe.reserve(event.id.as_bytes()).unwrap().unwrap();
        self.writer
            .write_reserved(pack_nostr_event(event).unwrap(), claim)
            .unwrap();
        self.writer.seal().unwrap();
    }

    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("jobs.sqlite")).unwrap()
    }
}

fn frame(message: Message) -> Frame {
    Frame::read(&mut Frame::encode(&message).unwrap().as_slice()).unwrap()
}

fn event(text: &str) -> Event {
    EventBuilder::new(Kind::TextNote, text)
        .custom_created_at(Timestamp::from(100))
        .sign_with_keys(&Keys::generate())
        .unwrap()
}

#[test]
fn legacy_pending_is_readmitted_without_inventing_archive_proof() {
    let candidate = event("legacy pending");
    let mut h = Harness::with_legacy_pending(Some(candidate.id.as_bytes()));
    assert_eq!(
        h.dedupe.get_status(candidate.id.as_bytes()).unwrap(),
        Some(EventStatus::Pending)
    );
    assert!(!h.dedupe.has_archive_owner(candidate.id.as_bytes()).unwrap());
    // A dropped reservation must not erase the legacy marker or block recovery.
    drop(
        h.dedupe
            .reserve_unarchived(candidate.id.as_bytes())
            .unwrap()
            .unwrap(),
    );
    assert_eq!(
        h.dedupe.get_status(candidate.id.as_bytes()).unwrap(),
        Some(EventStatus::Pending)
    );
    let lease = h.lease("legacy", 10);
    let mut session = UploadSession::new(lease.clone());
    for seq in 1..=2 {
        let received = h.receive(&mut session, &lease, seq, &candidate, 11);
        h.admit(&mut session, received, 12);
    }
    assert!(h.dedupe.has_archive_owner(candidate.id.as_bytes()).unwrap());
    h.done(&mut session, &lease, 13);
    assert!(!h.reconcile(lease.job().id, 4).complete);
    assert_eq!(h.writer.seal().unwrap().unwrap().event_count, 1);
    h.reconcile(lease.job().id, 4);
    assert!(h.reconcile(lease.job().id, 4).complete);
    assert_eq!(
        h.dedupe.get_status(candidate.id.as_bytes()).unwrap(),
        Some(EventStatus::Archived)
    );
    assert!(
        h.dedupe
            .reserve_unarchived(candidate.id.as_bytes())
            .unwrap()
            .is_none()
    );
}

#[test]
fn foreign_revocable_claim_cannot_ack_and_dropped_claim_is_recoverable() {
    let mut h = Harness::new();
    let candidate = event("live source has not written yet");
    let live_index = h.dedupe.clone();
    let live_claim = live_index
        .reserve(candidate.id.as_bytes())
        .unwrap()
        .unwrap();
    let lease = h.lease("foreign", 10);
    let mut session = UploadSession::new(lease.clone());
    let received = h.receive(&mut session, &lease, 1, &candidate, 11);
    assert!(
        session
            .admit_and_accept(&mut h.ledger, received, &h.dedupe, &h.writer, || 12)
            .is_err()
    );
    assert_eq!(
        h.ledger.get(lease.job().id).unwrap().state,
        JobState::Leased
    );
    assert!(h.writer.seal().unwrap().is_none());
    drop(live_claim);
    h.ledger
        .retry(&lease, 13, 60, RetryReason::RelayFailure)
        .unwrap();
    let retry = h.ledger.lease_next(73, 60).unwrap().unwrap();
    let mut session = UploadSession::new(retry.clone());
    let received = h.receive(&mut session, &retry, 1, &candidate, 74);
    h.admit(&mut session, received, 75);
    h.done(&mut session, &retry, 76);
    assert!(!h.reconcile(retry.job().id, 4).complete);
    assert_eq!(h.writer.seal().unwrap().unwrap().event_count, 1);
    h.reconcile(retry.job().id, 4);
    assert!(h.reconcile(retry.job().id, 4).complete);
    assert_eq!(
        h.ledger
            .attempt_progress(retry.job().id, 1)
            .unwrap()
            .archived,
        1
    );
}

#[test]
fn pending_duplicates_wait_for_seal_then_compact_without_parquet_or_notifications() {
    let mut h = Harness::new();
    for cycle in 0..3 {
        let lease = h.lease(&format!("s{cycle}"), 10);
        let mut session = UploadSession::new(lease.clone());
        let candidate = event("same ID in two frames");
        for seq in 1..=2 {
            let received = h.receive(&mut session, &lease, seq, &candidate, 11);
            assert_eq!(
                h.ledger
                    .attempt_progress(lease.job().id, 1)
                    .unwrap()
                    .received,
                seq
            );
            h.admit(&mut session, received, 12);
        }
        assert!(!h.dedupe.is_new(candidate.id.as_bytes()).unwrap());
        assert_eq!(h.dedupe.get_status(candidate.id.as_bytes()).unwrap(), None);
        h.done(&mut session, &lease, 13);
        assert!(!h.ledger.expire(70, 60).unwrap());
        assert!(!h.reconcile(lease.job().id, 4).complete);
        let sealed = h.writer.seal().unwrap().unwrap();
        assert_eq!(sealed.event_count, 1);
        assert_eq!(
            h.dedupe.get_status(candidate.id.as_bytes()).unwrap(),
            Some(EventStatus::Archived)
        );
        assert!(h.reconcile(lease.job().id, 4).complete);
        let progress = h.ledger.attempt_progress(lease.job().id, 1).unwrap();
        assert_eq!((progress.received, progress.archived), (2, 2));
        assert!(progress.protocol_done);
        assert_eq!(h.reconcile(lease.job().id, 4).satisfied, 0);
        assert_eq!(
            h.raw()
                .query_row("SELECT retained FROM receipt_totals", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn registration_before_admission_crash_remains_an_obligation_across_retry() {
    let mut h = Harness::new();
    let first = h.lease("s", 10);
    let mut session = UploadSession::new(first.clone());
    let candidate = event("received but never admitted");
    drop(h.receive(&mut session, &first, 1, &candidate, 11));
    // Simulate losing the entire upload session after FULL receipt commit.
    drop(session);
    h.ledger = JobLedger::open(&h.dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
    assert!(h.ledger.expire(70, 60).unwrap());
    let next = h.ledger.lease_next(130, 60).unwrap().unwrap();
    let mut session = UploadSession::new(next.clone());
    h.done(&mut session, &next, 131); // an empty later response cannot erase old IDs
    assert!(!h.reconcile(first.job().id, 1).complete);
    assert_eq!(
        h.ledger
            .attempt_progress(first.job().id, 1)
            .unwrap()
            .archived,
        0
    );
    h.live_archive(&candidate);
    h.reconcile(first.job().id, 1);
    assert!(h.reconcile(first.job().id, 1).complete);
    assert_eq!(
        h.ledger
            .attempt_progress(first.job().id, 1)
            .unwrap()
            .archived,
        1
    );
    assert!(
        !h.ledger
            .attempt_progress(first.job().id, 1)
            .unwrap()
            .protocol_done
    );
}

#[test]
fn archive_before_ledger_commit_replays_and_missing_first_id_does_not_starve_later_ids() {
    let mut h = Harness::new();
    let lease = h.lease("s", 10);
    let mut session = UploadSession::new(lease.clone());
    let missing = event("missing first");
    let later = event("archived second");
    drop(h.receive(&mut session, &lease, 1, &missing, 11));
    drop(h.receive(&mut session, &lease, 2, &later, 11));
    h.ledger
        .retry(&lease, 12, 60, RetryReason::WorkerLost)
        .unwrap();
    h.live_archive(&later);
    assert_eq!(h.reconcile(lease.job().id, 1).satisfied, 0);
    assert_eq!(h.reconcile(lease.job().id, 1).satisfied, 1);
    h.ledger = JobLedger::open(&h.dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
    assert_eq!(
        h.ledger
            .attempt_progress(lease.job().id, 1)
            .unwrap()
            .archived,
        1
    );
    h.live_archive(&missing);
    h.reconcile(lease.job().id, 1); // wrap
    let outcome = h.reconcile(lease.job().id, 1);
    assert_eq!(outcome.satisfied, 1);
    assert!(!outcome.complete); // all archived is insufficient without ProtocolDone
    assert_eq!(
        h.ledger.get(lease.job().id).unwrap().state,
        JobState::RetryWait
    );
}

#[test]
fn failed_compaction_rolls_back_deletes_counters_cursor_and_completion() {
    let mut h = Harness::new();
    let lease = h.lease("s", 10);
    let mut session = UploadSession::new(lease.clone());
    for seq in 1..=2 {
        let received = h.receive(&mut session, &lease, seq, &event("archivable"), 11);
        h.admit(&mut session, received, 12);
    }
    h.done(&mut session, &lease, 13);
    h.writer.seal().unwrap();
    h.raw().execute_batch("CREATE TRIGGER fail_delete BEFORE DELETE ON receipts WHEN OLD.sequence=2 BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        h.ledger
            .reconcile_archived(lease.job().id, &h.dedupe, &h.writer, 4)
            .is_err()
    );
    h.ledger = JobLedger::open(&h.dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
    assert_eq!(
        h.ledger
            .attempt_progress(lease.job().id, 1)
            .unwrap()
            .archived,
        0
    );
    assert_eq!(
        h.ledger.get(lease.job().id).unwrap().state,
        JobState::AwaitingDurability
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT retained FROM receipt_totals", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    h.raw().execute_batch("DROP TRIGGER fail_delete").unwrap();
    assert!(h.reconcile(lease.job().id, 4).complete);
}

#[test]
fn two_awaiting_attempts_stop_new_leases_and_zero_event_success_is_explicit() {
    let mut h = Harness::new();
    let mut ids = Vec::new();
    for sweep in ["a", "b"] {
        let lease = h.lease(sweep, 10);
        let mut session = UploadSession::new(lease.clone());
        h.done(&mut session, &lease, 11);
        ids.push(lease.job().id);
        assert!(
            h.ledger
                .retry(&lease, 12, 60, RetryReason::Cancelled)
                .is_err()
        );
    }
    h.ledger
        .enqueue("c", "wss://relay.example.com", 0, 200)
        .unwrap();
    assert!(h.ledger.lease_next(12, 60).unwrap().is_none());
    assert!(h.reconcile(ids[0], 1).complete);
    assert!(h.ledger.lease_next(12, 60).unwrap().is_some());
    assert!(h.ledger.retry_durability(ids[1], 2, 12, 60).is_err());
    h.ledger.retry_durability(ids[1], 1, 12, 60).unwrap();
    assert!(!h.reconcile(ids[1], 1).complete);
}

#[test]
fn split_parent_requires_both_children_and_its_own_receipts() {
    let mut h = Harness::new();
    let lease = h.lease("s", 10);
    let mut session = UploadSession::new(lease.clone());
    let candidate = event("parent receipt");
    drop(h.receive(&mut session, &lease, 1, &candidate, 11));
    assert!(matches!(
        h.ledger.split(&lease, 12).unwrap(),
        SplitOutcome::Children(_)
    ));
    for _ in 0..2 {
        let child = h.ledger.lease_next(13, 60).unwrap().unwrap();
        let mut session = UploadSession::new(child.clone());
        h.done(&mut session, &child, 14);
        assert!(h.reconcile(child.job().id, 1).complete);
        assert_eq!(h.ledger.get(lease.job().id).unwrap().state, JobState::Split);
    }
    h.live_archive(&candidate);
    assert!(h.reconcile(lease.job().id, 1).complete);
}

#[test]
fn slow_admission_does_not_ack_after_deadline_and_does_not_complete_job() {
    let mut h = Harness::new();
    let lease = h.lease("s", 10);
    let mut session = UploadSession::new(lease.clone());
    let received = h.receive(&mut session, &lease, 1, &event("slow"), 11);
    let mut times = [12, 70].into_iter();
    assert!(
        session
            .admit_and_accept(&mut h.ledger, received, &h.dedupe, &h.writer, || times
                .next()
                .unwrap())
            .is_err()
    );
    assert_eq!(
        h.ledger.get(lease.job().id).unwrap().state,
        JobState::Leased
    );
    assert!(h.ledger.expire(70, 60).unwrap());
    h.writer.seal().unwrap();
    assert!(!h.reconcile(lease.job().id, 1).complete);
}

#[test]
fn recovery_compaction_works_above_admission_ceiling_and_limits_are_adjustable() {
    let mut h = Harness::new();
    let lease = h.lease("s", 10);
    let mut session = UploadSession::new(lease.clone());
    let candidate = event("already archived duplicate");
    h.live_archive(&candidate);
    let received = h.receive(&mut session, &lease, 1, &candidate, 11);
    h.admit(&mut session, received, 12);
    assert!(h.writer.seal().unwrap().is_none());
    h.done(&mut session, &lease, 13);
    // Model an operator lowering the soft ceiling below the allocated footprint.
    // No destructive filesystem fill is necessary to exercise that recovery gate.
    h.raw()
        .execute_batch(
            "CREATE TABLE ballast(payload BLOB); INSERT INTO ballast VALUES(zeroblob(300000));",
        )
        .unwrap();
    h.ledger = JobLedger::open(
        &h.dir.path().join("jobs.sqlite"),
        LedgerLimits {
            max_bytes: 256 * 1024,
            max_receipts: 1,
            ..LedgerLimits::default()
        },
    )
    .unwrap();
    assert!(
        h.ledger
            .enqueue("blocked", "wss://relay.example.com", 0, 200)
            .is_err()
    );
    assert!(h.reconcile(lease.job().id, 1).complete);
    for limit in [0, 257] {
        assert!(
            h.ledger
                .reconcile_archived(lease.job().id, &h.dedupe, &h.writer, limit)
                .is_err()
        );
    }
    h.ledger = JobLedger::open(
        &h.dir.path().join("jobs.sqlite"),
        LedgerLimits {
            max_receipts: 8,
            ..LedgerLimits::default()
        },
    )
    .unwrap();
    assert!(
        h.ledger
            .enqueue("unblocked", "wss://relay.example.com", 0, 200)
            .is_ok()
    );
}

#[test]
fn archive_failure_retains_receipts_and_blocks_admission_ack_and_completion() {
    let mut h = Harness::new();
    let lease = h.lease("s", 10);
    let mut session = UploadSession::new(lease.clone());
    let candidate = event("write must fail");
    let received = h.receive(&mut session, &lease, 1, &candidate, 11);
    // Conflicting path in this test's temporary archive; never overwrite it.
    std::fs::create_dir(
        h.dir
            .path()
            .join("segments/segment-000000000.notepack.open"),
    )
    .unwrap();
    assert!(
        session
            .admit_and_accept(&mut h.ledger, received, &h.dedupe, &h.writer, || 12)
            .is_err()
    );
    assert!(h.writer.recovery_required());
    assert!(h.ledger.expire(70, 60).unwrap());
    assert!(
        h.ledger
            .reconcile_archived(lease.job().id, &h.dedupe, &h.writer, 1)
            .is_err()
    );
    let progress = h.ledger.attempt_progress(lease.job().id, 1).unwrap();
    assert_eq!(
        (progress.received, progress.archived, progress.protocol_done),
        (1, 0, false)
    );
    assert!(h.dedupe.is_new(candidate.id.as_bytes()).unwrap());
}

#[test]
fn abrupt_process_exit_after_seal_reconciles_without_seal_notifications() {
    const CHILD_DIR: &str = "PENSIEVE_RECEIPT_CRASH_TEST_DIR";
    if let Some(path) = std::env::var_os(CHILD_DIR) {
        let path = std::path::PathBuf::from(path);
        let dedupe = Arc::new(DedupeIndex::open(path.join("dedupe")).unwrap());
        let writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: path.join("segments"),
                compress: false,
                ..SegmentConfig::default()
            },
            None,
            Some(dedupe.clone()),
        )
        .unwrap();
        let mut ledger =
            JobLedger::open(&path.join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        ledger
            .enqueue("s", "wss://relay.example.com", 0, 200)
            .unwrap();
        let lease = ledger.lease_next(10, 60).unwrap().unwrap();
        let mut session = UploadSession::new(lease.clone());
        let UploadAction::Event(received) = session
            .receive(
                &mut ledger,
                frame(Message::Event {
                    header: Header::for_lease(&lease, 1),
                    event: Box::new(event("crash after seal")),
                }),
                11,
            )
            .unwrap()
        else {
            panic!("not event")
        };
        session
            .admit_and_accept(&mut ledger, received, &dedupe, &writer, || 12)
            .unwrap();
        let progress = ledger.attempt_progress(lease.job().id, 1).unwrap();
        session
            .receive(
                &mut ledger,
                frame(Message::ProtocolDone {
                    header: Header::for_lease(&lease, 2),
                    count: 1,
                    digest: progress.digest,
                }),
                13,
            )
            .unwrap();
        writer.seal().unwrap();
        // No Drop, destructor seal, SQLite checkpoint, or receipt reconciliation.
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("abrupt_process_exit_after_seal_reconciles_without_seal_notifications")
        .env(CHILD_DIR, dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let dedupe = Arc::new(DedupeIndex::open(dir.path().join("dedupe")).unwrap());
    let writer = SegmentWriter::new(
        SegmentConfig {
            output_dir: dir.path().join("segments"),
            compress: false,
            ..SegmentConfig::default()
        },
        None,
        Some(dedupe.clone()),
    )
    .unwrap();
    let mut ledger =
        JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
    assert_eq!(ledger.get(1).unwrap().state, JobState::AwaitingDurability);
    assert!(
        ledger
            .reconcile_archived(1, &dedupe, &writer, 1)
            .unwrap()
            .complete
    );
    drop(ledger);
    let ledger = JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
    assert_eq!(ledger.get(1).unwrap().state, JobState::Complete);
    assert_eq!(ledger.attempt_progress(1, 1).unwrap().archived, 1);
}
