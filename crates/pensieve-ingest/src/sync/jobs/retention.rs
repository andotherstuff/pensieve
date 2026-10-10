//! Bounded retention of successful rolling work; never deletion of obligations.
//!
//! A complete split tree is the smallest deletion unit. Unfinished trees and
//! exclusion-proof owners are deliberately pinned, even under admission pressure.

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{JobLedger, LedgerError};

const MAX_TREE: usize = 2048;
const MAX_DETAIL_ROWS: i64 = 4096;
const MAX_DETAIL_BYTES: i64 = 8 * 1024 * 1024;
const MAX_SUMMARIES: i64 = 16384;
const DETAIL_SECONDS: i64 = 24 * 60 * 60;
const SUMMARY_SECONDS: i64 = 90 * 24 * 60 * 60;
type RootMetadata = (String, String, i64, i64, Option<i64>, Option<Vec<u8>>);

pub(super) const SCHEMA: &str = "
ALTER TABLE jobs ADD COLUMN completed_at INTEGER;
ALTER TABLE jobs ADD COLUMN recorded_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN policy_identity BLOB;
CREATE TABLE lifecycle (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 high_job INTEGER NOT NULL, scan_root INTEGER NOT NULL DEFAULT 0,
 last_success INTEGER, activated_at INTEGER NOT NULL
);
INSERT INTO lifecycle(singleton,high_job,activated_at) SELECT 1,coalesce(max(id),0),unixepoch() FROM jobs;
CREATE INDEX retention_roots ON jobs(id) WHERE parent IS NULL AND state='complete';
CREATE INDEX pending_recorded ON jobs(recorded_at) WHERE state!='complete';
CREATE INDEX exclusion_owner ON policy_exclusions(first_job);
CREATE TABLE completed_coverage (
 relay TEXT NOT NULL, day INTEGER NOT NULL, policy_identity BLOB NOT NULL,
 windows BLOB NOT NULL CHECK(length(windows)=12),
 received INTEGER NOT NULL, archived INTEGER NOT NULL, excluded INTEGER NOT NULL,
 lease_grants INTEGER NOT NULL, failures INTEGER NOT NULL,
 compacted_at INTEGER NOT NULL,
 PRIMARY KEY(relay,day,policy_identity)
) WITHOUT ROWID;
CREATE INDEX coverage_age ON completed_coverage(compacted_at);
CREATE TRIGGER job_recorded AFTER INSERT ON jobs BEGIN
 UPDATE jobs SET recorded_at=unixepoch(),
 policy_identity=(SELECT fingerprint FROM admission_policy WHERE singleton=1)
 WHERE id=NEW.id;
END;
CREATE TRIGGER job_completed AFTER UPDATE OF state ON jobs
WHEN NEW.state='complete' AND OLD.state!='complete' BEGIN
 UPDATE jobs SET completed_at=unixepoch() WHERE id=NEW.id;
 UPDATE lifecycle SET last_success=unixepoch() WHERE singleton=1;
END;
";

pub(super) fn next_id(tx: &Transaction<'_>) -> Result<i64, LedgerError> {
    let high: i64 = tx.query_row("SELECT high_job FROM lifecycle", [], |r| r.get(0))?;
    let id = high
        .checked_add(1)
        .ok_or(LedgerError::Invalid("job ID exhausted"))?;
    tx.execute("UPDATE lifecycle SET high_job=?1 WHERE singleton=1", [id])?;
    Ok(id)
}

/// One bounded cleanup turn. A pinned or oversized tree is not an error or permission
/// to discard proof; capacity monitoring must surface continued accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionProgress {
    /// Complete job rows removed atomically with their successful coverage summary.
    pub removed_jobs: u32,
    /// A candidate was retained because it was unsafe, too recent or too large.
    pub retained_candidate: bool,
}

