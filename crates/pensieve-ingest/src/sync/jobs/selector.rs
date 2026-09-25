//! Opt-in bounded fresh/gap and per-class relay rotation. No runtime scheduler.

use nostr_sdk::Keys;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{
    JobLedger, Lease, LedgerError, MAX_AWAITING_DURABILITY, check_budget, commit, read_job,
};

pub(super) const SCHEMA: &str = "
CREATE TABLE lease_rotation (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 next_class INTEGER NOT NULL CHECK(next_class IN (0,1)),
 after_fresh_relay INTEGER NOT NULL CHECK(after_fresh_relay>=0),
 after_gap_relay INTEGER NOT NULL CHECK(after_gap_relay>=0)
);
INSERT INTO lease_rotation VALUES(1,0,0,0);
CREATE INDEX fresh_due ON jobs(relay,next_eligible,id)
 WHERE state='queued' AND parent IS NULL AND attempt=0;
CREATE INDEX gap_due ON jobs(relay,next_eligible,id)
 WHERE state='retry_wait' OR (state='queued' AND (parent IS NOT NULL OR attempt>0));
";

const FRESH: &str = "SELECT id FROM jobs INDEXED BY fresh_due
 WHERE state='queued' AND parent IS NULL AND attempt=0
 AND relay=?1 AND next_eligible<=?2 ORDER BY next_eligible,id LIMIT 1";
const GAP: &str = "SELECT id FROM jobs INDEXED BY gap_due
 WHERE (state='retry_wait' OR (state='queued' AND (parent IS NOT NULL OR attempt>0)))
 AND relay=?1 AND next_eligible<=?2 ORDER BY next_eligible,id LIMIT 1";

