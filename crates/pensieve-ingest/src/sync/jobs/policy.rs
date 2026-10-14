//! Parent-owned policy exclusions: not archive receipts, not dedupe markers.
//! A capped exclusion cache never evicts proof backing unresolved receipts.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{JobLedger, Lease, LedgerError, commit, verify_lease};
use crate::giftwrap_filter::AdmissionPolicy;
use crate::sync::{ArchivedWindow, MAX_WINDOW_ITEMS, SyncStateDb};

// Fixed-size metadata, not event payloads. Reserve conservatively 1 KiB per row
// within one eighth of the ledger budget; the physical budget still applies.
const MAX_EXCLUSIONS: u64 = 100_000;

fn capacity(max_bytes: u64) -> u64 {
    (max_bytes / (8 * 1024)).clamp(1, MAX_EXCLUSIONS)
}

pub(super) const SCHEMA: &str = "
ALTER TABLE attempts ADD COLUMN excluded INTEGER NOT NULL DEFAULT 0
    CHECK(excluded>=0 AND excluded+archived<=received);
CREATE TABLE admission_policy (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32),
    recipients TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason='giftwrap_recipient_v1')
);
CREATE TABLE policy_exclusions (
    event_id BLOB PRIMARY KEY CHECK(length(event_id)=32),
    created_at INTEGER NOT NULL CHECK(created_at>=0),
    first_job INTEGER NOT NULL, first_attempt INTEGER NOT NULL,
    FOREIGN KEY(first_job,first_attempt) REFERENCES attempts(job,attempt)
) WITHOUT ROWID;
CREATE INDEX policy_exclusions_time ON policy_exclusions(created_at,event_id);
CREATE INDEX receipts_policy_id ON receipts(event_id);
CREATE TABLE exclusion_totals (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    retained INTEGER NOT NULL CHECK(retained>=0)
);
INSERT INTO exclusion_totals VALUES(1,0);
CREATE TRIGGER exclusion_added AFTER INSERT ON policy_exclusions BEGIN
    UPDATE exclusion_totals SET retained=retained+1 WHERE singleton=1;
END;
CREATE TRIGGER exclusion_removed AFTER DELETE ON policy_exclusions BEGIN
    UPDATE exclusion_totals SET retained=retained-1 WHERE singleton=1;
END;
";

pub(super) fn bind(
    tx: &Transaction<'_>,
    policy: &AdmissionPolicy,
    initialize: bool,
) -> Result<(), LedgerError> {
    let saved: Option<([u8; 32], String)> = tx
        .query_row(
            "SELECT fingerprint,recipients FROM admission_policy WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match saved {
        Some((fingerprint, keys)) => {
            let keys: Vec<String> = serde_json::from_str(&keys)
                .map_err(|_| LedgerError::Invalid("invalid pinned admission policy"))?;
            let saved = AdmissionPolicy::new(&keys)
                .map_err(|_| LedgerError::Invalid("invalid pinned admission policy"))?;
            if saved.recipients() != keys || saved.fingerprint() != fingerprint {
                return Err(LedgerError::Invalid("invalid pinned admission policy"));
            }
            if !keys
                .iter()
                .all(|key| policy.recipients().binary_search(key).is_ok())
            {
                return Err(LedgerError::Invalid(
                    "admission policy removal requires explicit recovery of prior exclusions",
                ));
            }
            tx.execute(
                "UPDATE admission_policy SET fingerprint=?1,recipients=?2 WHERE singleton=1",
                params![policy.fingerprint().as_slice(), encode_policy(policy)?],
            )?;
            Ok(())
        }
        None if initialize => {
            tx.execute(
                "INSERT INTO admission_policy VALUES(1,?1,?2,'giftwrap_recipient_v1')",
                params![policy.fingerprint().as_slice(), encode_policy(policy)?],
            )?;
            Ok(())
        }
        None => Err(LedgerError::Invalid("missing pinned admission policy")),
    }
}