impl JobLedger {
    /// Sample restart-stable progress on the owner, never from event timestamps.
    pub fn progress_observation(
        &self,
        now: i64,
    ) -> Result<pensieve_core::reconciliation_health::ProgressObservation, LedgerError> {
        let (last_success, activated): (Option<i64>, i64) =
            self.db
                .query_row("SELECT last_success,activated_at FROM lifecycle", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        let mut pending: Option<i64> = self.db.query_row(
            "SELECT min(recorded_at) FROM jobs WHERE state!='complete' AND recorded_at>0",
            [],
            |r| r.get(0),
        )?;
        // Schema 8 never recorded creation time. Zero is unknown, not the
        // epoch: use the persisted upgrade observation as a one-time grace
        // baseline. Reopening the ledger cannot refresh this deadline.
        let unknown_pending: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE state!='complete' AND recorded_at=0)",
            [],
            |r| r.get(0),
        )?;
        if unknown_pending {
            pending = Some(pending.map_or(activated, |known| known.min(activated)));
        }
        let planning_due: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM planner_relays WHERE enabled=1 AND (next<upper OR upper!=?1))",
            [now / 900 * 900], |r| r.get(0),
        )?;
        if pending.is_none() && planning_due {
            pending = Some(last_success.unwrap_or(activated));
        }
        let persistent_failure = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE state='blocked'
             OR (state IN ('queued','leased','awaiting_durability','retry_wait') AND attempt>=3))",
            [],
            |r| r.get(0),
        )?;
        let retained: u32 = self
            .db
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))?;
        metrics::gauge!("negentropy_retained_jobs").set(f64::from(retained));
        let reserve = 2048.min(self.limits.max_jobs / 10);
        let capacity_exhausted = match super::check_budget(&self.db, &self.path, self.limits, 0) {
            Ok(()) => retained >= self.limits.max_jobs.saturating_sub(reserve),
            Err(LedgerError::Budget) => true,
            Err(error) => return Err(error),
        };
        Ok(pensieve_core::reconciliation_health::ProgressObservation {
            observed_at: now,
            pending_since: pending,
            last_success,
            persistent_failure,
            capacity_exhausted,
        })
    }

    /// Compact at most one complete tree per turn and expire at most 32 summaries.
    /// Keep 24 hours of successful detail and 90 days of compacted observations.
    /// The 16,384-summary ceiling is independent of the physical admission budget.
    pub fn retain_completed(&mut self, now: i64) -> Result<RetentionProgress, LedgerError> {
        if now < 0 {
            return Err(LedgerError::Invalid("invalid retention time"));
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM completed_coverage WHERE (relay,day,policy_identity) IN
             (SELECT relay,day,policy_identity FROM completed_coverage
              WHERE compacted_at<?1 ORDER BY compacted_at LIMIT 32)",
            [now.saturating_sub(SUMMARY_SECONDS)],
        )?;
        let after: i64 = tx.query_row("SELECT scan_root FROM lifecycle", [], |r| r.get(0))?;
        let root: Option<i64> = tx
            .query_row(
                "SELECT id FROM jobs INDEXED BY retention_roots
             WHERE parent IS NULL AND state='complete' AND id>?1 ORDER BY id LIMIT 1",
                [after],
                |r| r.get(0),
            )
            .optional()?;
        let Some(root) = root else {
            tx.execute("UPDATE lifecycle SET scan_root=0", [])?;
            tx.commit()?;
            return Ok(RetentionProgress {
                removed_jobs: 0,
                retained_candidate: false,
            });
        };
        tx.execute("UPDATE lifecycle SET scan_root=?1", [root])?;
        let removed = compact_tree(&tx, root, now)?;
        tx.commit()?;
        Ok(RetentionProgress {
            removed_jobs: removed,
            retained_candidate: removed == 0,
        })
    }
}