impl JobLedger {
    /// Inspect the next fair candidate without consuming an attempt or rotating
    /// cursors. The parent uses this to reject dense inventory before leasing.
    /// Only the single ledger owner may sequence this with a subsequent lease.
    pub(in crate::sync) fn fair_candidate(
        &self,
        now: i64,
    ) -> Result<Option<super::Job>, LedgerError> {
        if now < 0 {
            return Err(LedgerError::Invalid("invalid lease time"));
        }
        let active: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE state='leased')",
            [],
            |r| r.get(0),
        )?;
        let awaiting: u32 = self.db.query_row(
            "SELECT count(*) FROM jobs WHERE state='awaiting_durability'",
            [],
            |r| r.get(0),
        )?;
        if active || awaiting >= MAX_AWAITING_DURABILITY {
            return Ok(None);
        }
        let (next, fresh, gap): (usize, i64, i64) = self.db.query_row(
            "SELECT next_class,after_fresh_relay,after_gap_relay FROM lease_rotation WHERE singleton=1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let relays = {
            let mut query = self.db.prepare(
                "SELECT id,relay FROM planner_relays WHERE enabled=1 ORDER BY id LIMIT 33",
            )?;
            query
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if relays.len() > 32 {
            return Err(LedgerError::Invalid("too many active planner relays"));
        }
        for class in [next, 1 - next] {
            let after = if class == 0 { fresh } else { gap };
            let start = relays.iter().position(|r| r.0 > after).unwrap_or(0);
            for (_, relay) in relays[start..].iter().chain(&relays[..start]) {
                let id = self
                    .db
                    .query_row(
                        if class == 0 { FRESH } else { GAP },
                        params![relay, now],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?;
                if let Some(id) = id {
                    return read_job(&self.db, id).map(Some);
                }
            }
        }
        Ok(None)
    }

    /// Opt-in fair selection across fresh roots and retained gaps, then relays.
    /// Only enabled planner identities participate. Each class has its own durable
    /// relay cursor; the chosen class alternates after a committed lease. Within a
    /// relay/class choose oldest eligibility deadline then ID, not event timestamp.
    /// At most 32 relays and 64 indexed due-head queries are examined. Errors and
    /// no candidate leave rotation unchanged. Expired active leases are not stolen.
    pub fn lease_next_fair(
        &mut self,
        now: i64,
        ttl_secs: u32,
    ) -> Result<Option<Lease>, LedgerError> {
        let expiry = now
            .checked_add(i64::from(ttl_secs))
            .filter(|_| now >= 0 && ttl_secs > 0 && ttl_secs <= 600)
            .ok_or(LedgerError::Invalid("invalid lease deadline"))?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE state='leased')",
            [],
            |r| r.get(0),
        )?;
        let awaiting: u32 = tx.query_row(
            "SELECT count(*) FROM jobs WHERE state='awaiting_durability'",
            [],
            |r| r.get(0),
        )?;
        if active || awaiting >= MAX_AWAITING_DURABILITY {
            return Ok(None);
        }
        let (next, fresh, gap): (usize, i64, i64) = tx.query_row("SELECT next_class,after_fresh_relay,after_gap_relay FROM lease_rotation WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let relays = {
            let mut query = tx.prepare(
                "SELECT id,relay FROM planner_relays WHERE enabled=1 ORDER BY id LIMIT 33",
            )?;
            query
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if relays.len() > 32 {
            return Err(LedgerError::Invalid("too many active planner relays"));
        }
        for class in [next, 1 - next] {
            let after = if class == 0 { fresh } else { gap };
            let start = relays.iter().position(|r| r.0 > after).unwrap_or(0);
            for (relay_id, relay) in relays[start..].iter().chain(&relays[..start]) {
                let id = tx
                    .query_row(
                        if class == 0 { FRESH } else { GAP },
                        params![relay, now],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?;
                let Some(id) = id else {
                    continue;
                };
                check_budget(&tx, &self.path, self.limits, 0)?;
                if read_job(&tx, id)?.attempt == i64::MAX {
                    return Err(LedgerError::Invalid("attempt counter exhausted"));
                }
                let token = Keys::generate().secret_key().to_secret_bytes();
                tx.execute("UPDATE jobs SET state='leased',attempt=attempt+1,token=?2,expires_at=?3 WHERE id=?1", params![id,token.as_slice(),expiry])?;
                tx.execute(if class==0 { "UPDATE lease_rotation SET next_class=1,after_fresh_relay=?1 WHERE singleton=1" } else { "UPDATE lease_rotation SET next_class=0,after_gap_relay=?1 WHERE singleton=1" }, [relay_id])?;
                let job = read_job(&tx, id)?;
                commit(tx, &self.path, self.limits)?;
                return Ok(Some(Lease {
                    job,
                    token,
                    expires_at: expiry,
                }));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::jobs::{JobState, LedgerLimits, RetryReason, SplitOutcome};

    const A: &str = "wss://a.example.com";
    const B: &str = "wss://b.example.com";
    const C: &str = "wss://c.example.com";

    fn open(root: &tempfile::TempDir) -> JobLedger {
        JobLedger::open(&root.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap()
    }
    fn rotation(db: &JobLedger) -> (i64, i64, i64) {
        db.db
            .query_row(
                "SELECT next_class,after_fresh_relay,after_gap_relay FROM lease_rotation",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }
    fn block(db: &mut JobLedger, lease: Lease) {
        db.db
            .execute(
                "UPDATE jobs SET state='blocked',token=NULL,expires_at=NULL WHERE id=?1",
                [lease.job.id],
            )
            .unwrap();
    }
    fn job(db: &mut JobLedger, relay: &str, n: i64, gap: bool) -> i64 {
        let id = db.enqueue(&format!("s{n}"), relay, n, n).unwrap().id;
        if gap {
            db.db
                .execute(
                    "UPDATE jobs SET state='retry_wait',attempt=1 WHERE id=?1",
                    [id],
                )
                .unwrap();
        }
        id
    }

    #[test]
    fn fair_classes_have_independent_relay_rotation_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A, B, C]).unwrap();
        let a1 = job(&mut db, A, 1, false);
        let a2 = job(&mut db, A, 2, false);
        let b1 = job(&mut db, B, 3, false);
        let c1 = job(&mut db, C, 4, true);
        let c2 = job(&mut db, C, 5, true);
        let c3 = job(&mut db, C, 6, true);
        for expected in [a1, c1, b1, c2, a2, c3] {
            let lease = db.lease_next_fair(100, 60).unwrap().unwrap();
            assert_eq!(lease.job.id, expected);
            block(&mut db, lease);
            drop(db);
            db = open(&root);
        }
        let before = rotation(&db);
        assert!(db.lease_next_fair(100, 60).unwrap().is_none());
        assert_eq!(rotation(&db), before);
    }

    #[test]
    fn local_dense_candidate_splits_before_attempt_or_rotation() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A]).unwrap();
        let id = db.enqueue("dense", A, 0, 9).unwrap().id;
        let candidate = db.fair_candidate(100).unwrap().unwrap();
        assert_eq!(candidate.id, id);
        assert_eq!(candidate.attempt, 0);
        let children = db.split_unleased(candidate.id).unwrap();
        let SplitOutcome::Children(children) = children else {
            panic!("expected children");
        };
        assert_eq!(db.get(id).unwrap().attempt, 0);
        assert_eq!(rotation(&db), (0, 0, 0));
        assert_eq!((children[0].since, children[0].until), (0, 4));
        assert_eq!((children[1].since, children[1].until), (5, 9));
        let leaf = db.enqueue("leaf", A, 20, 20).unwrap().id;
        assert_eq!(db.split_unleased(leaf).unwrap(), SplitOutcome::Blocked);
        assert_eq!(db.get(leaf).unwrap().attempt, 0);
    }

    #[test]
    fn fair_due_boundaries_fallback_and_disabled_gaps_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A, B]).unwrap();
        let future = job(&mut db, A, 1, false);
        db.db
            .execute("UPDATE jobs SET next_eligible=101 WHERE id=?1", [future])
            .unwrap();
        let gap = job(&mut db, B, 2, true);
        db.configure_planner(&[A]).unwrap();
        assert!(db.lease_next_fair(100, 60).unwrap().is_none());
        assert_eq!(rotation(&db), (0, 0, 0));
        assert_eq!(db.get(gap).unwrap().state, JobState::RetryWait);
        db.configure_planner(&[A, B]).unwrap();
        let lease = db.lease_next_fair(100, 60).unwrap().unwrap();
        assert_eq!(lease.job.id, gap);
        block(&mut db, lease);
        let before = rotation(&db);
        assert!(db.lease_next_fair(99, 60).unwrap().is_none());
        assert_eq!(rotation(&db), before);
        let lease = db.lease_next_fair(101, 60).unwrap().unwrap();
        assert_eq!(lease.job.id, future);
        // Even an expired active lease blocks leasing; recovery owns release.
        assert!(db.lease_next_fair(1000, 60).unwrap().is_none());
    }

    #[test]
    fn fair_split_children_and_requeued_attempts_are_gaps() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A]).unwrap();
        db.enqueue("parent", A, 0, 9).unwrap();
        let lease = db.lease_next_fair(100, 60).unwrap().unwrap();
        db.split(&lease, 101).unwrap();
        let fresh = job(&mut db, A, 10, false);
        let child = db.lease_next_fair(102, 60).unwrap().unwrap();
        assert_eq!(child.job.parent, Some(lease.job.id));
        block(&mut db, child);
        let lease = db.lease_next_fair(102, 60).unwrap().unwrap();
        assert_eq!(lease.job.id, fresh);
        db.retry(&lease, 102, 60, RetryReason::WorkerLost).unwrap();
        db.db
            .execute("UPDATE jobs SET state='queued' WHERE id=?1", [fresh])
            .unwrap();
        let child = db.lease_next_fair(162, 60).unwrap().unwrap();
        assert!(child.job.parent.is_some());
        block(&mut db, child);
        let retried = db.lease_next_fair(162, 60).unwrap().unwrap();
        assert_eq!(retried.job.id, fresh);
        assert_eq!(retried.job.attempt, 2);
    }

    #[test]
    fn fair_selection_errors_roll_back_lease_and_rotation() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A]).unwrap();
        let id = job(&mut db, A, 1, true);
        for (now, ttl) in [(-1, 60), (i64::MAX, 60), (100, 0), (100, 601)] {
            assert!(db.lease_next_fair(now, ttl).is_err());
            assert_eq!(rotation(&db), (0, 0, 0));
        }
        db.db
            .execute(
                "UPDATE jobs SET attempt=?1 WHERE id=?2",
                params![i64::MAX, id],
            )
            .unwrap();
        assert!(matches!(
            db.lease_next_fair(100, 60),
            Err(LedgerError::Invalid(_))
        ));
        db.db
            .execute("UPDATE jobs SET attempt=1 WHERE id=?1", [id])
            .unwrap();
        db.db.execute_batch("CREATE TRIGGER rotation_fault BEFORE UPDATE ON lease_rotation BEGIN SELECT RAISE(ABORT,'rotation fault'); END;").unwrap();
        assert!(matches!(
            db.lease_next_fair(100, 60),
            Err(LedgerError::Database(_))
        ));
        assert_eq!(rotation(&db), (0, 0, 0));
        assert_eq!(db.get(id).unwrap().attempt, 1);
        assert_eq!(db.get(id).unwrap().state, JobState::RetryWait);
        db.db.execute_batch("DROP TRIGGER rotation_fault;").unwrap();
        db.limits.max_bytes = 1;
        assert!(matches!(
            db.lease_next_fair(100, 60),
            Err(LedgerError::Budget)
        ));
        assert_eq!(rotation(&db), (0, 0, 0));
        assert_eq!(db.get(id).unwrap().state, JobState::RetryWait);
    }

    #[test]
    fn fair_selection_respects_per_relay_unresolved_planning_quota() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A, B]).unwrap();
        for n in 0..32 {
            job(&mut db, A, n, true);
        }
        assert_eq!(db.plan_rolling(3600, 1).unwrap().enqueued, 1);
        let fresh = db.lease_next_fair(3600, 60).unwrap().unwrap();
        assert_eq!(fresh.job.relay, B);
        block(&mut db, fresh);
        let gap = db.lease_next_fair(3600, 60).unwrap().unwrap();
        assert_eq!(gap.job.relay, A);
        db.retry(&gap, 3600, 60, RetryReason::WorkerLost).unwrap();
        // Releasing the failed lease must not reopen its relay's root quota.
        assert_eq!(db.plan_rolling(3600, 1).unwrap().enqueued, 1);
        assert_eq!(
            db.db
                .query_row(
                    "SELECT count(*) FROM jobs WHERE relay=?1 AND parent IS NULL",
                    [A],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            32
        );
    }

    #[test]
    fn fair_due_queries_use_partial_indexes_without_sorting() {
        let root = tempfile::tempdir().unwrap();
        let db = open(&root);
        for (sql, index) in [(FRESH, "fresh_due"), (GAP, "gap_due")] {
            let mut q = db.db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let detail = q
                .query_map(params![A, 100], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .join(" ");
            assert!(detail.contains(index), "{detail}");
            assert!(!detail.contains("TEMP B-TREE"), "{detail}");
        }
    }

    #[test]
    fn fair_old_schema_is_rejected_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        job(&mut db, A, 1, true);
        db.db.pragma_update(None, "user_version", 6).unwrap();
        drop(db);
        let path = root.path().join("jobs.sqlite");
        assert!(JobLedger::open(&path, LedgerLimits::default()).is_err());
        let raw = rusqlite::Connection::open(path).unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            6
        );
        assert_eq!(
            raw.query_row(
                "SELECT count(*) FROM jobs WHERE state='retry_wait'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn fair_orders_due_deadlines_before_ids_and_rejects_excess_enabled_relays() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A]).unwrap();
        let later = job(&mut db, A, 1, true);
        let first = job(&mut db, A, 2, true);
        db.db
            .execute("UPDATE jobs SET next_eligible=50 WHERE id=?1", [later])
            .unwrap();
        let lease = db.lease_next_fair(100, 60).unwrap().unwrap();
        assert_eq!(lease.job.id, first);
        block(&mut db, lease);
        let before = rotation(&db);
        for n in 0..32 {
            db.db
                .execute(
                    "INSERT INTO planner_relays(relay,enabled) VALUES(?1,1)",
                    [format!("wss://extra{n}.example")],
                )
                .unwrap();
        }
        assert!(matches!(
            db.lease_next_fair(100, 60),
            Err(LedgerError::Invalid(_))
        ));
        assert_eq!(rotation(&db), before);
        assert_eq!(db.get(later).unwrap().state, JobState::RetryWait);
    }

    #[test]
    fn fair_awaiting_gate_and_legacy_selector_remain_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root);
        db.configure_planner(&[A]).unwrap();
        for n in 1..=3 {
            job(&mut db, A, n, false);
        }
        for _ in 0..2 {
            let lease = db.lease_next_fair(100, 60).unwrap().unwrap();
            db.record_protocol_done(&lease, 1, 0, crate::sync::ipc::initial_digest(), 101)
                .unwrap();
        }
        let before = rotation(&db);
        assert!(db.lease_next_fair(102, 60).unwrap().is_none());
        assert_eq!(rotation(&db), before);
        let other = tempfile::tempdir().unwrap();
        let mut old = open(&other);
        let id = job(&mut old, A, 1, false);
        assert!(old.lease_next_fair(100, 60).unwrap().is_none());
        assert_eq!(old.lease_next(100, 60).unwrap().unwrap().job.id, id);
        assert_eq!(rotation(&old), (0, 0, 0));
    }
}
