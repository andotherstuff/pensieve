//! Inactive bounded rolling-window planning. No leasing, pruning or runtime work.

use std::collections::BTreeSet;

use rusqlite::{TransactionBehavior, params};

use super::{JobLedger, LedgerError, check_budget, commit, enqueue_window};

const MAX_ACTIVE: usize = 32;
const MAX_RETAINED: i64 = 128;
const MAX_QUEUED_ROOTS: u32 = 32;
const WINDOW: i64 = 900;
const HORIZON: i64 = 14 * 24 * 60 * 60;

pub(super) const SCHEMA: &str = "
CREATE TABLE planner_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 sequence INTEGER NOT NULL CHECK(sequence>=0),
 after_relay INTEGER NOT NULL CHECK(after_relay>=0)
);
INSERT INTO planner_state VALUES(1,0,0);
CREATE TABLE planner_relays (
 id INTEGER PRIMARY KEY, relay TEXT NOT NULL UNIQUE,
 enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
 sequence INTEGER NOT NULL DEFAULT 0 CHECK(sequence>=0),
 upper INTEGER NOT NULL DEFAULT 0 CHECK(upper>=0),
 next INTEGER NOT NULL DEFAULT 0 CHECK(next>=0 AND next<=upper)
);
CREATE INDEX queued_roots ON jobs(id) WHERE state='queued' AND parent IS NULL;
";

/// Outcome of one atomic planning turn, not completed relay coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanningProgress {
    /// Root windows enqueued in this turn (at most 32).
    pub enqueued: u32,
    /// The enabled-relay queued-root ceiling prevents further planning.
    pub backpressured: bool,
}

impl JobLedger {
    /// Replace the explicit planning allowlist (at most 32 input entries).
    /// Canonical duplicates collapse. Empty disables planning. Removed relays'
    /// cursors and jobs remain; re-adding resumes them. At most 128 relay identities
    /// are retained for the ledger lifetime; churn beyond that fails atomically.
    pub fn configure_planner(&mut self, relays: &[&str]) -> Result<(), LedgerError> {
        if relays.len() > MAX_ACTIVE {
            return Err(LedgerError::Invalid("too many planner relays"));
        }
        let relays = relays
            .iter()
            .map(|relay| {
                if relay.len() > 2048 {
                    return Err(LedgerError::Invalid("planner relay too long"));
                }
                crate::relay::normalize_relay_url(relay)
                    .ok()
                    .ok_or(LedgerError::Invalid("invalid planner relay"))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut removal_only = true;
        for relay in &relays {
            let enabled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM planner_relays WHERE relay=?1 AND enabled=1)",
                [relay],
                |r| r.get(0),
            )?;
            removal_only &= enabled;
        }
        if !removal_only {
            check_budget(&tx, &self.path, self.limits, 0)?;
        }
        tx.execute("UPDATE planner_relays SET enabled=0 WHERE enabled=1", [])?;
        for relay in relays {
            tx.execute("INSERT INTO planner_relays(relay,enabled) VALUES(?1,1) ON CONFLICT(relay) DO UPDATE SET enabled=1", [relay])?;
        }
        let retained: i64 =
            tx.query_row("SELECT count(*) FROM planner_relays", [], |r| r.get(0))?;
        if retained > MAX_RETAINED {
            return Err(LedgerError::Invalid(
                "retained planner relay ceiling reached",
            ));
        }
        if removal_only {
            // Disabling work is recovery, not admission. Actual storage errors
            // still propagate; budget exhaustion must not prevent disabling.
            tx.commit()?;
            Ok(())
        } else {
            commit(tx, &self.path, self.limits)
        }
    }