fn encode_policy(policy: &AdmissionPolicy) -> Result<String, LedgerError> {
    serde_json::to_string(policy.recipients())
        .map_err(|_| LedgerError::Invalid("invalid admission policy"))
}

fn retained(db: &Connection) -> Result<u64, LedgerError> {
    Ok(db.query_row(
        "SELECT retained FROM exclusion_totals WHERE singleton=1",
        [],
        |r| r.get(0),
    )?)
}

pub(super) fn report(db: &Connection, max_bytes: u64) -> Result<(), LedgerError> {
    metrics::gauge!("negentropy_policy_exclusions_retained").set(retained(db)? as f64);
    metrics::gauge!("negentropy_policy_exclusions_capacity").set(capacity(max_bytes) as f64);
    Ok(())
}

// Called within the exclusion transaction, after removing this frame's receipt.
// At capacity evict ONE unpinned ID, never attempt counts or unfinished proof.
// The time index and receipt ID index bound the search by the cache cardinality.
fn reserve_slot(tx: &Transaction<'_>, id: &[u8; 32], cap: u64) -> Result<bool, LedgerError> {
    if contains(tx, id)? || retained(tx)? < cap {
        return Ok(false);
    }
    let removed = tx.execute(
        "DELETE FROM policy_exclusions WHERE event_id=(
        SELECT e.event_id FROM policy_exclusions e
        WHERE NOT EXISTS(SELECT 1 FROM receipts r WHERE r.event_id=e.event_id)
        ORDER BY e.created_at,e.event_id LIMIT 1)",
        [],
    )?;
    if removed == 0 || retained(tx)? >= cap {
        return Err(LedgerError::Budget);
    }
    Ok(true)
}

pub(super) fn contains(db: &Connection, id: &[u8; 32]) -> Result<bool, LedgerError> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM policy_exclusions WHERE event_id=?1)",
        [id.as_slice()],
        |r| r.get(0),
    )?)
}

