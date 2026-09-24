//! Durable, ingester-owned reconciliation obligations. Not wired to ingestion.
//!
//! Completion requires both a checked protocol summary and durable archive markers
//! for every received obligation, including older attempts. Bounded reconciliation
//! compacts only archive-confirmed receipt detail, preserving attempt summaries and
//! unfinished windows. Call from a bounded blocking executor after archive recovery.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nostr_sdk::Keys;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use thiserror::Error;

use super::failure::FailureDiagnostic;

const APPLICATION_ID: i64 = 0x504e4a31;
const WRITE_RESERVE: u64 = 64 * 1024;
const MAX_FAILURE_JSON: usize = 32 * 1024;
const MAINTENANCE_JOBS: &str = "SELECT id FROM jobs INDEXED BY unresolved_jobs WHERE id>?1 AND state IN ('awaiting_durability','retry_wait','blocked','split') ORDER BY id LIMIT ?2";
const FAILURE_SCHEMA: &str = "CREATE TABLE failure_reports (
    job INTEGER NOT NULL REFERENCES jobs(id), attempt INTEGER NOT NULL,
    report TEXT NOT NULL CHECK(length(report)<=32768), PRIMARY KEY(job,attempt)
);";
/// Maximum receipt rows examined in one archive reconciliation transaction.
pub const MAX_RECEIPT_BATCH: u32 = 256;
/// Maximum job rows examined by one fair archive-maintenance turn.
pub const MAX_RECOVERY_JOBS: u32 = 32;
/// Bound unsealed finished uploads before pausing new leases.
const MAX_AWAITING_DURABILITY: u32 = 2;

/// A rejected operation leaves the prior durable obligation intact.
#[derive(Debug, Error)]
pub enum LedgerError {
    /// SQLite failure, including full disk and contention.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// Filesystem accounting failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Archive/index failure; no receipt or job completion is inferred.
    #[error(transparent)]
    Archive(#[from] crate::Error),
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
    /// Retained, not-yet-archive-confirmed receipt rows across all attempts.
    pub max_receipts: u32,
    /// Checked for admissions, including WAL, shared memory,
    /// and a write reserve. This is an admission ceiling, not a filesystem quota:
    /// SQLite can allocate additional pages during commit or rollback.
    pub max_bytes: u64,
}

impl Default for LedgerLimits {
    fn default() -> Self {
        Self {
            max_jobs: 100_000,
            max_receipts: 100_000,
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Persisted job states. Only archive reconciliation can declare completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Eligible for a future lease.
    Queued,
    /// A single worker owns an expiring attempt.
    Leased,
    /// Protocol finished; local archive durability is still outstanding.
    AwaitingDurability,
    /// Protocol and every retained receipt (or both split children) are durable.
    Complete,
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
    pub(super) fn token(&self) -> [u8; 32] {
        self.token
    }
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

/// SQLite WAL/FULL ledger. Borrows the ingester's archive authority for completion;
/// never opens a second RocksDB handle or connects to a relay.
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
            || limits.max_receipts == 0
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
                            parent INTEGER REFERENCES jobs(id), state TEXT NOT NULL CHECK(state IN ('queued','leased','awaiting_durability','complete','retry_wait','blocked','split')),
                            attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt>=0), token BLOB, expires_at INTEGER,
                            next_eligible INTEGER NOT NULL DEFAULT 0, reason TEXT,
                            scan_attempt INTEGER NOT NULL DEFAULT 0, scan_sequence INTEGER NOT NULL DEFAULT 0,
                            UNIQUE(sweep,relay,since,until),
                            CHECK((state='leased' AND token IS NOT NULL AND typeof(token)='blob' AND length(token)=32 AND expires_at IS NOT NULL) OR (state!='leased' AND token IS NULL AND expires_at IS NULL))
                        );
                        CREATE UNIQUE INDEX one_active_lease ON jobs(state) WHERE state='leased';
                        CREATE INDEX due_jobs ON jobs(state,next_eligible,id);
                        CREATE INDEX child_jobs ON jobs(parent);
                        CREATE INDEX unresolved_jobs ON jobs(id)
                            WHERE state IN ('awaiting_durability','retry_wait','blocked','split');
                    CREATE TABLE attempts (
                        job INTEGER NOT NULL REFERENCES jobs(id), attempt INTEGER NOT NULL,
                        received INTEGER NOT NULL, bytes INTEGER NOT NULL, digest BLOB NOT NULL,
                        protocol_done INTEGER NOT NULL DEFAULT 0,
                        archived INTEGER NOT NULL DEFAULT 0 CHECK(archived>=0 AND archived<=received),
                        PRIMARY KEY(job,attempt)
                    );
                    CREATE TABLE receipts (
                        job INTEGER NOT NULL, attempt INTEGER NOT NULL, sequence INTEGER NOT NULL,
                        event_id BLOB NOT NULL, created_at INTEGER NOT NULL,
                        frame_bytes INTEGER NOT NULL, frame_digest BLOB NOT NULL,
                        PRIMARY KEY(job,attempt,sequence),
                        FOREIGN KEY(job,attempt) REFERENCES attempts(job,attempt)
                    );
                    CREATE TABLE receipt_totals (
                        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                        retained INTEGER NOT NULL CHECK(retained>=0),
                        scan_job INTEGER NOT NULL DEFAULT 0 CHECK(scan_job>=0)
                    );
                    INSERT INTO receipt_totals VALUES(1,0,0);")?;
                    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
                    tx.execute_batch(FAILURE_SCHEMA)?;
                    tx.pragma_update(None, "user_version", 5)?;
                    check_budget(&tx, path, limits, 0)?;
                }
                (APPLICATION_ID, 5) => {}
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

