//! Durable, ingester-owned reconciliation obligations. Not wired to ingestion.
//!
//! This first increment deliberately cannot complete a job: worker success is
//! not archive durability. Receipt accounting and completion belong to the next
//! increment. All mutations use immediate transactions; unfinished work is never
//! aged out or deleted to make room. Call from a bounded blocking executor.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nostr_sdk::Keys;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use thiserror::Error;

const APPLICATION_ID: i64 = 0x504e4a31;
const WRITE_RESERVE: u64 = 64 * 1024;

/// A rejected operation leaves the prior durable obligation intact.
#[derive(Debug, Error)]
pub enum LedgerError {
    /// SQLite failure, including full disk and contention.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// Filesystem accounting failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Invalid input or an incompatible database.
    #[error("invalid ledger input: {0}")]
    Invalid(&'static str),
    /// Stale, expired, forged, or already-consumed lease identity.
    #[error("lease is no longer active")]
    StaleLease,
    /// Preserve all existing work and pause admission until space is available.
    #[error("ledger admission budget exhausted")]
    Budget,
}

/// Independent limits on job rows and SQLite/WAL admission footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerLimits {
    /// Includes split parents; no automatic history deletion.
    pub max_jobs: u32,
    /// Checked before mutations and before commit, including WAL, shared memory,
    /// and a write reserve. This is an admission ceiling, not a filesystem quota:
    /// SQLite can allocate additional pages during commit or rollback.
    pub max_bytes: u64,
}

impl Default for LedgerLimits {
    fn default() -> Self {
        Self {
            max_jobs: 100_000,
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Persisted nonterminal states. No worker message can declare completion yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Eligible for a future lease.
    Queued,
    /// A single worker owns an expiring attempt.
    Leased,
    /// The same window remains due after backoff.
    RetryWait,
    /// A one-second window cannot be subdivided; operator attention required.
    Blocked,
    /// Children own the partitioned interval; the parent is not complete.
    Split,
}

/// A bounded snapshot of one obligation, excluding its secret lease token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Database-local identity.
    pub id: i64,
    /// Caller-owned identity of a frozen sweep.
    pub sweep: String,
    /// Canonical relay URL.
    pub relay: String,
    /// Inclusive lower Unix-second bound.
    pub since: i64,
    /// Inclusive upper Unix-second bound.
    pub until: i64,
    /// Split lineage, if present.
    pub parent: Option<i64>,
    /// Current durable state.
    pub state: JobState,
    /// Number of leased attempts, never reset by retry.
    pub attempt: i64,
    /// Earliest eligibility for a retry.
    pub next_eligible: i64,
    /// Fixed classification, never an unbounded remote error string.
    pub reason: Option<String>,
}

/// Unpredictable attempt capability. Intentionally not Debug or serializable.
/// The future IPC layer must authenticate the peer before exporting it.
#[derive(Clone)]
pub struct Lease {
    job: Job,
    token: [u8; 32],
    expires_at: i64,
}

impl Lease {
    /// Leased job and attempt number.
    pub fn job(&self) -> &Job {
        &self.job
    }
    /// Exclusive expiry boundary in Unix seconds.
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
}

/// Fixed failure categories used for bounded accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// Relay connection or protocol failure; do not subdivide automatically.
    RelayFailure,
    /// Worker disappeared without reliable OOM/timeout classification.
    WorkerLost,
    /// Parent cancelled an attempt.
    Cancelled,
}