#[cfg(test)]
pub(super) fn strip_for_legacy_fixture(db: &rusqlite::Connection) {
    db.execute_batch(
        "DROP TRIGGER job_recorded; DROP TRIGGER job_completed;
        DROP TABLE lifecycle; DROP TABLE completed_coverage;
        DROP INDEX retention_roots; DROP INDEX pending_recorded; DROP INDEX exclusion_owner;
        ALTER TABLE jobs DROP COLUMN completed_at; ALTER TABLE jobs DROP COLUMN recorded_at;
        ALTER TABLE jobs DROP COLUMN policy_identity; PRAGMA user_version=8;",
    )
    .unwrap();
}

fn compact_tree(tx: &Transaction<'_>, root: i64, now: i64) -> Result<u32, LedgerError> {
    let (sweep, relay, since, until, completed, policy): RootMetadata = tx.query_row(
        "SELECT sweep,relay,since,until,completed_at,policy_identity FROM jobs WHERE id=?1",
        [root],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    if sweep
        .strip_prefix("rolling-v1:")
        .and_then(|s| s.parse::<i64>().ok())
        .is_none()
    {
        return Ok(0);
    }
    let Some(policy) = policy.filter(|p| p.len() == 32) else {
        return Ok(0);
    };
    if since % 900 != 0
        || until != since.saturating_add(899)
        || completed.is_none_or(|t| t > now.saturating_sub(DETAIL_SECONDS))
    {
        return Ok(0);
    }
    // Planners resume from their persisted next window, whether enabled or
    // disabled; completed roots cannot be recreated in this reserved namespace.
    // Other roots' gaps do not pin this tree's completed lineage.
    let ids = {
        let mut query = tx.prepare(
            "WITH RECURSIVE tree(id) AS (SELECT ?1 UNION ALL
             SELECT j.id FROM jobs j JOIN tree t ON j.parent=t.id LIMIT ?2)
             SELECT id FROM tree",
        )?;
        query
            .query_map(params![root, (MAX_TREE + 1) as i64], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    if ids.len() > MAX_TREE {
        return Ok(0);
    }
    let mut totals = [0_i64; 5];
    let mut detail_rows = ids.len() as i64;
    let mut detail_bytes = 0_i64;
    for id in &ids {
        let pinned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM receipts WHERE job=?1)
             OR EXISTS(SELECT 1 FROM policy_exclusions WHERE first_job=?1)
             OR EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND
               (state!='complete' OR completed_at IS NULL OR completed_at>?2
                OR policy_identity IS NULL OR policy_identity!=?3))",
            params![id, now.saturating_sub(DETAIL_SECONDS), policy],
            |r| r.get(0),
        )?;
        if pinned {
            return Ok(0);
        }
        // LIMIT bounds history inspection even for a job retried for years.
        let (attempts, received, archived, excluded): (i64,i64,i64,i64) = tx.query_row(
            "SELECT count(*),coalesce(sum(received),0),coalesce(sum(archived),0),coalesce(sum(excluded),0)
             FROM (SELECT received,archived,excluded FROM attempts WHERE job=?1 LIMIT ?2)",
            params![id, MAX_DETAIL_ROWS + 1], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        )?;
        let (failures, bytes): (i64, i64) = tx.query_row(
            "SELECT count(*),coalesce(sum(length(report)),0)
             FROM (SELECT report FROM failure_reports WHERE job=?1 LIMIT ?2)",
            params![id, MAX_DETAIL_ROWS + 1],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        detail_rows += attempts + failures;
        detail_bytes += bytes + attempts * 128 + 4096;
        if detail_rows > MAX_DETAIL_ROWS || detail_bytes > MAX_DETAIL_BYTES {
            return Ok(0);
        }
        let grants: i64 =
            tx.query_row("SELECT attempt FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        for (total, value) in totals
            .iter_mut()
            .zip([received, archived, excluded, grants, failures])
        {
            *total = total
                .checked_add(value)
                .ok_or(LedgerError::Invalid("retention counter overflow"))?;
        }
    }
    let day = since / 86400;
    let prior: Option<(Vec<u8>, [i64;5])> = tx.query_row(
        "SELECT windows,received,archived,excluded,lease_grants,failures FROM completed_coverage
         WHERE relay=?1 AND day=?2 AND policy_identity=?3",
        params![relay, day, policy], |r| Ok((r.get(0)?, [r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?])),
    ).optional()?;
    let mut windows = vec![0_u8; 12];
    match prior {
        Some((mask, old)) => {
            windows = mask;
            for (total, value) in totals.iter_mut().zip(old) {
                *total = total
                    .checked_add(value)
                    .ok_or(LedgerError::Invalid("coverage counter overflow"))?;
            }
        }
        None => {
            let count: i64 =
                tx.query_row("SELECT count(*) FROM completed_coverage", [], |r| r.get(0))?;
            if count >= MAX_SUMMARIES {
                return Ok(0);
            }
        }
    }
    if windows.len() != 12 {
        return Err(LedgerError::Invalid("invalid coverage bitmap"));
    }
    let slot = (since % 86400 / 900) as usize;
    windows[slot / 8] |= 1 << (slot % 8);
    tx.execute(
        "INSERT INTO completed_coverage VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(relay,day,policy_identity) DO UPDATE SET windows=excluded.windows,
         received=excluded.received,archived=excluded.archived,excluded=excluded.excluded,
         lease_grants=excluded.lease_grants,failures=excluded.failures,compacted_at=excluded.compacted_at",
        params![relay,day,policy,windows,totals[0],totals[1],totals[2],totals[3],totals[4],now],
    )?;
    // Deferred checks permit deleting a whole tree without relying on traversal order.
    tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
    for id in &ids {
        tx.execute("DELETE FROM failure_reports WHERE job=?1", [id])?;
        tx.execute("DELETE FROM attempts WHERE job=?1", [id])?;
        tx.execute("DELETE FROM jobs WHERE id=?1", [id])?;
    }
    Ok(ids.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::jobs::LedgerLimits;

    fn closed_sweep(dir: &tempfile::TempDir) -> JobLedger {
        let mut db =
            JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        db.configure_planner(&["wss://relay.example"]).unwrap();
        assert_eq!(db.plan_rolling(1800, 2).unwrap().enqueued, 2);
        // Stand-in for archive completion, confined to retention unit tests.
        db.db
            .execute("UPDATE jobs SET state='complete'", [])
            .unwrap();
        db.db.execute("UPDATE jobs SET completed_at=1", []).unwrap();
        assert_eq!(db.plan_rolling(2700, 1).unwrap().enqueued, 1);
        db
    }

    #[test]
    fn compaction_is_atomic_counted_and_never_reuses_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
        let (mask, grants): (Vec<u8>, i64) = db
            .db
            .query_row(
                "SELECT windows,lease_grants FROM completed_coverage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mask[0], 3);
        assert_eq!(grants, 0);
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 0);
        // Even deletion of the highest ID cannot reset the durable allocator.
        db.db.execute("DELETE FROM jobs WHERE id=3", []).unwrap();
        drop(db);
        let mut db =
            JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        assert_eq!(
            db.enqueue("manual", "wss://relay.example", 0, 10)
                .unwrap()
                .id,
            4
        );
    }

    #[test]
    fn unresolved_tree_survives_while_completed_sibling_is_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db
            .execute("UPDATE jobs SET state='retry_wait' WHERE id=2", [])
            .unwrap();
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
        assert!(db.get(1).is_err());
        assert!(db.get(2).is_ok());
        db.db
            .execute("UPDATE jobs SET state='complete' WHERE id=3", [])
            .unwrap();
        db.db
            .execute("UPDATE jobs SET completed_at=1 WHERE id=3", [])
            .unwrap();
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
        assert!(db.get(3).is_err());
        assert!(db.get(2).is_ok());
        assert_eq!(db.plan_rolling(2700, 1).unwrap().enqueued, 1);
        assert_eq!(db.get(4).unwrap().since, 900);
    }

    #[test]
    fn disabled_relay_completion_compacts_without_recreating_windows_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db
            .execute("UPDATE jobs SET state='complete' WHERE id=3", [])
            .unwrap();
        db.db.execute("UPDATE jobs SET completed_at=1", []).unwrap();
        db.configure_planner(&[]).unwrap();
        for _ in 0..3 {
            assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
        }
        assert!(db.get(3).is_err());
        db.configure_planner(&["wss://relay.example"]).unwrap();
        assert_eq!(db.plan_rolling(2700, 2).unwrap().enqueued, 2);
        // The disabled partial sweep resumes after its already-planned [0,899].
        assert_eq!(db.get(4).unwrap().since, 900);
        assert_eq!(db.get(5).unwrap().since, 1800);
    }

    #[test]
    fn deletion_fault_rolls_back_summary_and_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db.execute_batch("CREATE TRIGGER prevent_delete BEFORE DELETE ON jobs BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(db.retain_completed(100_000).is_err());
        assert!(db.get(1).is_ok());
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM completed_coverage", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.db
                .query_row("SELECT scan_root FROM lifecycle", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn proof_owner_and_unknown_completion_are_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db.execute("INSERT INTO attempts(job,attempt,received,bytes,digest,protocol_done,archived,excluded) VALUES(1,1,1,10,zeroblob(32),1,0,1)", []).unwrap();
        db.db
            .execute(
                "INSERT INTO policy_exclusions VALUES(zeroblob(32),1,1,1)",
                [],
            )
            .unwrap();
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 0);
        db.db
            .execute("UPDATE jobs SET completed_at=NULL WHERE id=2", [])
            .unwrap();
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 0);
        assert_eq!(
            db.db
                .query_row("SELECT retained FROM exclusion_totals", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn complete_split_tree_is_deleted_together_and_accounting_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db
            .execute("UPDATE jobs SET state='queued' WHERE id=1", [])
            .unwrap();
        let super::super::SplitOutcome::Children(children) = db.split_unleased(1).unwrap() else {
            panic!("expected split");
        };
        for child in children.iter() {
            db.db.execute("INSERT INTO attempts(job,attempt,received,bytes,digest,protocol_done,archived,excluded) VALUES(?1,1,2,10,zeroblob(32),1,2,0)", [child.id]).unwrap();
            db.db
                .execute(
                    "UPDATE jobs SET state='complete',attempt=1 WHERE id=?1",
                    [child.id],
                )
                .unwrap();
        }
        db.db
            .execute("UPDATE jobs SET state='complete' WHERE id=1", [])
            .unwrap();
        db.db
            .execute("UPDATE jobs SET completed_at=1 WHERE state='complete'", [])
            .unwrap();
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 3);
        assert!(db.get(1).is_err());
        drop(db);
        let db = JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        let totals: (i64, i64, i64) = db
            .db
            .query_row(
                "SELECT received,archived,lease_grants FROM completed_coverage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(totals, (4, 4, 2));
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
    }

    #[test]
    fn migration_is_transactional_and_legacy_completion_is_not_invented() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let db = closed_sweep(&dir);
        strip_for_legacy_fixture(&db.db);
        drop(db);
        let too_small = LedgerLimits {
            max_bytes: 1,
            ..LedgerLimits::default()
        };
        assert!(JobLedger::open(&path, too_small).is_err());
        let raw = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            8
        );
        assert_eq!(
            raw.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='lifecycle'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(raw);
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        assert_eq!(db.retain_completed(i64::MAX).unwrap().removed_jobs, 0);
        assert_eq!(
            db.enqueue("new", "wss://relay.example", 1, 2).unwrap().id,
            4
        );
        assert_eq!(db.progress_observation(100_000).unwrap().last_success, None);
    }

    #[test]
    fn allocator_overflow_does_not_mutate_jobs_and_retention_uses_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db
            .execute("UPDATE lifecycle SET high_job=?1", [i64::MAX])
            .unwrap();
        assert!(db.enqueue("new", "wss://relay.example", 5, 6).is_err());
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        for (query, index) in [
            (
                "EXPLAIN QUERY PLAN SELECT id FROM jobs INDEXED BY retention_roots WHERE parent IS NULL AND state='complete' AND id>0 ORDER BY id LIMIT 1",
                "retention_roots",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT min(recorded_at) FROM jobs WHERE state!='complete' AND recorded_at>0",
                "pending_recorded",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM jobs WHERE state!='complete' AND recorded_at=0)",
                "pending_recorded",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT 1 FROM policy_exclusions WHERE first_job=1",
                "exclusion_owner",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM jobs WHERE state='blocked' OR (state IN ('queued','leased','awaiting_durability','retry_wait') AND attempt>=3))",
                "due_jobs",
            ),
        ] {
            let details = db
                .db
                .prepare(query)
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .join(" ");
            assert!(details.contains(index), "{details}");
            assert!(!details.contains("TEMP B-TREE"), "{details}");
        }
    }

    #[test]
    fn legacy_pending_grace_is_persisted_without_inventing_a_completion() {
        use pensieve_core::reconciliation_health::ReconciliationHealth;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        db.enqueue("legacy", "wss://relay.example", 0, 899).unwrap();
        strip_for_legacy_fixture(&db.db);
        drop(db);
        let db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        let activated: i64 = db
            .db
            .query_row("SELECT activated_at FROM lifecycle", [], |r| r.get(0))
            .unwrap();
        let observation = db.progress_observation(activated).unwrap();
        assert_eq!(observation.pending_since, Some(activated));
        assert_eq!(observation.last_success, None);
        let health = ReconciliationHealth::default();
        health.enable();
        health.observe(observation);
        assert!(health.status(activated).0);
        drop(db);
        let db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        health.observe(db.progress_observation(activated + 3600).unwrap());
        assert_eq!(
            health.status(activated + 3600),
            (false, "durable_progress_stalled")
        );
        assert_eq!(
            db.db
                .query_row("SELECT recorded_at FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn detail_bound_and_summary_expiry_are_bounded_without_dropping_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = closed_sweep(&dir);
        db.db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4097) INSERT INTO attempts(job,attempt,received,bytes,digest,protocol_done,archived) SELECT 1,x,0,0,zeroblob(32),1,0 FROM n;").unwrap();
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 0);
        assert!(db.get(1).is_ok());
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
        db.db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<40) INSERT INTO completed_coverage SELECT 'wss://old.example',x,zeroblob(32),zeroblob(12),0,0,0,0,0,1 FROM n;").unwrap();
        db.retain_completed(SUMMARY_SECONDS + 2).unwrap();
        assert_eq!(
            db.db
                .query_row(
                    "SELECT count(*) FROM completed_coverage WHERE relay='wss://old.example'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            8
        );
        assert!(db.get(3).is_ok());
    }

    #[test]
    fn persisted_failures_and_capacity_clear_only_after_real_state_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let db = closed_sweep(&dir);
        db.db
            .execute(
                "UPDATE jobs SET state='retry_wait',attempt=3,recorded_at=100 WHERE id=3",
                [],
            )
            .unwrap();
        drop(db);
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        let o = db.progress_observation(4000).unwrap();
        assert!(o.persistent_failure);
        assert_eq!(o.pending_since, Some(100));
        db.db
            .execute("UPDATE jobs SET state='complete' WHERE id=3", [])
            .unwrap();
        assert!(!db.progress_observation(4001).unwrap().persistent_failure);
        db.limits.max_jobs = 3;
        assert!(db.progress_observation(4001).unwrap().capacity_exhausted);
        db.limits.max_bytes = 1;
        assert!(db.progress_observation(4001).unwrap().capacity_exhausted);
        // Cleanup is recovery: it remains usable above the admission budget.
        assert_eq!(db.retain_completed(100_000).unwrap().removed_jobs, 1);
    }
}