    /// Plan at most 32 windows, lazily, against a caller-supplied Unix time.
    /// Future owner wiring must obtain time at execution, not queue submission.
    /// Sweep upper bounds freeze at the latest completed 15-minute boundary.
    /// A backward clock resumes frozen work but cannot create an older sweep.
    /// Root-job backpressure counts currently enabled relay identities only;
    /// disabled obligations remain under lifetime ledger budgets. Failures roll back this
    /// entire turn, including sweep sequence and round-robin/cursor progress.
    pub fn plan_rolling(
        &mut self,
        now: i64,
        max_jobs: u32,
    ) -> Result<PlanningProgress, LedgerError> {
        if now < 0 || max_jobs == 0 || max_jobs > MAX_QUEUED_ROOTS {
            return Err(LedgerError::Invalid("invalid planner bounds"));
        }
        let boundary = now / WINDOW * WINDOW;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let queued: u32 = tx.query_row(
            "SELECT count(*) FROM jobs WHERE state='queued' AND parent IS NULL AND relay IN (SELECT relay FROM planner_relays WHERE enabled=1)",
            [],
            |r| r.get(0),
        )?;
        let mut progress = PlanningProgress {
            enqueued: 0,
            backpressured: queued >= MAX_QUEUED_ROOTS,
        };
        if progress.backpressured {
            return Ok(progress);
        }
        let (mut sequence, mut after): (i64, i64) = tx.query_row(
            "SELECT sequence,after_relay FROM planner_state WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut relays = {
            let mut query = tx.prepare("SELECT id,relay,sequence,upper,next FROM planner_relays WHERE enabled=1 ORDER BY id LIMIT 33")?;
            query
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        if relays.len() > MAX_ACTIVE {
            return Err(LedgerError::Invalid("too many active planner relays"));
        }
        if relays.is_empty() {
            return Ok(progress);
        }
        let start = relays.iter().position(|r| r.0 > after).unwrap_or(0);
        relays.rotate_left(start);
        let mut idle = 0;
        let mut index = 0;
        let limit = max_jobs.min(MAX_QUEUED_ROOTS - queued);
        while progress.enqueued < limit && idle < relays.len() {
            let (id, relay, sweep_sequence, upper, next) = &mut relays[index];
            if *next == *upper && boundary > *upper {
                sequence = sequence
                    .checked_add(1)
                    .ok_or(LedgerError::Invalid("planner sequence exhausted"))?;
                *sweep_sequence = sequence;
                *upper = boundary;
                *next = boundary.saturating_sub(HORIZON).max(0);
            }
            if *next < *upper {
                let until = next.saturating_add(WINDOW - 1).min(*upper - 1);
                enqueue_window(
                    &tx,
                    &self.path,
                    self.limits,
                    &format!("rolling-v1:{sweep_sequence}"),
                    relay,
                    *next,
                    until,
                )?;
                *next = until + 1;
                tx.execute(
                    "UPDATE planner_relays SET sequence=?2,upper=?3,next=?4 WHERE id=?1",
                    params![*id, *sweep_sequence, *upper, *next],
                )?;
                after = *id;
                progress.enqueued += 1;
                idle = 0;
            } else {
                idle += 1;
            }
            index = (index + 1) % relays.len();
        }
        if progress.enqueued > 0 {
            tx.execute(
                "UPDATE planner_state SET sequence=?1,after_relay=?2 WHERE singleton=1",
                params![sequence, after],
            )?;
            commit(tx, &self.path, self.limits)?;
        }
        progress.backpressured = queued + progress.enqueued >= MAX_QUEUED_ROOTS;
        Ok(progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::jobs::{LedgerLimits, RetryReason};

    fn open(dir: &tempfile::TempDir) -> JobLedger {
        JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap()
    }

    fn cursors(db: &JobLedger) -> Vec<(String, i64, i64, i64, i64)> {
        db.db
            .prepare("SELECT relay,enabled,sequence,upper,next FROM planner_relays ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn state(db: &JobLedger) -> (i64, i64) {
        db.db
            .query_row("SELECT sequence,after_relay FROM planner_state", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
    }

    fn windows(db: &JobLedger) -> Vec<(String, String, i64, i64)> {
        db.db
            .prepare("SELECT sweep,relay,since,until FROM jobs ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn frozen_boundaries_reopen_and_backwards_clock_preserve_round_robin() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://b.example", "wss://a.example/", "wss://a.example"])
            .unwrap();
        let now = HORIZON + 2 * WINDOW + 123;
        assert_eq!(db.plan_rolling(now, 1).unwrap().enqueued, 1);
        let first = windows(&db)[0].clone();
        assert_eq!((first.2, first.3), (1800, 2699));
        assert_eq!(cursors(&db).len(), 2);
        drop(db);
        let mut db = open(&dir);
        // New relay gets its own frozen boundary; first relay retains its old one.
        db.plan_rolling(now, 1).unwrap();
        db.plan_rolling(0, 2).unwrap();
        let rows = windows(&db);
        assert_eq!(
            rows.iter().map(|r| r.1.clone()).collect::<Vec<_>>(),
            vec![
                "wss://a.example",
                "wss://b.example",
                "wss://a.example",
                "wss://b.example"
            ]
        );
        assert_eq!((rows[2].2, rows[2].3), (2700, 3599));
        assert_eq!(rows[0].0, rows[2].0);
        assert_ne!(rows[0].0, rows[1].0);
    }

    #[test]
    fn full_fourteen_day_sweep_has_1344_contiguous_roots_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        let upper = 1_800_000_000;
        for turn in 0..42 {
            assert_eq!(db.plan_rolling(upper + 899, 32).unwrap().enqueued, 32);
            // Test-only scheduler stand-in, never a production completion path.
            db.db
                .execute("UPDATE jobs SET state='complete' WHERE state='queued'", [])
                .unwrap();
            if turn == 20 {
                drop(db);
                db = open(&dir);
            }
        }
        assert_eq!(db.plan_rolling(upper, 32).unwrap().enqueued, 0);
        let rows = windows(&db);
        assert_eq!(rows.len(), 1344);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.0, rows[0].0);
            let since = upper - HORIZON + index as i64 * WINDOW;
            assert_eq!((row.2, row.3), (since, since + WINDOW - 1));
        }
        assert_eq!(rows.last().unwrap().3, upper - 1);
        assert_eq!(db.plan_rolling(upper + WINDOW, 1).unwrap().enqueued, 1);
        let next = windows(&db).pop().unwrap();
        assert_ne!(next.0, rows[0].0);
        assert_eq!(next.2, upper + WINDOW - HORIZON);
    }

    #[test]
    fn completed_boundaries_are_revisited_only_after_upper_advances() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        assert_eq!(db.plan_rolling(899, 32).unwrap().enqueued, 0);
        assert_eq!(db.plan_rolling(1799, 32).unwrap().enqueued, 1);
        assert_eq!(db.plan_rolling(900, 32).unwrap().enqueued, 0);
        assert_eq!(db.plan_rolling(0, 32).unwrap().enqueued, 0);
        assert_eq!(db.plan_rolling(1800, 32).unwrap().enqueued, 2);
        let rows = windows(&db);
        assert_eq!((rows[0].2, rows[0].3), (0, 899));
        assert_eq!((rows[1].2, rows[1].3), (0, 899));
        assert_eq!((rows[2].2, rows[2].3), (900, 1799));
        assert_ne!(rows[0].0, rows[1].0);
    }