impl RetryReason {
    fn label(self) -> &'static str {
        match self {
            Self::RelayFailure => "relay_failure",
            Self::WorkerLost => "worker_lost",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Outcome of an atomic inclusive-window split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitOutcome {
    /// Exact, gap-free children; parent remains unresolved.
    Children(Box<[Job; 2]>),
    /// Density at a single timestamp requires intervention, not an infinite loop.
    Blocked,
}

/// SQLite WAL/FULL ledger. It does not open the archive, RocksDB or any relay.
pub struct JobLedger {
    db: Connection,
    path: PathBuf,
    limits: LedgerLimits,
}

impl JobLedger {
    /// Open or initialize a dedicated ledger. Parent directory must exist.
    /// Limits are runtime policy: an operator may raise them after capacity review.
    pub fn open(path: &Path, limits: LedgerLimits) -> Result<Self, LedgerError> {
        if limits.max_jobs == 0
            || limits.max_bytes < 4 * WRITE_RESERVE
            || limits.max_bytes > i64::MAX as u64
        {
            return Err(LedgerError::Invalid("invalid ledger limits"));
        }
        let mut db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(1))?;
        {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            let application: i64 =
                tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
            match (application, version) {
                (0, 0) => {
                    let tables: i64 = tx.query_row(
                        "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                        [],
                        |r| r.get(0),
                    )?;
                    if tables != 0 {
                        return Err(LedgerError::Invalid("database is not an empty ledger"));
                    }
                    tx.execute_batch("CREATE TABLE jobs (
                            id INTEGER PRIMARY KEY, sweep TEXT NOT NULL, relay TEXT NOT NULL,
                            since INTEGER NOT NULL CHECK(since>=0), until INTEGER NOT NULL CHECK(until>=since),
                            parent INTEGER REFERENCES jobs(id), state TEXT NOT NULL CHECK(state IN ('queued','leased','retry_wait','blocked','split')),
                            attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt>=0), token BLOB, expires_at INTEGER,
                            next_eligible INTEGER NOT NULL DEFAULT 0, reason TEXT,
                            UNIQUE(sweep,relay,since,until),
                            CHECK((state='leased' AND token IS NOT NULL AND typeof(token)='blob' AND length(token)=32 AND expires_at IS NOT NULL) OR (state!='leased' AND token IS NULL AND expires_at IS NULL))
                        );
                        CREATE UNIQUE INDEX one_active_lease ON jobs(state) WHERE state='leased';
                        CREATE INDEX due_jobs ON jobs(state,next_eligible,id);
                        CREATE INDEX child_jobs ON jobs(parent);")?;
                    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
                    tx.pragma_update(None, "user_version", 1)?;
                }
                (APPLICATION_ID, 1) => {}
                _ => return Err(LedgerError::Invalid("unsupported ledger identity/version")),
            }
            tx.commit()?;
        }
        let mode: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if mode != "wal" {
            db.pragma_update(None, "journal_mode", "WAL")?;
        }
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "wal_autocheckpoint", 16)?;
        Ok(Self {
            db,
            path: path.canonicalize()?,
            limits,
        })
    }

    /// Idempotently record one frozen window. Different sweeps can revisit it.
    /// Within a sweep, overlapping root windows are rejected rather than hidden.
    pub fn enqueue(
        &mut self,
        sweep: &str,
        relay: &str,
        since: i64,
        until: i64,
    ) -> Result<Job, LedgerError> {
        if sweep.is_empty() || sweep.len() > 128 || relay.len() > 2048 || since < 0 || until < since
        {
            return Err(LedgerError::Invalid("invalid window or identity"));
        }
        let relay = crate::relay::normalize_relay_url(relay)
            .ok()
            .ok_or(LedgerError::Invalid("invalid relay"))?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id) = tx.query_row("SELECT id FROM jobs WHERE sweep=?1 AND relay=?2 AND since=?3 AND until=?4 AND parent IS NULL", params![sweep,relay,since,until], |r| r.get::<_,i64>(0)).optional()? {
            return read_job(&tx,id);
        }
        let overlaps: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE sweep=?1 AND relay=?2 AND parent IS NULL AND since<=?4 AND until>=?3)",params![sweep,relay,since,until],|r|r.get(0))?;
        if overlaps {
            return Err(LedgerError::Invalid("overlapping root window"));
        }
        check_budget(&tx, &self.path, self.limits, 1)?;
        tx.execute(
            "INSERT INTO jobs(sweep,relay,since,until,state) VALUES(?1,?2,?3,?4,'queued')",
            params![sweep, relay, since, until],
        )?;
        let job = read_job(&tx, tx.last_insert_rowid())?;
        commit(tx, &self.path, self.limits)?;
        Ok(job)
    }

    /// Lease the oldest eligible job. An active lease blocks all other leases.
    /// Expired work must first be moved to retry via `expire`; never stolen.
    pub fn lease_next(&mut self, now: i64, ttl_secs: u32) -> Result<Option<Lease>, LedgerError> {
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
        if active {
            return Ok(None);
        }
        let id=tx.query_row("SELECT id FROM jobs WHERE state IN ('queued','retry_wait') AND next_eligible<=?1 ORDER BY id LIMIT 1",[now],|r|r.get::<_,i64>(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        check_budget(&tx, &self.path, self.limits, 0)?;
        if read_job(&tx, id)?.attempt == i64::MAX {
            return Err(LedgerError::Invalid("attempt counter exhausted"));
        }
        // A fresh CSPRNG-backed key supplies an unpredictable 256-bit capability.
        let token = Keys::generate().secret_key().to_secret_bytes();
        tx.execute(
            "UPDATE jobs SET state='leased',attempt=attempt+1,token=?2,expires_at=?3 WHERE id=?1",
            params![id, token.as_slice(), expiry],
        )?;
        let job = read_job(&tx, id)?;
        commit(tx, &self.path, self.limits)?;
        Ok(Some(Lease {
            job,
            token,
            expires_at: expiry,
        }))
    }

    /// Release a live lease into persisted same-window backoff. Jitter is chosen
    /// by the future scheduler, within this accepted 1-minute to 1-hour bound.
    pub fn retry(
        &mut self,
        lease: &Lease,
        now: i64,
        delay_secs: u32,
        reason: RetryReason,
    ) -> Result<(), LedgerError> {
        let due = retry_due(now, delay_secs)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_lease(&tx, lease, now)?;
        retry_job(&tx, lease.job.id, due, reason.label())?;
        // Recovery releases ownership without admitting new work. SQLite may
        // still fail on a genuinely full filesystem; an admission ceiling alone
        // must not make this durable obligation impossible to recover.
        tx.commit()?;
        Ok(())
    }

    /// Recover a lost/expired worker after restart. Does not delete its window or
    /// infer OOM. Exactly one active lease means recovery work is always bounded.
    pub fn expire(&mut self, now: i64, delay_secs: u32) -> Result<bool, LedgerError> {
        let due = retry_due(now, delay_secs)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = tx
            .query_row(
                "SELECT id FROM jobs WHERE state='leased' AND expires_at<=?1",
                [now],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(false) };
        retry_job(&tx, id, due, RetryReason::WorkerLost.label())?;
        tx.commit()?;
        Ok(true)
    }

    /// Split only an explicitly classified volume/resource failure. Connectivity
    /// failures must use `retry`. Children and parent transition commit together.
    pub fn split(&mut self, lease: &Lease, now: i64) -> Result<SplitOutcome, LedgerError> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_lease(&tx, lease, now)?;
        let parent = read_job(&tx, lease.job.id)?;
        let dense = parent.since == parent.until;
        check_budget(&tx, &self.path, self.limits, if dense { 0 } else { 2 })?;
        tx.execute(
            "UPDATE jobs SET state=?2,token=NULL,expires_at=NULL,reason=?3 WHERE id=?1",
            params![
                parent.id,
                if dense { "blocked" } else { "split" },
                if dense {
                    "dense_timestamp"
                } else {
                    "resource_split"
                }
            ],
        )?;
        let outcome = if dense {
            SplitOutcome::Blocked
        } else {
            let midpoint = parent.since + (parent.until - parent.since) / 2;
            let mut children = Vec::with_capacity(2);
            for (since, until) in [(parent.since, midpoint), (midpoint + 1, parent.until)] {
                tx.execute("INSERT INTO jobs(sweep,relay,since,until,parent,state) VALUES(?1,?2,?3,?4,?5,'queued')",params![parent.sweep,parent.relay,since,until,parent.id])?;
                children.push(read_job(&tx, tx.last_insert_rowid())?);
            }
            SplitOutcome::Children(Box::new([children.remove(0), children.remove(0)]))
        };
        commit(tx, &self.path, self.limits)?;
        Ok(outcome)
    }

    /// Read one obligation without exposing its lease capability.
    pub fn get(&self, id: i64) -> Result<Job, LedgerError> {
        read_job(&self.db, id)
    }
}