    /// Lease the oldest eligible job. An active lease or two jobs awaiting archive
    /// durability block new leases.
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
        let awaiting: u32 = tx.query_row(
            "SELECT count(*) FROM jobs WHERE state='awaiting_durability'",
            [],
            |r| r.get(0),
        )?;
        if awaiting >= MAX_AWAITING_DURABILITY {
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

    /// Bounded persisted accounting for one attempt. Receipt registration is not
    /// evidence of archive durability or novelty. No payloads are stored here.
    pub fn attempt_progress(&self, job: i64, attempt: i64) -> Result<AttemptProgress, LedgerError> {
        attempt_progress(&self.db, job, attempt)
    }

    /// Persist a terminal worker diagnostic without discarding any receipts or
    /// completing the job. The parent must separately choose a retry/backoff policy.
    pub fn record_failure_report(
        &mut self,
        lease: &Lease,
        sequence: u64,
        report: &FailureDiagnostic,
        now: i64,
    ) -> Result<(), LedgerError> {
        if !report.validate() {
            return Err(LedgerError::Invalid("invalid failure diagnostic"));
        }
        let json = serde_json::to_string(report)
            .map_err(|_| LedgerError::Invalid("invalid failure diagnostic"))?;
        if json.len() > MAX_FAILURE_JSON {
            return Err(LedgerError::Invalid("failure diagnostic too large"));
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_lease(&tx, lease, now)?;
        let progress = attempt_progress(&tx, lease.job.id, lease.job.attempt)?;
        if progress.protocol_done || has_failure(&tx, lease.job.id, lease.job.attempt)? {
            return Err(LedgerError::Invalid("protocol already ended"));
        }
        if sequence != progress.received + 1 {
            return Err(LedgerError::Invalid("failure sequence mismatch"));
        }
        check_budget(&tx, &self.path, self.limits, 0)?;
        tx.execute(
            "INSERT INTO failure_reports VALUES(?1,?2,?3)",
            params![lease.job.id, lease.job.attempt, json],
        )?;
        commit(tx, &self.path, self.limits)
    }

    /// Read retained diagnostic history, including failed attempts after retries.
    pub fn failure_report(
        &self,
        job: i64,
        attempt: i64,
    ) -> Result<Option<FailureDiagnostic>, LedgerError> {
        let json: Option<String> = self.db.query_row(
            "SELECT CASE WHEN length(report)<=32768 THEN report ELSE NULL END FROM failure_reports WHERE job=?1 AND attempt=?2",
            params![job, attempt], |r| r.get(0)).optional()?;
        json.map(|json| {
            let report: FailureDiagnostic = serde_json::from_str(&json)
                .map_err(|_| LedgerError::Invalid("invalid stored failure diagnostic"))?;
            if !report.validate() {
                return Err(LedgerError::Invalid("invalid stored failure diagnostic"));
            }
            Ok(report)
        })
        .transpose()
    }

    pub(super) fn verify_active(&mut self, lease: &Lease, now: i64) -> Result<(), LedgerError> {
        verify_lease(&self.db, lease, now)
    }

    pub(super) fn register_received(
        &mut self,
        lease: &Lease,
        received: &super::ipc::ReceiptRecord,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_lease(&tx, lease, now)?;
        if received.created_at < lease.job.since as u64
            || received.created_at > lease.job.until as u64
            || received.frame_bytes == 0
            || received.frame_bytes > super::ipc::MAX_FRAME_BYTES as u64 + 4
        {
            return Err(LedgerError::Invalid("invalid receipt metadata"));
        }
        let progress = attempt_progress(&tx, lease.job.id, lease.job.attempt)?;
        if progress.protocol_done || has_failure(&tx, lease.job.id, lease.job.attempt)? {
            return Err(LedgerError::Invalid("protocol already ended"));
        }
        if received.sequence != progress.received + 1
            || received.sequence > super::ipc::MAX_EVENTS
            || progress.bytes.saturating_add(received.frame_bytes) > super::ipc::MAX_ATTEMPT_BYTES
        {
            return Err(LedgerError::Invalid("receipt sequence or attempt limit"));
        }
        let count: i64 = tx.query_row(
            "SELECT retained FROM receipt_totals WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if count >= i64::from(self.limits.max_receipts) {
            return Err(LedgerError::Budget);
        }
        let digest = super::ipc::extend_digest(progress.digest, received.frame_digest);
        tx.execute(
            "INSERT INTO attempts(job,attempt,received,bytes,digest) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(job,attempt) DO UPDATE SET received=excluded.received,bytes=excluded.bytes,digest=excluded.digest",
            params![lease.job.id, lease.job.attempt, received.sequence, progress.bytes + received.frame_bytes, digest.as_slice()],
        )?;
        tx.execute(
            "INSERT INTO receipts VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                lease.job.id,
                lease.job.attempt,
                received.sequence,
                received.event_id.as_slice(),
                received.created_at,
                received.frame_bytes,
                received.frame_digest.as_slice()
            ],
        )?;
        tx.execute(
            "UPDATE receipt_totals SET retained=retained+1 WHERE singleton=1",
            [],
        )?;
        commit(tx, &self.path, self.limits)
    }

    pub(super) fn record_protocol_done(
        &mut self,
        lease: &Lease,
        sequence: u64,
        count: u64,
        digest: [u8; 32],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_lease(&tx, lease, now)?;
        let progress = attempt_progress(&tx, lease.job.id, lease.job.attempt)?;
        if sequence != progress.received + 1
            || count != progress.received
            || digest != progress.digest
        {
            return Err(LedgerError::Invalid("protocol receipt summary mismatch"));
        }
        if progress.protocol_done || has_failure(&tx, lease.job.id, lease.job.attempt)? {
            return Err(LedgerError::Invalid("protocol already ended"));
        }
        tx.execute(
            "INSERT INTO attempts(job,attempt,received,bytes,digest,protocol_done) VALUES(?1,?2,0,0,?3,1)
             ON CONFLICT(job,attempt) DO UPDATE SET protocol_done=1",
            params![lease.job.id, lease.job.attempt, digest.as_slice()],
        )?;
        tx.execute(
            "UPDATE jobs SET state='awaiting_durability',token=NULL,expires_at=NULL WHERE id=?1",
            [lease.job.id],
        )?;
        commit(tx, &self.path, self.limits)
    }

    /// Inspect at most `limit` retained receipts against the *same* durable index
    /// used by `writer`. The caller must finish startup recovery before calling.
    /// Never accepts worker-provided booleans or pending dedupe as archive proof.
    ///
    /// A persisted keyset cursor wraps, so one missing early event cannot starve
    /// later receipts. Confirmed IDs are removed atomically with archived counters;
    /// count/bytes/digest/protocol summaries and all unresolved IDs remain durable.
    /// Recovery/compaction bypass admission ceilings, but actual I/O errors roll
    /// back. No live attempt is compacted. This does not perform network recovery.
    /// Continue periodic checks until complete; zero satisfied rows means only
    /// that this batch has no new durable evidence, not that polling should stop.
    pub fn reconcile_archived(
        &mut self,
        job: i64,
        dedupe: &crate::DedupeIndex,
        writer: &crate::SegmentWriter,
        limit: u32,
    ) -> Result<ReceiptReconciliation, LedgerError> {
        if limit == 0 || limit > MAX_RECEIPT_BATCH || writer.recovery_required() {
            return Err(LedgerError::Invalid(
                "invalid receipt batch or archive recovery required",
            ));
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_job(&tx, job)?.state == JobState::Leased {
            return Err(LedgerError::Invalid("upload still active"));
        }
        let (after_attempt, after_sequence): (i64, i64) = tx.query_row(
            "SELECT scan_attempt,scan_sequence FROM jobs WHERE id=?1",
            [job],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let rows = {
            let mut query = tx.prepare(
                "SELECT attempt,sequence,event_id FROM receipts WHERE job=?1
                 AND (attempt,sequence)>(?2,?3) ORDER BY attempt,sequence LIMIT ?4",
            )?;
            query
                .query_map(params![job, after_attempt, after_sequence, limit], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, [u8; 32]>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut satisfied = 0;
        for (attempt, sequence, event_id) in &rows {
            if dedupe.get_status(event_id)? == Some(crate::EventStatus::Archived) {
                tx.execute(
                    "DELETE FROM receipts WHERE job=?1 AND attempt=?2 AND sequence=?3",
                    params![job, attempt, sequence],
                )?;
                tx.execute(
                    "UPDATE attempts SET archived=archived+1 WHERE job=?1 AND attempt=?2",
                    params![job, attempt],
                )?;
                satisfied += 1;
            }
        }
        let (next_attempt, next_sequence) = match rows.last() {
            Some(row) if rows.len() == limit as usize => (row.0, row.1),
            _ => (0, 0),
        };
        tx.execute(
            "UPDATE jobs SET scan_attempt=?2,scan_sequence=?3 WHERE id=?1",
            params![job, next_attempt, next_sequence],
        )?;
        tx.execute(
            "UPDATE receipt_totals SET retained=retained-?1 WHERE singleton=1",
            [satisfied],
        )?;
        complete_if_durable(&tx, job)?;
        let complete = read_job(&tx, job)?.state == JobState::Complete;
        if writer.recovery_required() {
            return Err(LedgerError::Invalid("archive recovery required"));
        }
        tx.commit()?;
        Ok(ReceiptReconciliation {
            checked: rows.len() as u32,
            satisfied,
            complete,
        })
    }

    /// One fair, persisted keyset turn over at most 32 jobs and 256 total receipts.
    ///
    /// Visits all non-live unresolved states, including split/blocked/retry jobs,
    /// and zero-receipt awaiting jobs. A partial index excludes queued, complete
    /// and leased history. Persist the cursor once per turn, including on error.
    /// Each successfully handled job advances the local cursor. Errors leave
    /// that job due; prior successful reconciliation remains valid. A crash
    /// between reconciliation and cursor persistence only repeats safe work.
    /// Recovery bypasses admission ceilings but never actual storage errors.
    pub fn maintain_archived(
        &mut self,
        dedupe: &crate::DedupeIndex,
        writer: &crate::SegmentWriter,
        max_jobs: u32,
        max_receipts: u32,
    ) -> Result<MaintenanceProgress, LedgerError> {
        if max_jobs == 0
            || max_jobs > MAX_RECOVERY_JOBS
            || max_receipts == 0
            || max_receipts > MAX_RECEIPT_BATCH
            || writer.recovery_required()
            || !writer.uses_dedupe(dedupe)
        {
            return Err(LedgerError::Invalid(
                "invalid maintenance bounds or archive authority",
            ));
        }
        let after: i64 = self.db.query_row(
            "SELECT scan_job FROM receipt_totals WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let jobs = {
            let mut query = self.db.prepare(MAINTENANCE_JOBS)?;
            query
                .query_map(params![after, max_jobs], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut progress = MaintenanceProgress::default();
        let end_known = jobs.len() < max_jobs as usize;
        let selected = jobs.len();
        let mut last = after;
        let result = (|| -> Result<(), LedgerError> {
            for id in jobs {
                if progress.checked == max_receipts {
                    break;
                }
                let outcome =
                    self.reconcile_archived(id, dedupe, writer, max_receipts - progress.checked)?;
                progress.checked += outcome.checked;
                progress.satisfied += outcome.satisfied;
                progress.completed += u32::from(outcome.complete);
                last = id;
                progress.jobs += 1;
            }
            Ok(())
        })();
        if result.is_ok() && end_known && progress.jobs as usize == selected {
            last = 0;
            progress.wrapped = true;
        }
        // One cursor commit per turn: errors retain the failed job for retry.
        // A crash before this write repeats at most one bounded turn safely.
        if last != after {
            self.db.execute(
                "UPDATE receipt_totals SET scan_job=?1 WHERE singleton=1",
                [last],
            )?;
        }
        result?;
        Ok(progress)
    }

    /// Explicit parent recovery when a finished upload cannot reach durability.
    /// Preserves every receipt and retries the same interval with a new capability.
    /// The attempt number fences stale recovery decisions. Not a worker message.
    pub fn retry_durability(
        &mut self,
        job: i64,
        attempt: i64,
        now: i64,
        delay_secs: u32,
    ) -> Result<(), LedgerError> {
        let due = retry_due(now, delay_secs)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_job(&tx, job)?;
        if current.state != JobState::AwaitingDurability || current.attempt != attempt {
            return Err(LedgerError::StaleLease);
        }
        retry_job(&tx, job, due, "durability_retry")?;
        tx.commit()?;
        Ok(())
    }
}

/// Result of one bounded reconciliation call; checked rows can still be missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptReconciliation {
    /// Durable-index lookups performed, bounded by the caller's batch limit.
    pub checked: u32,
    /// Archive-confirmed rows atomically compacted into their attempt summaries.
    pub satisfied: u32,
    /// This job is durably complete, not merely protocol-complete.
    pub complete: bool,
}

/// Bounded maintenance accounting; zero changes is not a stop condition.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceProgress {
    /// Unresolved job rows reconciled this turn.
    pub jobs: u32,
    /// Total receipt rows checked across every visited job.
    pub checked: u32,
    /// Archive-confirmed receipts compacted this turn.
    pub satisfied: u32,
    /// Selected jobs newly observed complete after reconciliation.
    pub completed: u32,
    /// End of the keyspace reached; next turn starts at its beginning.
    pub wrapped: bool,
}

fn complete_if_durable(tx: &Transaction<'_>, mut job: i64) -> Result<(), LedgerError> {
    // An i64-second inclusive interval can split at most 63 times. Bound parent
    // propagation even in the presence of an invalid externally edited database.
    for _ in 0..=63 {
        let changed = tx.execute(
            "UPDATE jobs SET state='complete',reason=NULL WHERE id=?1
             AND NOT EXISTS(SELECT 1 FROM receipts WHERE job=?1)
             AND ((state='awaiting_durability' AND EXISTS(
                 SELECT 1 FROM attempts WHERE job=?1 AND attempt=jobs.attempt
                 AND protocol_done=1 AND archived=received))
               OR (state='split' AND (SELECT count(*) FROM jobs child WHERE child.parent=?1)=2
                 AND NOT EXISTS(SELECT 1 FROM jobs child WHERE child.parent=?1 AND child.state!='complete')))",
            [job],
        )?;
        if changed == 0 {
            break;
        }
        match read_job(tx, job)?.parent {
            Some(parent) => job = parent,
            None => break,
        }
    }
    Ok(())
}

/// Received-frame accounting and separately archive-confirmed frame totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptProgress {
    /// Event frames durably registered, including duplicate event IDs.
    pub received: u64,
    /// Framed wire bytes for those events.
    pub bytes: u64,
    /// Domain-separated ordered digest chain over exact event frames.
    pub digest: [u8; 32],
    /// The worker's final count and digest matched; not job completion.
    pub protocol_done: bool,
    /// Received frames independently confirmed by durable archive markers.
    /// Duplicate IDs count per received frame, not as novel archived events.
    pub archived: u64,
}

fn attempt_progress(
    db: &Connection,
    job: i64,
    attempt: i64,
) -> Result<AttemptProgress, LedgerError> {
    Ok(db
        .query_row(
            "SELECT received,bytes,digest,protocol_done,archived FROM attempts WHERE job=?1 AND attempt=?2",
            params![job, attempt],
            |r| {
                Ok(AttemptProgress {
                    received: r.get(0)?,
                    bytes: r.get(1)?,
                    digest: r.get(2)?,
                    protocol_done: r.get(3)?,
                    archived: r.get(4)?,
                })
            },
        )
        .optional()?
        .unwrap_or(AttemptProgress {
            received: 0,
            bytes: 0,
            digest: super::ipc::initial_digest(),
            protocol_done: false,
            archived: 0,
        }))
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

fn verify_lease(tx: &Connection, lease: &Lease, now: i64) -> Result<(), LedgerError> {
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
            "awaiting_durability"=>JobState::AwaitingDurability,"complete"=>JobState::Complete,
            _=>return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(Job {id:r.get(0)?,sweep:r.get(1)?,relay:r.get(2)?,since:r.get(3)?,until:r.get(4)?,parent:r.get(5)?,state,attempt:r.get(7)?,next_eligible:r.get(8)?,reason:r.get(9)?})
    })?)
}

fn has_failure(db: &Connection, job: i64, attempt: i64) -> Result<bool, LedgerError> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM failure_reports WHERE job=?1 AND attempt=?2)",
        params![job, attempt],
        |r| r.get(0),
    )?)
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

    fn diagnostic() -> FailureDiagnostic {
        FailureDiagnostic {
            kind: super::super::failure::FailureKind::Unavailable,
            missing_count: 2,
            sample: vec![[1; 32], [2; 32]],
        }
    }

    #[test]
    fn failure_reports_are_terminal_preserved_and_lease_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        db.enqueue("s", RELAY, 0, 9).unwrap();
        let first = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&first, &receipt(1), 11).unwrap();
        let prior = db.attempt_progress(first.job.id, 1).unwrap();
        assert!(
            db.record_failure_report(&first, 1, &diagnostic(), 12)
                .is_err()
        );
        assert!(db.failure_report(first.job.id, 1).unwrap().is_none());
        db.record_failure_report(&first, 2, &diagnostic(), 12)
            .unwrap();
        assert!(
            db.record_failure_report(&first, 2, &diagnostic(), 13)
                .is_err()
        );
        assert!(db.register_received(&first, &receipt(2), 13).is_err());
        assert!(
            db.record_protocol_done(&first, 2, 1, prior.digest, 13)
                .is_err()
        );
        assert_eq!(db.attempt_progress(first.job.id, 1).unwrap(), prior);
        assert_eq!(db.get(first.job.id).unwrap().state, JobState::Leased);
        db.expire(71, 60).unwrap();
        let second = db.lease_next(131, 60).unwrap().unwrap();
        assert!(matches!(
            db.record_failure_report(&first, 2, &diagnostic(), 132),
            Err(LedgerError::StaleLease)
        ));
        db.register_received(&second, &receipt(1), 132).unwrap();
        let path = db.path.clone();
        drop(db);
        let db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        assert_eq!(
            db.failure_report(first.job.id, 1).unwrap(),
            Some(diagnostic())
        );
        assert!(db.failure_report(first.job.id, 2).unwrap().is_none());
        assert_eq!(db.attempt_progress(first.job.id, 1).unwrap(), prior);
        assert_eq!(db.attempt_progress(first.job.id, 2).unwrap().received, 1);
    }

    #[test]
    fn failure_report_budget_rejection_preserves_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&lease, &receipt(1), 11).unwrap();
        let prior = db.attempt_progress(lease.job.id, 1).unwrap();
        db.limits.max_bytes = file_bytes(&db.path).unwrap();
        assert!(matches!(
            db.record_failure_report(&lease, 2, &diagnostic(), 12),
            Err(LedgerError::Budget)
        ));
        assert!(db.failure_report(lease.job.id, 1).unwrap().is_none());
        assert_eq!(db.attempt_progress(lease.job.id, 1).unwrap(), prior);
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn old_prototype_schema_is_rejected_without_changing_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let job = db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&lease, &receipt(1), 11).unwrap();
        let prior = db.attempt_progress(job.id, 1).unwrap();
        db.db
            .execute_batch("DROP TABLE failure_reports; PRAGMA user_version=3;")
            .unwrap();
        let path = db.path.clone();
        drop(db);
        assert!(matches!(
            JobLedger::open(&path, LedgerLimits::default()),
            Err(LedgerError::Invalid(_))
        ));
        let raw = Connection::open(path).unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |r| r.get::<_, u64>(0))
                .unwrap(),
            3
        );
        assert_eq!(read_job(&raw, job.id).unwrap().state, JobState::Leased);
        assert_eq!(attempt_progress(&raw, job.id, 1).unwrap(), prior);
        assert_eq!(
            raw.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            raw.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='failure_reports'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn current_schema_over_admission_ceiling_still_opens_and_recovers() {
        for expire in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut db = ledger(&dir);
            db.enqueue("first", RELAY, 0, 9).unwrap();
            db.enqueue("second", RELAY, 10, 19).unwrap();
            let lease = db.lease_next(10, 60).unwrap().unwrap();
            db.register_received(&lease, &receipt(1), 11).unwrap();
            let path = db.path.clone();
            drop(db);
            let mut db = JobLedger::open(
                &path,
                LedgerLimits {
                    max_jobs: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            if expire {
                assert!(db.expire(71, 60).unwrap());
            } else {
                db.retry(&lease, 12, 60, RetryReason::RelayFailure).unwrap();
            }
            assert_eq!(db.get(lease.job.id).unwrap().state, JobState::RetryWait);
            assert_eq!(db.attempt_progress(lease.job.id, 1).unwrap().received, 1);
            assert!(matches!(db.lease_next(132, 60), Err(LedgerError::Budget)));
        }
    }

    #[test]
    fn failure_report_insert_error_rolls_back_without_losing_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&lease, &receipt(1), 11).unwrap();
        let prior = db.attempt_progress(lease.job.id, 1).unwrap();
        db.db.execute_batch("CREATE TRIGGER reject_failure BEFORE INSERT ON failure_reports BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(matches!(
            db.record_failure_report(&lease, 2, &diagnostic(), 12),
            Err(LedgerError::Database(_))
        ));
        assert!(db.failure_report(lease.job.id, 1).unwrap().is_none());
        assert_eq!(db.attempt_progress(lease.job.id, 1).unwrap(), prior);
        assert_eq!(db.get(lease.job.id).unwrap().state, JobState::Leased);
        db.register_received(&lease, &receipt(2), 13).unwrap();
    }

    fn ledger(dir: &tempfile::TempDir) -> JobLedger {
        JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap()
    }

    fn archive(
        dir: &tempfile::TempDir,
    ) -> (std::sync::Arc<crate::DedupeIndex>, crate::SegmentWriter) {
        let dedupe =
            std::sync::Arc::new(crate::DedupeIndex::open(dir.path().join("dedupe")).unwrap());
        let writer = crate::SegmentWriter::new(
            crate::SegmentConfig {
                output_dir: dir.path().join("archive"),
                ..crate::SegmentConfig::default()
            },
            None,
            Some(dedupe.clone()),
        )
        .unwrap();
        (dedupe, writer)
    }

    #[test]
    fn maintenance_cursor_survives_reopen_and_missing_first_receipt_does_not_starve() {
        let dir = tempfile::tempdir().unwrap();
        let (dedupe, writer) = archive(&dir);
        let mut db = ledger(&dir);
        let a = db.enqueue("a", RELAY, 0, 9).unwrap();
        let b = db.enqueue("b", RELAY, 0, 9).unwrap();
        let c = db.enqueue("c", RELAY, 0, 9).unwrap();
        for id in [a.id, b.id] {
            let lease = db.lease_next(10, 60).unwrap().unwrap();
            assert_eq!(lease.job.id, id);
            let mut item = receipt(1);
            item.event_id = [id as u8; 32];
            db.register_received(&lease, &item, 11).unwrap();
            db.retry(&lease, 12, 60, RetryReason::WorkerLost).unwrap();
        }
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.record_protocol_done(&lease, 1, 0, super::super::ipc::initial_digest(), 11)
            .unwrap();
        dedupe
            .mark_archived([&[b.id as u8; 32]].into_iter())
            .unwrap();
        let progress = db.maintain_archived(&dedupe, &writer, 1, 1).unwrap();
        assert_eq!(
            (progress.jobs, progress.checked, progress.satisfied),
            (1, 1, 0)
        );
        drop(db);
        let mut db = ledger(&dir);
        db.limits.max_bytes = 1; // Admission paused; maintenance must continue.
        assert!(matches!(
            db.enqueue("budget", RELAY, 0, 9),
            Err(LedgerError::Budget)
        ));
        let progress = db.maintain_archived(&dedupe, &writer, 1, 1).unwrap();
        assert_eq!(
            (progress.jobs, progress.checked, progress.satisfied),
            (1, 1, 1)
        );
        assert_eq!(db.get(b.id).unwrap().state, JobState::RetryWait);
        let progress = db.maintain_archived(&dedupe, &writer, 1, 1).unwrap();
        assert_eq!((progress.checked, progress.completed), (0, 1));
        assert_eq!(db.get(c.id).unwrap().state, JobState::Complete);
        assert!(
            db.maintain_archived(&dedupe, &writer, 1, 1)
                .unwrap()
                .wrapped
        );
        assert_eq!(
            db.maintain_archived(&dedupe, &writer, 1, 1).unwrap().jobs,
            1
        );
        assert_eq!(db.get(a.id).unwrap().state, JobState::RetryWait);
    }

    #[test]
    fn failure_report_survives_split_maintenance_and_reopen_without_completing_gaps() {
        for until in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let (dedupe, writer) = archive(&dir);
            let mut db = ledger(&dir);
            let job = db.enqueue("s", RELAY, 1, until).unwrap();
            let lease = db.lease_next(10, 60).unwrap().unwrap();
            db.register_received(&lease, &receipt(1), 11).unwrap();
            let report = FailureDiagnostic {
                kind: super::super::failure::FailureKind::Volume,
                missing_count: 0,
                sample: Vec::new(),
            };
            db.record_failure_report(&lease, 2, &report, 12).unwrap();
            db.split(&lease, 12).unwrap();
            assert_eq!(db.failure_report(job.id, 1).unwrap(), Some(report.clone()));
            dedupe.mark_archived([&[1; 32]].into_iter()).unwrap();
            let progress = db.maintain_archived(&dedupe, &writer, 32, 256).unwrap();
            assert_eq!(progress.satisfied, 1);
            assert_eq!(progress.completed, 0);
            drop(db);
            let mut db = ledger(&dir);
            assert_eq!(db.failure_report(job.id, 1).unwrap(), Some(report));
            let attempt = db.attempt_progress(job.id, 1).unwrap();
            assert_eq!((attempt.received, attempt.archived), (1, 1));
            assert!(!attempt.protocol_done);
            let remaining: i64 = db
                .db
                .query_row("SELECT count(*) FROM receipts", [], |row| row.get(0))
                .unwrap();
            assert_eq!(remaining, 0);
            // Repeated maintenance after reopen cannot turn a compacted failed
            // attempt into success; split children have not recovered their gaps.
            for _ in 0..2 {
                assert_eq!(
                    db.maintain_archived(&dedupe, &writer, 32, 256)
                        .unwrap()
                        .completed,
                    0
                );
            }
            assert_eq!(
                db.get(job.id).unwrap().state,
                if until == 1 {
                    JobState::Blocked
                } else {
                    JobState::Split
                }
            );
        }
    }

    #[test]
    fn maintenance_query_uses_partial_index_without_sorting_history() {
        let dir = tempfile::tempdir().unwrap();
        let db = ledger(&dir);
        let plan = db
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {MAINTENANCE_JOBS}"))
            .unwrap()
            .query_map(params![0, MAX_RECOVERY_JOBS], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("SEARCH jobs USING INDEX unresolved_jobs (id>?)")),
            "{plan:?}"
        );
        assert!(
            plan.iter().all(|detail| !detail.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }

    #[test]
    fn maintenance_limits_empty_history_and_total_receipts_and_keeps_error_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let (dedupe, writer) = archive(&dir);
        let mut db = ledger(&dir);
        for n in 0..80 {
            db.enqueue(&format!("{n}"), RELAY, 0, 9).unwrap();
        }
        // Synthetic terminal history must not consume scan turns or writes.
        db.db
            .execute("UPDATE jobs SET state='complete' WHERE id>40", [])
            .unwrap();
        let writes = db.db.total_changes();
        assert_eq!(
            db.maintain_archived(&dedupe, &writer, 32, 256)
                .unwrap()
                .jobs,
            0
        );
        assert_eq!(
            db.maintain_archived(&dedupe, &writer, 32, 256)
                .unwrap()
                .jobs,
            0
        );
        assert!(
            db.maintain_archived(&dedupe, &writer, 32, 256)
                .unwrap()
                .wrapped
        );
        assert_eq!(db.db.total_changes(), writes);
        for _ in 0..3 {
            let lease = db.lease_next(10, 60).unwrap().unwrap();
            for n in 1..=200 {
                db.register_received(&lease, &receipt(n), 11).unwrap();
            }
            db.retry(&lease, 12, 60, RetryReason::WorkerLost).unwrap();
        }
        let progress = db.maintain_archived(&dedupe, &writer, 32, 256).unwrap();
        assert_eq!((progress.jobs, progress.checked), (2, 256));
        db.db.execute_batch("CREATE TRIGGER reject_cursor BEFORE UPDATE OF scan_job ON receipt_totals BEGIN SELECT RAISE(ABORT,'cursor fault'); END;").unwrap();
        assert!(db.maintain_archived(&dedupe, &writer, 1, 1).is_err());
        let cursor: i64 = db
            .db
            .query_row("SELECT scan_job FROM receipt_totals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cursor, 2);
        assert_eq!(db.get(3).unwrap().state, JobState::RetryWait);
    }

    #[test]
    fn maintenance_persists_success_before_error_and_wraps_in_the_last_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (dedupe, writer) = archive(&dir);
        let mut db = ledger(&dir);
        for n in 0..3 {
            let job = db.enqueue(&format!("{n}"), RELAY, 0, 9).unwrap();
            let lease = db.lease_next(10, 60).unwrap().unwrap();
            let mut item = receipt(1);
            item.event_id = [job.id as u8; 32];
            db.register_received(&lease, &item, 11).unwrap();
            db.retry(&lease, 12, 60, RetryReason::WorkerLost).unwrap();
            dedupe.mark_archived([&item.event_id].into_iter()).unwrap();
        }
        db.db.execute_batch("CREATE TABLE cursor_writes(value INTEGER);
            CREATE TRIGGER count_cursor AFTER UPDATE OF scan_job ON receipt_totals BEGIN INSERT INTO cursor_writes VALUES(NEW.scan_job); END;
            CREATE TRIGGER fail_second BEFORE UPDATE ON attempts WHEN NEW.job=2 BEGIN SELECT RAISE(ABORT,'second job fault'); END;").unwrap();
        assert!(db.maintain_archived(&dedupe, &writer, 32, 256).is_err());
        assert_eq!(
            db.db
                .query_row("SELECT scan_job FROM receipt_totals", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(db.attempt_progress(1, 1).unwrap().archived, 1);
        assert_eq!(db.attempt_progress(2, 1).unwrap().archived, 0);
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM cursor_writes", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.db.execute_batch("DROP TRIGGER fail_second;").unwrap();
        let progress = db.maintain_archived(&dedupe, &writer, 32, 256).unwrap();
        assert_eq!(
            (progress.jobs, progress.satisfied, progress.wrapped),
            (2, 2, true)
        );
        assert_eq!(
            db.db
                .query_row("SELECT scan_job FROM receipt_totals", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM cursor_writes", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
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
        for limits in [
            LedgerLimits {
                max_jobs: 0,
                ..LedgerLimits::default()
            },
            LedgerLimits {
                max_receipts: 0,
                ..LedgerLimits::default()
            },
        ] {
            assert!(matches!(
                JobLedger::open(&db.path, limits),
                Err(LedgerError::Invalid(_))
            ));
        }
        // The previous undeployed receipt schema is not silently reinterpreted.
        db.db.pragma_update(None, "user_version", 2).unwrap();
        assert!(matches!(
            JobLedger::open(&db.path, LedgerLimits::default()),
            Err(LedgerError::Invalid(_))
        ));
        assert_eq!(
            db.db
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        db.db.pragma_update(None, "user_version", 99).unwrap();
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
            let lease = db.lease_next(10, 60).unwrap().unwrap();
            db.register_received(&lease, &receipt(1), 11).unwrap();
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
        assert_eq!(db.attempt_progress(job.id, 1).unwrap().received, 1);
        assert!(db.expire(70, 60).unwrap());
        assert_eq!(db.get(1).unwrap().state, JobState::RetryWait);
    }

    fn receipt(sequence: u64) -> super::super::ipc::ReceiptRecord {
        super::super::ipc::ReceiptRecord {
            sequence,
            event_id: [1; 32],
            created_at: 1,
            frame_bytes: 500,
            frame_digest: [sequence as u8; 32],
        }
    }

    #[test]
    fn undeployed_v1_schema_is_rejected_without_mutating_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let job = db.enqueue("s", RELAY, 0, 9).unwrap();
        db.lease_next(10, 60).unwrap().unwrap();
        // Recreate the exact v1 schema shape: it has neither receipt table.
        db.db
            .execute_batch("DROP TABLE receipts; DROP TABLE attempts; DROP TABLE receipt_totals; DROP TABLE failure_reports; PRAGMA user_version=1;")
            .unwrap();
        let path = db.path.clone();
        drop(db);
        assert!(matches!(
            JobLedger::open(&path, LedgerLimits::default()),
            Err(LedgerError::Invalid(_))
        ));
        let raw = Connection::open(path).unwrap();
        assert_eq!(read_job(&raw, job.id).unwrap().state, JobState::Leased);
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn receipt_replay_is_rejected_and_old_attempt_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        let job = db.enqueue("s", RELAY, 0, 9).unwrap();
        let first = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&first, &receipt(1), 11).unwrap();
        assert!(db.register_received(&first, &receipt(1), 11).is_err());
        let mut conflict = receipt(1);
        conflict.frame_digest[0] ^= 1;
        assert!(db.register_received(&first, &conflict, 11).is_err());
        assert_eq!(db.attempt_progress(job.id, 1).unwrap().received, 1);
        db.retry(&first, 12, 60, RetryReason::WorkerLost).unwrap();
        let second = db.lease_next(72, 60).unwrap().unwrap();
        assert!(matches!(
            db.register_received(&first, &receipt(2), 73),
            Err(LedgerError::StaleLease)
        ));
        db.register_received(&second, &receipt(1), 73).unwrap();
        assert_eq!(db.attempt_progress(job.id, 1).unwrap().received, 1);
        assert_eq!(db.attempt_progress(job.id, 2).unwrap().received, 1);
        let digest = db.attempt_progress(job.id, 2).unwrap().digest;
        db.record_protocol_done(&second, 2, 1, digest, 74).unwrap();
        assert!(db.record_protocol_done(&second, 2, 1, digest, 74).is_err());
        assert!(db.register_received(&second, &receipt(2), 75).is_err());
    }

    #[test]
    fn receipt_insert_failure_rolls_back_summary_and_emits_no_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.db.execute_batch("CREATE TRIGGER reject_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").unwrap();
        assert!(matches!(
            db.register_received(&lease, &receipt(1), 11),
            Err(LedgerError::Database(_))
        ));
        drop(db);
        let db = ledger(&dir);
        assert_eq!(db.attempt_progress(lease.job.id, 1).unwrap().received, 0);
        assert_eq!(
            db.db
                .query_row("SELECT retained FROM receipt_totals", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.db
                .query_row("SELECT count(*) FROM attempts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(db.get(lease.job.id).unwrap().state, JobState::Leased);
    }

    #[test]
    fn receipt_budget_failure_keeps_previous_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&lease, &receipt(1), 11).unwrap();
        let prior = db.attempt_progress(lease.job.id, 1).unwrap();
        db.limits.max_bytes = file_bytes(&db.path).unwrap();
        assert!(matches!(
            db.register_received(&lease, &receipt(2), 12),
            Err(LedgerError::Budget)
        ));
        assert!(matches!(
            db.record_protocol_done(&lease, 2, 1, prior.digest, 12),
            Err(LedgerError::Budget)
        ));
        assert_eq!(db.attempt_progress(lease.job.id, 1).unwrap(), prior);
    }

    #[test]
    fn global_receipt_ceiling_and_persisted_attempt_limits_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = ledger(&dir);
        db.enqueue("s", RELAY, 0, 9).unwrap();
        let lease = db.lease_next(10, 60).unwrap().unwrap();
        db.register_received(&lease, &receipt(1), 11).unwrap();
        db.db
            .execute(
                "UPDATE attempts SET received=?1 WHERE job=?2",
                params![super::super::ipc::MAX_EVENTS, lease.job.id],
            )
            .unwrap();
        assert!(
            db.register_received(&lease, &receipt(super::super::ipc::MAX_EVENTS + 1), 12)
                .is_err()
        );
        db.db
            .execute(
                "UPDATE attempts SET received=1,bytes=?1 WHERE job=?2",
                params![super::super::ipc::MAX_ATTEMPT_BYTES, lease.job.id],
            )
            .unwrap();
        assert!(db.register_received(&lease, &receipt(2), 12).is_err());
        db.db
            .execute("UPDATE attempts SET bytes=500 WHERE job=?1", [lease.job.id])
            .unwrap();
        // Seed only the authoritative counter to exercise the admission bound.
        db.db
            .execute(
                "UPDATE receipt_totals SET retained=?1",
                [db.limits.max_receipts],
            )
            .unwrap();
        // A new attempt must not evade the retained-receipt ceiling.
        db.retry(&lease, 12, 60, RetryReason::WorkerLost).unwrap();
        let next = db.lease_next(72, 60).unwrap().unwrap();
        assert!(matches!(
            db.register_received(&next, &receipt(1), 73),
            Err(LedgerError::Budget)
        ));
        assert_eq!(db.attempt_progress(lease.job.id, 2).unwrap().received, 0);
    }
}