    #[test]
    fn queue_ceiling_preserves_cursor_and_old_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        assert!(db.plan_rolling(HORIZON, 32).unwrap().backpressured);
        let before = (state(&db), cursors(&db), windows(&db));
        assert_eq!(
            db.plan_rolling(HORIZON * 2, 32).unwrap(),
            PlanningProgress {
                enqueued: 0,
                backpressured: true
            }
        );
        assert_eq!((state(&db), cursors(&db), windows(&db)), before);
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.retry(&lease, 11, 60, RetryReason::WorkerLost).unwrap();
        assert_eq!(db.plan_rolling(HORIZON * 2, 32).unwrap().enqueued, 1);
        assert_eq!(
            db.get(lease.job().id).unwrap().state,
            crate::sync::jobs::JobState::RetryWait
        );
        assert_eq!(cursors(&db)[0].3, HORIZON);
    }

    #[test]
    fn disabled_backlog_does_not_block_replacement_and_disable_bypasses_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://old.example"]).unwrap();
        db.plan_rolling(HORIZON, 32).unwrap();
        let old = windows(&db);
        db.limits.max_bytes = 1;
        db.configure_planner(&[]).unwrap();
        assert_eq!(windows(&db), old);
        assert_eq!(cursors(&db)[0].1, 0);
        assert!(db.configure_planner(&["wss://new.example"]).is_err());
        db.limits = LedgerLimits::default();
        db.configure_planner(&["wss://new.example"]).unwrap();
        assert_eq!(db.plan_rolling(HORIZON, 32).unwrap().enqueued, 32);
        assert_eq!(&windows(&db)[..32], old.as_slice());
        assert_eq!(windows(&db).len(), 64);
    }

    #[test]
    fn removal_readd_and_bounded_churn_preserve_progress() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        db.plan_rolling(HORIZON, 1).unwrap();
        let before = cursors(&db)[0].clone();
        db.configure_planner(&[]).unwrap();
        assert_eq!(db.plan_rolling(HORIZON, 32).unwrap().enqueued, 0);
        db.configure_planner(&["wss://a.example"]).unwrap();
        assert_eq!(cursors(&db)[0], before);
        db.plan_rolling(HORIZON, 1).unwrap();
        assert_eq!(windows(&db)[1].2, 900);
        for n in 1..128 {
            db.configure_planner(&[&format!("wss://r{n}.example")])
                .unwrap();
        }
        let before = cursors(&db);
        assert!(db.configure_planner(&["wss://overflow.example"]).is_err());
        assert_eq!(cursors(&db), before);
        assert!(db.configure_planner(&vec!["wss://a.example"; 33]).is_err());
        assert_eq!(cursors(&db), before);
    }

    #[test]
    fn failed_cursor_write_and_late_budget_error_rollback_entire_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        let before = (state(&db), cursors(&db));
        db.db.execute_batch("CREATE TRIGGER fail_cursor BEFORE UPDATE OF next ON planner_relays BEGIN SELECT RAISE(ABORT,'cursor fault'); END;").unwrap();
        assert!(db.plan_rolling(HORIZON, 2).is_err());
        assert!(windows(&db).is_empty());
        assert_eq!((state(&db), cursors(&db)), before);
        db.db.execute_batch("DROP TRIGGER fail_cursor;").unwrap();
        db.limits.max_jobs = 1;
        assert!(matches!(
            db.plan_rolling(HORIZON, 2),
            Err(LedgerError::Budget)
        ));
        assert!(windows(&db).is_empty());
        assert_eq!((state(&db), cursors(&db)), before);
        db.limits.max_jobs = 100_000;
        db.limits.max_bytes = 1;
        assert!(matches!(
            db.plan_rolling(HORIZON, 1),
            Err(LedgerError::Budget)
        ));
        assert_eq!((state(&db), cursors(&db)), before);
    }

    #[test]
    fn invalid_inputs_and_extreme_time_never_wrap_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        let before = (state(&db), cursors(&db));
        assert!(db.plan_rolling(-1, 1).is_err());
        assert!(db.plan_rolling(900, 0).is_err());
        assert!(db.plan_rolling(900, 33).is_err());
        assert!(db.configure_planner(&["not a relay URL"]).is_err());
        assert_eq!((state(&db), cursors(&db)), before);
        db.plan_rolling(i64::MAX, 1).unwrap();
        let boundary = i64::MAX / WINDOW * WINDOW;
        let row = &windows(&db)[0];
        assert_eq!(
            (row.2, row.3),
            (boundary - HORIZON, boundary - HORIZON + 899)
        );
    }

    #[test]
    fn sequence_overflow_and_old_schema_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.configure_planner(&["wss://a.example"]).unwrap();
        db.db
            .execute("UPDATE planner_state SET sequence=?1", [i64::MAX])
            .unwrap();
        let before = (state(&db), cursors(&db));
        assert!(db.plan_rolling(i64::MAX, 1).is_err());
        assert_eq!((state(&db), cursors(&db)), before);
        assert!(windows(&db).is_empty());
        db.enqueue("manual", "wss://a.example", 0, 1).unwrap();
        assert!(db.enqueue("rolling-v1:1", "wss://a.example", 0, 1).is_err());
        db.db.pragma_update(None, "user_version", 5).unwrap();
        drop(db);
        assert!(JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).is_err());
        let raw = rusqlite::Connection::open(dir.path().join("jobs.sqlite")).unwrap();
        assert_eq!(
            raw.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
    }
}