fn retry_due(now: i64, delay: u32) -> Result<i64, LedgerError> {
    now.checked_add(i64::from(delay))
        .filter(|_| now >= 0 && (60..=3600).contains(&delay))
        .ok_or(LedgerError::Invalid("invalid retry deadline"))
}

fn retry_job(tx: &Transaction<'_>, id: i64, due: i64, reason: &str) -> Result<(), LedgerError> {
    tx.execute("UPDATE jobs SET state='retry_wait',token=NULL,expires_at=NULL,next_eligible=?2,reason=?3 WHERE id=?1",params![id,due,reason])?;
    Ok(())
}

fn verify_lease(tx: &Transaction<'_>, lease: &Lease, now: i64) -> Result<(), LedgerError> {
    if now < 0 {
        return Err(LedgerError::Invalid("negative clock"));
    }
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND state='leased' AND attempt=?2 AND token=?3 AND expires_at>?4)",params![lease.job.id,lease.job.attempt,lease.token.as_slice(),now],|r|r.get(0))?;
    if !valid {
        return Err(LedgerError::StaleLease);
    }
    Ok(())
}

fn read_job(db: &Connection, id: i64) -> Result<Job, LedgerError> {
    Ok(db.query_row("SELECT id,sweep,relay,since,until,parent,state,attempt,next_eligible,reason FROM jobs WHERE id=?1",[id],|r| {
        let state:String=r.get(6)?;
        let state=match state.as_str() {
            "queued"=>JobState::Queued,"leased"=>JobState::Leased,"retry_wait"=>JobState::RetryWait,
            "blocked"=>JobState::Blocked,"split"=>JobState::Split,
            _=>return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(Job {id:r.get(0)?,sweep:r.get(1)?,relay:r.get(2)?,since:r.get(3)?,until:r.get(4)?,parent:r.get(5)?,state,attempt:r.get(7)?,next_eligible:r.get(8)?,reason:r.get(9)?})
    })?)
}

fn file_bytes(path: &Path) -> Result<u64, LedgerError> {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::metadata(Path::new(&name)) {
            Ok(m) => {
                #[cfg(unix)]
                let size = {
                    use std::os::unix::fs::MetadataExt;
                    m.len().max(m.blocks().saturating_mul(512))
                };
                #[cfg(not(unix))]
                let size = m.len();
                total = total.checked_add(size).ok_or(LedgerError::Budget)?;
            }
            Err(e) if !suffix.is_empty() && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(total)
}

fn check_budget(
    db: &Connection,
    path: &Path,
    limits: LedgerLimits,
    additional: u32,
) -> Result<(), LedgerError> {
    let count: u64 = db.query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))?;
    let pages: u64 = db.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let page_size: u64 = db.pragma_query_value(None, "page_size", |r| r.get(0))?;
    if count + u64::from(additional) > u64::from(limits.max_jobs)
        || file_bytes(path)?
            .max(pages.saturating_mul(page_size))
            .saturating_add(WRITE_RESERVE)
            > limits.max_bytes
    {
        return Err(LedgerError::Budget);
    }
    Ok(())
}