impl JobLedger {
    /// Resolve only this validated frame after a parent-owned policy decision.
    /// Atomic exclusion + accounting precedes credit return. A crash before the
    /// commit leaves a normal unresolved receipt; after it, retry inventory has ID.
    pub(crate) fn exclude_by_policy(
        &mut self,
        lease: &Lease,
        sequence: u64,
        event: &nostr_sdk::Event,
        now: i64,
    ) -> Result<bool, LedgerError> {
        if !self.policy.blocks(event) {
            return Ok(false);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_lease(&tx, lease, now)?;
        let removed = tx.execute(
            "DELETE FROM receipts WHERE job=?1 AND attempt=?2 AND sequence=?3 AND event_id=?4 AND created_at=?5",
            params![lease.job().id, lease.job().attempt, sequence, event.id.as_bytes().as_slice(), event.created_at.as_secs()],
        )?;
        if removed != 1 {
            return Err(LedgerError::Invalid("missing policy exclusion receipt"));
        }
        let evicted = reserve_slot(&tx, event.id.as_bytes(), capacity(self.limits.max_bytes))?;
        tx.execute(
            "INSERT INTO policy_exclusions VALUES(?1,?2,?3,?4) ON CONFLICT(event_id) DO NOTHING",
            params![
                event.id.as_bytes().as_slice(),
                event.created_at.as_secs(),
                lease.job().id,
                lease.job().attempt
            ],
        )?;
        tx.execute(
            "UPDATE attempts SET excluded=excluded+1 WHERE job=?1 AND attempt=?2",
            params![lease.job().id, lease.job().attempt],
        )?;
        tx.execute(
            "UPDATE receipt_totals SET retained=retained-1 WHERE singleton=1",
            [],
        )?;
        commit(tx, &self.path, self.limits)?;
        report(&self.db, self.limits.max_bytes)?;
        if evicted {
            metrics::counter!("negentropy_policy_exclusions_evicted_total").increment(1);
        }
        Ok(true)
    }

    /// Complete bounded reconciliation inventory: archived OR explicitly excluded.
    /// Exclusions never enter the archive-only inventory or global dedupe index.
    /// Over-cap unions return TooDense, allowing normal preflight splitting.
    pub(crate) fn reconciliation_window(
        &self,
        inventory: &SyncStateDb,
        dedupe: &crate::DedupeIndex,
        since: u64,
        until: u64,
    ) -> Result<ArchivedWindow, LedgerError> {
        // Exclusions first: an over-cap exclusion set is TooDense whatever the
        // inventory holds, and checking it is far cheaper than the inventory's
        // per-ID dedupe lookups.
        let mut exclusions = Vec::new();
        let mut query = self.db.prepare("SELECT event_id,created_at FROM policy_exclusions WHERE created_at>=?1 AND created_at<=?2 ORDER BY created_at,event_id LIMIT ?3")?;
        for (count, item) in query
            .query_map(params![since, until, MAX_WINDOW_ITEMS + 1], |r| {
                Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, u64>(1)?))
            })?
            .enumerate()
        {
            if count == MAX_WINDOW_ITEMS {
                return Ok(ArchivedWindow::TooDense);
            }
            exclusions.push(item?);
        }
        let mut items = match inventory.archived_window(since, until, MAX_WINDOW_ITEMS, dedupe)? {
            ArchivedWindow::Complete(items) => items,
            ArchivedWindow::TooDense => return Ok(ArchivedWindow::TooDense),
        };
        items.append(&mut exclusions);
        items.sort_unstable_by_key(|(id, timestamp)| (*timestamp, *id));
        items.dedup();
        if items.len() > MAX_WINDOW_ITEMS {
            return Ok(ArchivedWindow::TooDense);
        }
        Ok(ArchivedWindow::Complete(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::ipc::{
        AdmissionOutcome, Frame, Header, Message, RegisteredEvent, UploadAction, UploadSession,
    };
    use crate::sync::jobs::{JobState, LedgerLimits};
    use nostr_sdk::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

    fn event(kind: u16, key: &str) -> Event {
        EventBuilder::new(
            Kind::from(kind),
            "payload must not be retained for blocked events",
        )
        .tags([Tag::parse(["p", key]).unwrap()])
        .custom_created_at(Timestamp::from(150))
        .sign_with_keys(&Keys::generate())
        .unwrap()
    }

    fn receive(
        db: &mut JobLedger,
        upload: &mut UploadSession,
        lease: &Lease,
        seq: u64,
        event: Event,
    ) -> RegisteredEvent {
        let bytes = Frame::encode(&Message::Event {
            header: Header::for_lease(lease, seq),
            event: Box::new(event),
        })
        .unwrap();
        match upload
            .receive(db, Frame::read(&mut &bytes[..]).unwrap(), 11)
            .unwrap()
        {
            UploadAction::Event(event) => event,
            _ => panic!("expected event"),
        }
    }

    fn done(db: &mut JobLedger, upload: &mut UploadSession, lease: &Lease) {
        let progress = db
            .attempt_progress(lease.job().id, lease.job().attempt)
            .unwrap();
        let bytes = Frame::encode(&Message::ProtocolDone {
            header: Header::for_lease(lease, progress.received + 1),
            count: progress.received,
            digest: progress.digest,
        })
        .unwrap();
        assert!(matches!(
            upload
                .receive(db, Frame::read(&mut &bytes[..]).unwrap(), 13)
                .unwrap(),
            UploadAction::ProtocolDone
        ));
    }

    #[test]
    fn mixed_upload_excludes_before_dedupe_and_requires_real_archive_durability() {
        let root = tempfile::tempdir().unwrap();
        let key = Keys::generate().public_key().to_hex();
        let policy = AdmissionPolicy::new(std::slice::from_ref(&key)).unwrap();
        let path = root.path().join("jobs.sqlite");
        let mut db =
            JobLedger::open_with_policy(&path, LedgerLimits::default(), policy.clone()).unwrap();
        let (dedupe, writer) = super::super::tests::archive(&root);
        let inventory = SyncStateDb::open(root.path().join("inventory")).unwrap();
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = db.lease_next(10, 600).unwrap().unwrap();
        let mut upload = UploadSession::new(lease.clone());
        let blocked = event(1059, &key);
        let admitted = event(1, &key);
        let registered = receive(&mut db, &mut upload, &lease, 1, blocked.clone());
        let accepted = upload
            .admit_and_accept(&mut db, registered, &dedupe, &writer, || 12)
            .unwrap();
        assert_eq!(accepted.outcome, AdmissionOutcome::PolicyExcluded);
        assert_eq!(dedupe.get_status(blocked.id.as_bytes()).unwrap(), None);
        assert!(writer.seal().unwrap().is_none());
        let registered = receive(&mut db, &mut upload, &lease, 2, admitted.clone());
        let accepted = upload
            .admit_and_accept(&mut db, registered, &dedupe, &writer, || 12)
            .unwrap();
        assert_eq!(accepted.outcome, AdmissionOutcome::Admitted);
        done(&mut db, &mut upload, &lease);
        assert!(
            !db.reconcile_archived(lease.job().id, &dedupe, &writer, 256)
                .unwrap()
                .complete
        );
        let progress = db.attempt_progress(lease.job().id, 1).unwrap();
        assert_eq!(
            (progress.received, progress.archived, progress.excluded),
            (2, 0, 1)
        );
        writer.seal().unwrap().unwrap();
        assert!(
            db.reconcile_archived(lease.job().id, &dedupe, &writer, 256)
                .unwrap()
                .complete
        );
        let progress = db.attempt_progress(lease.job().id, 1).unwrap();
        assert_eq!((progress.archived, progress.excluded), (1, 1));
        drop(db);
        let db = JobLedger::open_with_policy(&path, LedgerLimits::default(), policy).unwrap();
        assert_eq!(db.get(lease.job().id).unwrap().state, JobState::Complete);
        assert_eq!(
            db.reconciliation_window(&inventory, &dedupe, 100, 200)
                .unwrap(),
            ArchivedWindow::Complete(vec![(blocked.id.to_bytes(), 150)])
        );
        assert_eq!(
            inventory
                .archived_window(100, 200, MAX_WINDOW_ITEMS, &dedupe)
                .unwrap(),
            ArchivedWindow::Complete(vec![])
        );
        assert!(JobLedger::open(&path, LedgerLimits::default()).is_err());
    }

    #[test]
    fn lost_ack_and_prior_attempt_receipts_recover_without_archival_claims() {
        let root = tempfile::tempdir().unwrap();
        let key = Keys::generate().public_key().to_hex();
        let policy = AdmissionPolicy::new(std::slice::from_ref(&key)).unwrap();
        let path = root.path().join("jobs.sqlite");
        let mut db =
            JobLedger::open_with_policy(&path, LedgerLimits::default(), policy.clone()).unwrap();
        let (dedupe, writer) = super::super::tests::archive(&root);
        let inventory = SyncStateDb::open(root.path().join("inventory")).unwrap();
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let first = db.lease_next(10, 60).unwrap().unwrap();
        let blocked = event(1059, &key);
        let mut upload = UploadSession::new(first.clone());
        let _unadmitted = receive(&mut db, &mut upload, &first, 1, blocked.clone());
        // Crash after registration but before the policy transaction: still pending.
        drop(db);
        let mut db =
            JobLedger::open_with_policy(&path, LedgerLimits::default(), policy.clone()).unwrap();
        assert!(db.expire(70, 60).unwrap());
        let second = db.lease_next(130, 600).unwrap().unwrap();
        // Register the same independently verified event on a later attempt.
        let mut upload = UploadSession::new(second.clone());
        let bytes = Frame::encode(&Message::Event {
            header: Header::for_lease(&second, 1),
            event: Box::new(blocked.clone()),
        })
        .unwrap();
        let UploadAction::Event(registered) = upload
            .receive(&mut db, Frame::read(&mut &bytes[..]).unwrap(), 131)
            .unwrap()
        else {
            panic!("event")
        };
        // Commit exclusion, then expire before ACK: exclusion survives, no summary.
        let mut ticks = [131, 730].into_iter();
        assert!(
            upload
                .admit_and_accept(&mut db, registered, &dedupe, &writer, || ticks
                    .next()
                    .unwrap())
                .is_err()
        );
        drop(db);
        let mut db = JobLedger::open_with_policy(&path, LedgerLimits::default(), policy).unwrap();
        assert!(db.expire(730, 60).unwrap());
        assert!(
            !db.reconcile_archived(first.job().id, &dedupe, &writer, 256)
                .unwrap()
                .complete
        );
        assert_eq!(db.attempt_progress(first.job().id, 1).unwrap().excluded, 1);
        assert_eq!(db.attempt_progress(first.job().id, 2).unwrap().excluded, 1);
        assert_eq!(
            db.reconciliation_window(&inventory, &dedupe, 100, 200)
                .unwrap(),
            ArchivedWindow::Complete(vec![(blocked.id.to_bytes(), 150)])
        );
        let third = db.lease_next(790, 600).unwrap().unwrap();
        let digest = crate::sync::ipc::initial_digest();
        db.record_protocol_done(&third, 1, 0, digest, 791).unwrap();
        assert!(
            db.reconcile_archived(first.job().id, &dedupe, &writer, 256)
                .unwrap()
                .complete
        );
        assert_eq!(dedupe.get_status(blocked.id.as_bytes()).unwrap(), None);
        assert!(writer.seal().unwrap().is_none());
    }

    #[test]
    fn v7_migration_preserves_jobs_and_policy_change_is_rejected_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        let job = db
            .enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        super::super::retention::strip_for_legacy_fixture(&db.db);
        db.db.execute_batch("DROP TABLE policy_exclusions; DROP TABLE exclusion_totals; DROP INDEX receipts_policy_id; DROP TABLE admission_policy; ALTER TABLE attempts DROP COLUMN excluded; PRAGMA user_version=7;").unwrap();
        drop(db);
        let key = Keys::generate().public_key().to_hex();
        let policy = AdmissionPolicy::new(&[key]).unwrap();
        let db =
            JobLedger::open_with_policy(&path, LedgerLimits::default(), policy.clone()).unwrap();
        assert_eq!(db.get(job.id).unwrap(), job);
        assert_eq!(
            db.db
                .pragma_query_value(None, "user_version", |r| r.get::<_, u64>(0))
                .unwrap(),
            10
        );
        drop(db);
        assert!(JobLedger::open(&path, LedgerLimits::default()).is_err());
        let db = JobLedger::open_with_policy(&path, LedgerLimits::default(), policy).unwrap();
        assert_eq!(db.get(job.id).unwrap(), job);
        db.db.execute("DELETE FROM admission_policy", []).unwrap();
        drop(db);
        assert!(JobLedger::open(&path, LedgerLimits::default()).is_err());
    }

    #[test]
    fn failed_exclusion_transaction_preserves_receipt_and_poisoned_session() {
        let root = tempfile::tempdir().unwrap();
        let key = Keys::generate().public_key().to_hex();
        let policy = AdmissionPolicy::new(std::slice::from_ref(&key)).unwrap();
        let mut db = JobLedger::open_with_policy(
            &root.path().join("jobs.sqlite"),
            LedgerLimits::default(),
            policy,
        )
        .unwrap();
        let (dedupe, writer) = super::super::tests::archive(&root);
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = db.lease_next(10, 600).unwrap().unwrap();
        let mut upload = UploadSession::new(lease.clone());
        let blocked = event(1059, &key);
        let registered = receive(&mut db, &mut upload, &lease, 1, blocked.clone());
        db.db.execute_batch("CREATE TRIGGER exclusion_fault BEFORE INSERT ON policy_exclusions BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(
            upload
                .admit_and_accept(&mut db, registered, &dedupe, &writer, || 12)
                .is_err()
        );
        assert_eq!(db.attempt_progress(lease.job().id, 1).unwrap().excluded, 0);
        assert!(!contains(&db.db, blocked.id.as_bytes()).unwrap());
        db.db
            .execute_batch("DROP TRIGGER exclusion_fault;")
            .unwrap();
        db.limits.max_bytes = 1;
        assert!(matches!(
            db.exclude_by_policy(&lease, 1, &blocked, 12),
            Err(LedgerError::Budget)
        ));
        assert!(!contains(&db.db, blocked.id.as_bytes()).unwrap());
        assert_eq!(db.attempt_progress(lease.job().id, 1).unwrap().excluded, 0);
        assert_eq!(
            db.db
                .query_row("SELECT retained FROM receipt_totals", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert_eq!(dedupe.get_status(blocked.id.as_bytes()).unwrap(), None);
        assert!(writer.seal().unwrap().is_none());
    }

    #[test]
    fn exclusion_inventory_never_returns_a_truncated_dense_window() {
        let root = tempfile::tempdir().unwrap();
        let mut db =
            JobLedger::open(&root.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        let (dedupe, _writer) = super::super::tests::archive(&root);
        let inventory = SyncStateDb::open(root.path().join("inventory")).unwrap();
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = db.lease_next(10, 600).unwrap().unwrap();
        let mut upload = UploadSession::new(lease.clone());
        let _receipt = receive(
            &mut db,
            &mut upload,
            &lease,
            1,
            event(1, &Keys::generate().public_key().to_hex()),
        );
        // Deliberately insert synthetic metadata only to exercise the fixed cap.
        db.db.execute("WITH RECURSIVE ids(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM ids WHERE n<?1) INSERT INTO policy_exclusions SELECT CAST(printf('%032d',n) AS BLOB),150,?2,?3 FROM ids", params![MAX_WINDOW_ITEMS+1, lease.job().id, lease.job().attempt]).unwrap();
        inventory.record(&[7; 32], 150).unwrap();
        let lookups = dedupe.point_lookups();
        assert_eq!(
            db.reconciliation_window(&inventory, &dedupe, 100, 200)
                .unwrap(),
            ArchivedWindow::TooDense
        );
        // Over-cap exclusions are rejected before any inventory dedupe lookup.
        assert_eq!(dedupe.point_lookups(), lookups);
        assert_eq!(
            db.reconciliation_window(&inventory, &dedupe, 151, 200)
                .unwrap(),
            ArchivedWindow::Complete(vec![])
        );
    }

    #[test]
    fn policy_additions_preserve_jobs_but_removal_and_corruption_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        // A policy-less tooling open must not prevent later production additions.
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        let job = db
            .enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        drop(db);
        let a = Keys::generate().public_key().to_hex();
        let first = AdmissionPolicy::new(std::slice::from_ref(&a)).unwrap();
        drop(JobLedger::open_with_policy(&path, LedgerLimits::default(), first.clone()).unwrap());
        let expanded = AdmissionPolicy::new(&[a, Keys::generate().public_key().to_hex()]).unwrap();
        let db =
            JobLedger::open_with_policy(&path, LedgerLimits::default(), expanded.clone()).unwrap();
        assert_eq!(db.get(job.id).unwrap(), job);
        drop(db);
        assert!(JobLedger::open_with_policy(&path, LedgerLimits::default(), first).is_err());
        let db =
            JobLedger::open_with_policy(&path, LedgerLimits::default(), expanded.clone()).unwrap();
        assert_eq!(db.get(job.id).unwrap(), job);
        db.db
            .execute("UPDATE admission_policy SET recipients='[]'", [])
            .unwrap();
        drop(db);
        assert!(JobLedger::open_with_policy(&path, LedgerLimits::default(), expanded).is_err());
    }

    #[test]
    fn cache_churn_is_capped_and_never_evicts_unresolved_receipt_proof() {
        let root = tempfile::tempdir().unwrap();
        let mut db =
            JobLedger::open(&root.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = db.lease_next(10, 600).unwrap().unwrap();
        let mut upload = UploadSession::new(lease.clone());
        let pinned = event(1, &Keys::generate().public_key().to_hex());
        let _pending = receive(&mut db, &mut upload, &lease, 1, pinned.clone());
        db.db
            .execute(
                "INSERT INTO policy_exclusions VALUES(?1,100,?2,1)",
                params![pinned.id.as_bytes().as_slice(), lease.job().id],
            )
            .unwrap();
        // Use the production eviction helper with a tiny test capacity. Churn
        // cannot grow the cache, delete unfinished proof, or alter attempt counts.
        for n in 0u64..1000 {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&n.to_be_bytes());
            let tx = db.db.transaction().unwrap();
            assert_eq!(reserve_slot(&tx, &id, 2).unwrap(), n != 0);
            tx.execute(
                "INSERT INTO policy_exclusions VALUES(?1,150,?2,1)",
                params![id.as_slice(), lease.job().id],
            )
            .unwrap();
            tx.commit().unwrap();
            assert_eq!(retained(&db.db).unwrap(), 2);
            assert!(contains(&db.db, pinned.id.as_bytes()).unwrap());
        }
        let progress = db.attempt_progress(lease.job().id, 1).unwrap();
        assert_eq!(
            (progress.received, progress.archived, progress.excluded),
            (1, 0, 0)
        );
        // When every cached ID backs unfinished proof, fail without deleting any.
        let tx = db.db.transaction().unwrap();
        tx.execute("INSERT INTO receipts SELECT ?1,1,2,event_id,150,1,zeroblob(32) FROM policy_exclusions WHERE event_id!=?2", params![lease.job().id,pinned.id.as_bytes().as_slice()]).unwrap();
        assert!(matches!(
            reserve_slot(&tx, &[255; 32], 2),
            Err(LedgerError::Budget)
        ));
        assert_eq!(retained(&tx).unwrap(), 2);
        tx.rollback().unwrap();
        assert_eq!(capacity(LedgerLimits::default().max_bytes), 100_000);
        assert!(capacity(256 * 1024) < 100_000);
    }

    #[test]
    fn archive_recovery_does_not_turn_policy_exclusions_into_unresolved_receipts() {
        let root = tempfile::tempdir().unwrap();
        let key = Keys::generate().public_key().to_hex();
        let mut db = JobLedger::open_with_policy(
            &root.path().join("jobs.sqlite"),
            LedgerLimits::default(),
            AdmissionPolicy::new(std::slice::from_ref(&key)).unwrap(),
        )
        .unwrap();
        let (dedupe, writer) = super::super::tests::archive(&root);
        std::fs::create_dir(root.path().join("archive/segment-000000000.notepack.open")).unwrap();
        let normal = event(1, &key);
        assert!(
            writer
                .write_reserved(
                    crate::pack_nostr_event(&normal).unwrap(),
                    dedupe
                        .reserve_unarchived(normal.id.as_bytes())
                        .unwrap()
                        .unwrap()
                )
                .is_err()
        );
        assert!(writer.recovery_required());
        db.enqueue("s", "wss://relay.example.com", 100, 200)
            .unwrap();
        let lease = db.lease_next(10, 600).unwrap().unwrap();
        let mut upload = UploadSession::new(lease.clone());
        let blocked = event(1059, &key);
        let registered = receive(&mut db, &mut upload, &lease, 1, blocked.clone());
        assert_eq!(
            upload
                .admit_and_accept(&mut db, registered, &dedupe, &writer, || 12)
                .unwrap()
                .outcome,
            AdmissionOutcome::PolicyExcluded
        );
        assert_eq!(
            db.db
                .query_row("SELECT retained FROM receipt_totals", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert!(contains(&db.db, blocked.id.as_bytes()).unwrap());
        assert_eq!(dedupe.get_status(blocked.id.as_bytes()).unwrap(), None);
        done(&mut db, &mut upload, &lease);
        assert!(
            db.reconcile_archived(lease.job().id, &dedupe, &writer, 256)
                .is_err()
        );
        assert_ne!(db.get(lease.job().id).unwrap().state, JobState::Complete);
    }
}