fn commit(tx: Transaction<'_>, path: &Path, limits: LedgerLimits) -> Result<(), LedgerError> {
    check_budget(&tx, path, limits, 0)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY: &str = "wss://relay.example.com";

    fn ledger(dir: &tempfile::TempDir) -> JobLedger {
        JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap()
    }

    #[test]
    fn frozen_windows_are_idempotent_nonoverlapping_and_never_age_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let first = db.enqueue("sweep", RELAY, 0, 899).unwrap();
        assert_eq!(
            first,
            db.enqueue("sweep", &format!("{RELAY}/"), 0, 899).unwrap()
        );
        assert!(matches!(
            db.enqueue("sweep", RELAY, 899, 900),
            Err(LedgerError::Invalid(_))
        ));
        db.enqueue("sweep", RELAY, 900, 1799).unwrap();
        db.enqueue("new-sweep", RELAY, 0, 899).unwrap();
        drop(db);
        let mut db = ledger(&dir);
        assert_eq!(db.get(first.id).unwrap(), first);
        let lease = db.lease_next(2_000_000_000, 600).unwrap().unwrap();
        assert_eq!(lease.job.id, first.id);
        assert_eq!(lease.job.since, 0);
    }

    #[test]
    fn expiry_reopen_and_retry_fence_old_or_forged_workers() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let job = db.enqueue("s", RELAY, 0, 9).unwrap();
        let old = db.lease_next(100, 10).unwrap().unwrap();
        let mut forged = old.clone();
        forged.token[0] ^= 1;
        assert!(matches!(
            db.split(&forged, 101),
            Err(LedgerError::StaleLease)
        ));
        drop(db);
        let mut db = ledger(&dir);
        assert_eq!(db.get(job.id).unwrap().state, JobState::Leased);
        assert!(db.lease_next(110, 10).unwrap().is_none());
        assert!(matches!(
            db.retry(&old, 110, 60, RetryReason::RelayFailure),
            Err(LedgerError::StaleLease)
        ));
        assert!(!db.expire(109, 60).unwrap());
        assert!(db.expire(110, 60).unwrap());
        assert!(!db.expire(110, 60).unwrap());
        drop(db);
        let mut db = ledger(&dir);
        let waiting = db.get(job.id).unwrap();
        assert_eq!(waiting.state, JobState::RetryWait);
        assert_eq!(waiting.next_eligible, 170);
        assert_eq!(waiting.reason.as_deref(), Some("worker_lost"));
        assert!(db.lease_next(169, 10).unwrap().is_none());
        let new = db.lease_next(170, 10).unwrap().unwrap();
        assert_eq!(new.job.attempt, 2);
        assert_ne!(new.token, old.token);
        assert!(matches!(db.split(&old, 171), Err(LedgerError::StaleLease)));
        db.retry(&new, 171, 60, RetryReason::RelayFailure).unwrap();
        assert_eq!(
            db.get(job.id).unwrap().reason.as_deref(),
            Some("relay_failure")
        );
    }

    #[test]
    fn separate_connections_cannot_lease_two_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = ledger(&dir);
        let mut b = ledger(&dir);
        a.enqueue("s", RELAY, 0, 9).unwrap();
        let second = b.enqueue("s", RELAY, 10, 19).unwrap();
        let lease = a.lease_next(10, 60).unwrap().unwrap();
        assert!(b.lease_next(10, 60).unwrap().is_none());
        assert!(
            b.db.execute(
                "UPDATE jobs SET state='leased',token=?1,expires_at=70 WHERE id=?2",
                params![&[1u8; 32][..], second.id]
            )
            .is_err()
        );
        a.retry(&lease, 11, 60, RetryReason::Cancelled).unwrap();
        assert_eq!(b.lease_next(12, 60).unwrap().unwrap().job.id, second.id);
    }

    #[test]
    fn split_is_gap_free_and_dense_timestamp_stays_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let parent = db.enqueue("s", RELAY, 0, 1).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        let SplitOutcome::Children(children) = db.split(&lease, 11).unwrap() else {
            panic!("expected children")
        };
        let [a, b] = *children;
        assert_eq!((a.since, a.until, b.since, b.until), (0, 0, 1, 1));
        assert_eq!((a.parent, b.parent), (Some(parent.id), Some(parent.id)));
        assert!(matches!(db.split(&lease, 12), Err(LedgerError::StaleLease)));
        let dense = db.lease_next(12, 60).unwrap().unwrap();
        assert_eq!(db.split(&dense, 13).unwrap(), SplitOutcome::Blocked);
        drop(db);
        let db = ledger(&dir);
        assert_eq!(db.get(parent.id).unwrap().state, JobState::Split);
        assert_eq!(db.get(a.id).unwrap().state, JobState::Blocked);
        assert_eq!(db.get(b.id).unwrap().state, JobState::Queued);
    }

    #[test]
    fn child_insert_failure_rolls_back_entire_split() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let parent = db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.db.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON jobs WHEN NEW.parent IS NOT NULL AND NEW.since=5 BEGIN SELECT RAISE(ABORT,'injected second child failure'); END;").unwrap();
        assert!(matches!(
            db.split(&lease, 11),
            Err(LedgerError::Database(_))
        ));
        drop(db);
        let db = ledger(&dir);
        assert_eq!(db.get(parent.id).unwrap().state, JobState::Leased);
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn row_and_byte_limits_preserve_existing_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let limits = LedgerLimits {
            max_jobs: 2,
            ..LedgerLimits::default()
        };
        let mut db = JobLedger::open(&dir.path().join("jobs.sqlite"), limits).unwrap();
        let job = db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        assert!(matches!(db.split(&lease, 11), Err(LedgerError::Budget)));
        assert_eq!(db.get(job.id).unwrap().state, JobState::Leased);
        // Admission limits are not a full filesystem: recovery remains possible.
        db.limits.max_bytes = file_bytes(&db.path).unwrap();
        db.retry(&lease, 11, 60, RetryReason::Cancelled).unwrap();
        assert!(matches!(
            db.enqueue("s", RELAY, 10, 19),
            Err(LedgerError::Budget)
        ));
        assert_eq!(db.get(job.id).unwrap().state, JobState::RetryWait);
        drop(db);
        let mut db = JobLedger::open(&dir.path().join("jobs.sqlite"), limits).unwrap();
        let lease = db.lease_next(71, 60).unwrap().unwrap();
        db.limits.max_bytes = file_bytes(&db.path).unwrap();
        assert!(db.expire(lease.expires_at(), 60).unwrap());
        assert_eq!(db.get(job.id).unwrap().state, JobState::RetryWait);
        assert!(matches!(db.lease_next(191, 60), Err(LedgerError::Budget)));
        drop(db);
        let mut db = ledger(&dir);
        assert_eq!(db.lease_next(191, 60).unwrap().unwrap().job.id, job.id);
    }

    #[test]
    fn rejects_incompatible_databases_limits_and_invalid_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        for (now, ttl) in [(-1, 10), (0, 0), (0, 601), (i64::MAX, 1)] {
            assert!(matches!(
                db.lease_next(now, ttl),
                Err(LedgerError::Invalid(_))
            ));
        }
        for (now, delay) in [(-1, 60), (0, 59), (0, 3601), (i64::MAX, 60)] {
            assert!(matches!(
                db.expire(now, delay),
                Err(LedgerError::Invalid(_))
            ));
        }
        assert!(
            JobLedger::open(
                &db.path,
                LedgerLimits {
                    max_jobs: 1,
                    ..LedgerLimits::default()
                }
            )
            .is_ok()
        );
        db.db.pragma_update(None, "user_version", 2).unwrap();
        assert!(matches!(
            JobLedger::open(&db.path, LedgerLimits::default()),
            Err(LedgerError::Invalid(_))
        ));
        let other = dir.path().join("other.sqlite");
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated(id INTEGER);")
            .unwrap();
        assert!(matches!(
            JobLedger::open(&other, LedgerLimits::default()),
            Err(LedgerError::Invalid(_))
        ));
    }

    #[test]
    fn invalid_capability_and_attempt_overflow_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let job = db.enqueue("s", RELAY, 0, 9).unwrap();
        assert!(
            db.db
                .execute(
                    "UPDATE jobs SET state='leased',expires_at=10 WHERE id=?1",
                    [job.id]
                )
                .is_err()
        );
        db.db
            .execute(
                "UPDATE jobs SET attempt=?1 WHERE id=?2",
                params![i64::MAX, job.id],
            )
            .unwrap();
        assert!(matches!(db.lease_next(0, 10), Err(LedgerError::Invalid(_))));
        assert_eq!(db.get(job.id).unwrap().state, JobState::Queued);
    }

    #[test]
    fn simultaneous_claimers_have_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = ledger(&dir);
        first.enqueue("s", RELAY, 0, 9).unwrap();
        first.enqueue("s", RELAY, 10, 19).unwrap();
        let second = ledger(&dir);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|mut db| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    db.lease_next(10, 60).unwrap().is_some()
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
    }

    #[test]
    fn abrupt_process_exit_preserves_commit_and_discards_uncommitted_work() {
        const CHILD_PATH: &str = "PENSIEVE_LEDGER_TEST_CHILD_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let mut db = JobLedger::open(Path::new(&path), LedgerLimits::default()).unwrap();
            db.enqueue("s", RELAY, 0, 9).unwrap();
            db.lease_next(10, 60).unwrap().unwrap();
            db.db.execute_batch("BEGIN IMMEDIATE; UPDATE jobs SET state='retry_wait',token=NULL,expires_at=NULL;").unwrap();
            // No Rust destructors or SQLite close/checkpoint. This simulates
            // process loss, not a power-loss or filesystem durability test.
            std::process::exit(73);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sync::jobs::tests::abrupt_process_exit_preserves_commit_and_discards_uncommitted_work"])
            .env(CHILD_PATH, &path)
            .status().unwrap();
        assert_eq!(status.code(), Some(73));
        let mut db = ledger(&dir);
        let job = db.get(1).unwrap();
        assert_eq!(job.state, JobState::Leased);
        assert_eq!(job.attempt, 1);
        assert!(db.expire(70, 60).unwrap());
        assert_eq!(db.get(1).unwrap().state, JobState::RetryWait);
    }
}
