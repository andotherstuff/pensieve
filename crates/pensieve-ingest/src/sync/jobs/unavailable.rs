//! Relay-unavailable exclusions: a proven, separately accounted completion for a
//! window whose relay keeps advertising events it never delivers.
//!
//! Production job 768 (2026-10-13, `wss://premium.primal.net`) failed fifteen
//! times with the same single advertised-but-undelivered ID while every other
//! event in the window was archived; no relay returned that event by ID. Without
//! an exclusion such a window is retried hourly forever and holds health red.
//!
//! A job qualifies only when its latest `attempts` consecutive reported attempts
//! each ended in an `unavailable` report naming the *identical*, fully enumerated
//! missing-ID set (`missing_count` equals the sample length, at most `max_ids`),
//! every one of those IDs was explicitly requested (the attempt's whole diff fit
//! in the worker's single fetch batch), the reports span at least
//! `min_span_secs` and every received frame of the job is already archived.
//! Attempts that ended without any report (a lost worker, or a lease consumed
//! without a session, like job 768's attempt 16) carry no relay evidence: they
//! are skipped, not counted. Any other report kind, a different set or a
//! protocol-complete attempt ends the run.
//!
//! IDs of the set that already carry a durable archive marker count as
//! delivered: the archive holds them, whichever path brought them in. This
//! matters for events archived before the inventory replay floor, which the
//! inventory never lists, so every attempt's diff names them again and normal
//! reconciliation could never finish the window. Only the remaining IDs are
//! excluded; when none remain, the window completes with reason
//! `already_archived` and no exclusion row, because nothing is missing.
//!
//! The proof is copied into `relay_unavailable_exclusions` (it outlives
//! retention of the job's detail), the job completes with reason
//! `relay_unavailable`, and coverage accounting keeps the count separate from
//! policy exclusions. Receipts, archive markers and job IDs are untouched. Both
//! completions fire the `job_completed` trigger, so they advance
//! `lifecycle.last_success` like any other durable completion.
//!
//! [`JobLedger::pause_failures`] exempts from the runtime's failure pause a job
//! whose in-progress run is on track to such a completion; see
//! `exemption_ceiling` for the bound.
//!
//! Report times are recorded from schema 10 on. Older reports have none; for a
//! gap that touches one, the span uses the runtime's fixed backoff floor after
//! that attempt (`min(60 * 2^(n-1), 3600)` seconds after failure `n`, unchanged
//! since the runtime shipped). The only path that could shorten such a gap is a
//! release that lost its lease and fell back to expiry's 60-second retry
//! (`negentropy_isolated_stale_release_total`).

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{JobLedger, JobState, LedgerError, complete_if_durable, read_job};
use crate::sync::failure::{FailureDiagnostic, FailureKind, MAX_FAILURE_SAMPLE};

/// IDs the worker requests per fetch subscription. A diff no larger than this
/// was requested in one batch, so every reported missing ID was asked for by ID
/// and answered with EOSE. Larger diffs can report not-yet-requested IDs.
pub(crate) const WORKER_FETCH_BATCH: u64 = 128;
/// Upper bound on report rows read to measure one job's identical run.
const MAX_EVIDENCE_ATTEMPTS: i64 = 1024;
/// Retry-wait jobs examined per evaluation turn.
pub(super) const MAX_JOBS_PER_TURN: i64 = 32;
/// Unresolved exclusions checked against the archive per turn.
pub(super) const MAX_LATE_CHECKS: i64 = 64;
/// Failing jobs whose report history one pause evaluation reads. Any further
/// candidate is counted toward the pause, the conservative direction.
const MAX_EXEMPTION_CHECKS: usize = 256;
/// The reason recorded on the completed job and every exclusion row.
pub const RELAY_UNAVAILABLE: &str = "relay_unavailable";
/// The reason recorded on a job whose withheld IDs were all already archived.
pub const ALREADY_ARCHIVED: &str = "already_archived";

pub(super) const SCHEMA: &str = "
ALTER TABLE failure_reports ADD COLUMN reported_at INTEGER;
CREATE TABLE relay_unavailable_exclusions (
 id INTEGER PRIMARY KEY,
 event_id BLOB NOT NULL CHECK(length(event_id)=32),
 job INTEGER NOT NULL, relay TEXT NOT NULL,
 since INTEGER NOT NULL CHECK(since>=0), until INTEGER NOT NULL CHECK(until>=since),
 first_attempt INTEGER NOT NULL CHECK(first_attempt>=1),
 last_attempt INTEGER NOT NULL CHECK(last_attempt>=first_attempt),
 attempts INTEGER NOT NULL CHECK(attempts>=1 AND attempts<=last_attempt-first_attempt+1),
 first_reported_at INTEGER, last_reported_at INTEGER,
 span_secs INTEGER NOT NULL CHECK(span_secs>=0),
 excluded_at INTEGER NOT NULL, resolved_at INTEGER,
 reason TEXT NOT NULL CHECK(reason='relay_unavailable'),
 UNIQUE(job,event_id)
);
CREATE INDEX relay_unavailable_unresolved ON relay_unavailable_exclusions(id)
 WHERE resolved_at IS NULL;
ALTER TABLE completed_coverage ADD COLUMN relay_unavailable INTEGER NOT NULL DEFAULT 0;
";

/// Operator policy for automatic relay-unavailable exclusions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnavailableRule {
    /// Consecutive latest attempts that must report the identical missing set.
    pub attempts: u32,
    /// Minimum time from the first to the last of those reports.
    pub min_span_secs: u32,
    /// Largest missing set that can be excluded.
    pub max_ids: u32,
}

impl UnavailableRule {
    /// Accepted range for [`Self::attempts`].
    pub const ATTEMPTS: std::ops::RangeInclusive<u32> = 3..=32;
    /// Accepted range for [`Self::min_span_secs`] (30 minutes to one day).
    pub const SPAN_SECS: std::ops::RangeInclusive<u32> = 1800..=86_400;
    /// Accepted range for [`Self::max_ids`]; never above the diagnostic sample.
    pub const MAX_IDS: std::ops::RangeInclusive<u32> = 1..=MAX_FAILURE_SAMPLE as u32;

    /// Reject values outside the conservative bounds.
    pub fn validate(&self) -> Result<(), LedgerError> {
        if Self::ATTEMPTS.contains(&self.attempts)
            && Self::SPAN_SECS.contains(&self.min_span_secs)
            && Self::MAX_IDS.contains(&self.max_ids)
        {
            Ok(())
        } else {
            Err(LedgerError::Invalid("invalid relay-unavailable rule"))
        }
    }
}

impl Default for UnavailableRule {
    fn default() -> Self {
        Self {
            attempts: 5,
            min_span_secs: 3600,
            max_ids: 8,
        }
    }
}

/// An excluded event ID and when it later reached the archive, if it did.
pub type ExclusionStatus = ([u8; 32], Option<i64>);

/// One window completed by a relay-unavailable exclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableExclusion {
    /// Completed job.
    pub job: i64,
    /// Relay that advertised and withheld the IDs.
    pub relay: String,
    /// Excluded event IDs (the withheld set minus IDs already archived), sorted.
    pub event_ids: Vec<[u8; 32]>,
    /// First attempt of the identical run.
    pub first_attempt: i64,
    /// Last attempt of the identical run.
    pub last_attempt: i64,
    /// Qualifying reports in the run; attempts without a report are not counted.
    pub attempts: i64,
    /// Lower bound on the run's report span in seconds.
    pub span_secs: i64,
}

/// One bounded evaluation turn.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UnavailableProgress {
    /// Windows completed by exclusion this turn.
    pub excluded: Vec<UnavailableExclusion>,
    /// Qualifying jobs left in retry because received frames are not yet archived.
    pub awaiting_archive: u32,
    /// Qualifying jobs completed with reason `already_archived`, without an
    /// exclusion, because every withheld ID already carried an archive marker.
    pub already_archived: Vec<i64>,
    /// Exclusions whose event has since reached the archive by another path.
    pub resolved: u32,
}

#[derive(Debug)]
struct Evidence {
    ids: Vec<[u8; 32]>,
    first_attempt: i64,
    last_attempt: i64,
    attempts: i64,
    first_reported_at: Option<i64>,
    last_reported_at: Option<i64>,
    span_secs: i64,
}

/// Minimum release delay the runtime applies after failed attempt `attempt`.
fn backoff_floor(attempt: i64) -> i64 {
    let power = attempt.saturating_sub(1).clamp(0, 6) as u32;
    (60_i64 << power).min(3600)
}

/// Lower bound on the time between the oldest and newest report of a run.
/// `run` is newest first; each entry is `(attempt, reported_at)`.
fn span_lower_bound(run: &[(i64, Option<i64>)]) -> i64 {
    run.windows(2)
        .map(|pair| match (pair[0].1, pair[1].1) {
            (Some(newer), Some(older)) => newer.saturating_sub(older).max(0),
            _ => backoff_floor(pair[1].0),
        })
        .fold(0_i64, i64::saturating_add)
}

/// Whether an identical run (newest first) already satisfies the rule.
fn meets(run: &[(i64, Option<i64>)], rule: &UnavailableRule) -> bool {
    let needed = rule.attempts as usize;
    run.len() >= needed && span_lower_bound(&run[..needed]) >= i64::from(rule.min_span_secs)
}

/// Highest job attempt at which an in-progress identical run still exempts its
/// job from the failure pause: twice the rule's attempt count (10 by default).
/// This tolerates up to `attempts` interrupting attempts (another failure kind,
/// or an attempt without a report) while bounding the exemption: past the
/// ceiling the job counts as a persistent failure whatever it reports, so a
/// run that keeps restarting cannot hold the pause off forever.
fn exemption_ceiling(rule: &UnavailableRule) -> i64 {
    2 * i64::from(rule.attempts)
}

/// One stored failure report with its attempt's received-frame count.
struct Reported {
    attempt: i64,
    report: FailureDiagnostic,
    reported_at: Option<i64>,
    received: u64,
}

impl Reported {
    /// A fully enumerated, explicitly requested set within the rule's size cap.
    fn qualifies(&self, rule: &UnavailableRule) -> bool {
        self.report.kind == FailureKind::Unavailable
            && !self.report.sample.is_empty()
            && self.report.missing_count == self.report.sample.len() as u64
            && self.report.sample.len() <= rule.max_ids as usize
            && self.received.saturating_add(self.report.missing_count) <= WORKER_FETCH_BATCH
    }
}

type ReportRow = (i64, Option<String>, Option<i64>, u64, bool);

/// The job's failure reports after its last protocol-complete attempt (which
/// means the relay delivered every advertised ID then), newest first, at most
/// `limit`. Rows are parsed lazily by [`parse_report`].
fn reports_since_delivery(
    db: &Connection,
    job: i64,
    attempt: i64,
    limit: i64,
) -> Result<Vec<ReportRow>, LedgerError> {
    let delivered: i64 = db.query_row(
        "SELECT coalesce(max(attempt),0) FROM attempts WHERE job=?1 AND protocol_done=1",
        [job],
        |r| r.get(0),
    )?;
    let mut query = db.prepare(
        "SELECT f.attempt, CASE WHEN length(f.report)<=32768 THEN f.report END, f.reported_at,
                coalesce(a.received,0), coalesce(a.protocol_done,0)
         FROM failure_reports f LEFT JOIN attempts a ON a.job=f.job AND a.attempt=f.attempt
         WHERE f.job=?1 AND f.attempt<=?2 AND f.attempt>?3 ORDER BY f.attempt DESC LIMIT ?4",
    )?;
    Ok(query
        .query_map(params![job, attempt, delivered, limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Unparseable stored diagnostics are corruption and fail closed.
fn parse_report(
    (attempt, json, reported_at, received, protocol_done): ReportRow,
) -> Result<Reported, LedgerError> {
    let report: FailureDiagnostic = json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .filter(FailureDiagnostic::validate)
        .ok_or(LedgerError::Invalid("invalid stored failure diagnostic"))?;
    if protocol_done {
        return Err(LedgerError::Invalid("failed attempt marked protocol done"));
    }
    Ok(Reported {
        attempt,
        report,
        reported_at,
        received,
    })
}

/// The identical qualifying run of the job's most recent reported attempts, if it
/// meets the rule. Attempts without a report (a lost worker, a lease consumed
/// without a session, a parent-side protocol or deadline error) carry no relay
/// evidence and are skipped, never counted; a protocol-complete attempt, any
/// other report kind or a different set ends the run.
fn evidence(
    db: &Connection,
    job: &super::Job,
    rule: &UnavailableRule,
) -> Result<Option<Evidence>, LedgerError> {
    if job.state != JobState::RetryWait || job.attempt < i64::from(rule.attempts) {
        return Ok(None);
    }
    let mut ids: Option<Vec<[u8; 32]>> = None;
    let mut run = Vec::new();
    for row in reports_since_delivery(db, job.id, job.attempt, MAX_EVIDENCE_ATTEMPTS)? {
        let reported = parse_report(row)?;
        if !reported.qualifies(rule)
            || ids
                .as_ref()
                .is_some_and(|ids| *ids != reported.report.sample)
        {
            break;
        }
        ids.get_or_insert(reported.report.sample);
        run.push((reported.attempt, reported.reported_at));
    }
    if !meets(&run, rule) {
        return Ok(None);
    }
    let (Some(ids), Some(&(first_attempt, first_reported_at))) = (ids, run.last()) else {
        return Ok(None);
    };
    Ok(Some(Evidence {
        ids,
        first_attempt,
        last_attempt: run[0].0,
        attempts: run.len() as i64,
        first_reported_at,
        last_reported_at: run[0].1,
        span_secs: span_lower_bound(&run),
    }))
}

/// Whether a retry-wait job at `attempt` is on track to a relay-unavailable
/// completion and so does not count toward the failure pause. All of:
///
/// - `attempt` is at most [`exemption_ceiling`];
/// - its latest reported attempt qualifies (an `unavailable` report naming a
///   fully enumerated, explicitly requested set of at most `max_ids` IDs);
/// - every `unavailable` report since its last protocol-complete attempt
///   qualifies and names that same set (a changed or unqualifiable set is not
///   on track); reports of other kinds and attempts without a report may
///   interrupt the run;
/// - the current identical run does not meet the rule yet. A run that meets it
///   but is still in retry was held back (receipts not yet archived) and counts
///   again, so the exemption cannot outlast the evidence it waits for.
fn on_track(
    db: &Connection,
    job: i64,
    attempt: i64,
    rule: &UnavailableRule,
) -> Result<bool, LedgerError> {
    let ceiling = exemption_ceiling(rule);
    if attempt > ceiling {
        return Ok(false);
    }
    let mut ids: Option<Vec<[u8; 32]>> = None;
    let mut run = Vec::new();
    let mut open = true;
    for row in reports_since_delivery(db, job, attempt, ceiling)? {
        let reported = parse_report(row)?;
        if reported.report.kind != FailureKind::Unavailable {
            if run.is_empty() {
                // The latest reported attempt is not an unavailable report.
                return Ok(false);
            }
            open = false;
            continue;
        }
        if !reported.qualifies(rule)
            || ids
                .as_ref()
                .is_some_and(|ids| *ids != reported.report.sample)
        {
            return Ok(false);
        }
        ids.get_or_insert(reported.report.sample);
        if open {
            run.push((reported.attempt, reported.reported_at));
        }
    }
    Ok(!run.is_empty() && !meets(&run, rule))
}

/// Relay-unavailable exclusions owned by one job, for coverage accounting.
pub(super) fn count_for_job(tx: &Transaction<'_>, job: i64) -> Result<i64, LedgerError> {
    Ok(tx.query_row(
        "SELECT count(*) FROM relay_unavailable_exclusions WHERE job=?1",
        [job],
        |r| r.get(0),
    )?)
}

fn report_unresolved(db: &Connection) -> Result<(), LedgerError> {
    let unresolved: i64 = db.query_row(
        "SELECT count(*) FROM relay_unavailable_exclusions WHERE resolved_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    metrics::gauge!("negentropy_relay_unavailable_exclusions_unresolved").set(unresolved as f64);
    Ok(())
}

impl JobLedger {
    /// Replace the automatic exclusion rule; `None` disables new exclusions.
    /// Recorded exclusions and late-arrival resolution are unaffected.
    pub fn set_relay_unavailable_rule(
        &mut self,
        rule: Option<UnavailableRule>,
    ) -> Result<(), LedgerError> {
        if let Some(rule) = &rule {
            rule.validate()?;
        }
        self.unavailable_rule = rule;
        Ok(())
    }

    /// Persistently failing jobs that count toward the runtime's failure pause,
    /// in ID order. The candidates are exactly the jobs behind `unresolved_failures`
    /// (blocked, or in retry wait after at least three failed attempts); a
    /// retry-wait job whose in-progress identical unavailable run is on track to
    /// a relay-unavailable completion (see `on_track`) is left out. Health is
    /// unaffected: such a job still counts toward `unresolved_failures`. With the
    /// rule disabled nothing is exempt. Reads at most `MAX_EXEMPTION_CHECKS`
    /// report histories; any further candidate is counted.
    pub fn pause_failures(&self) -> Result<Vec<i64>, LedgerError> {
        let failing = {
            let mut query = self.db.prepare(
                "SELECT id,state='retry_wait',attempt FROM jobs
                 WHERE state='blocked' OR (state='retry_wait' AND attempt>=3) ORDER BY id",
            )?;
            query
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut counted = Vec::with_capacity(failing.len());
        let mut checked = 0;
        let mut exempt = 0_u32;
        for (id, retrying, attempt) in failing {
            if let Some(rule) = &self.unavailable_rule
                && retrying
                && attempt <= exemption_ceiling(rule)
                && checked < MAX_EXEMPTION_CHECKS
            {
                checked += 1;
                if on_track(&self.db, id, attempt, rule)? {
                    exempt += 1;
                    continue;
                }
            }
            counted.push(id);
        }
        metrics::gauge!("negentropy_isolated_failure_pause_exempt_jobs").set(f64::from(exempt));
        metrics::gauge!("negentropy_isolated_failure_pause_counted_jobs").set(counted.len() as f64);
        Ok(counted)
    }

    /// One bounded turn: complete retry-wait jobs whose history proves the relay
    /// withholds the same advertised IDs, then mark earlier exclusions resolved
    /// once their event carries a durable archive marker. Call only when archive
    /// recovery is not required. A qualifying job with unarchived receipts stays in
    /// retry; receipt maintenance must confirm them first. Like retry and expiry,
    /// this resolves an existing obligation and bypasses the admission ceiling;
    /// real storage errors still roll back.
    pub fn exclude_relay_unavailable(
        &mut self,
        now: i64,
        dedupe: &crate::DedupeIndex,
    ) -> Result<UnavailableProgress, LedgerError> {
        if now < 0 {
            return Err(LedgerError::Invalid("invalid exclusion time"));
        }
        let mut progress = UnavailableProgress::default();
        if let Some(rule) = self.unavailable_rule {
            let candidates = {
                let mut query = self.db.prepare(
                    "SELECT id FROM jobs WHERE state='retry_wait' AND attempt>=?1 AND id>?2
                     ORDER BY id LIMIT ?3",
                )?;
                query
                    .query_map(
                        params![rule.attempts, self.unavailable_cursor, MAX_JOBS_PER_TURN],
                        |r| r.get::<_, i64>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?
            };
            self.unavailable_cursor = match candidates.last() {
                Some(&last) if candidates.len() as i64 == MAX_JOBS_PER_TURN => last,
                _ => 0,
            };
            for id in candidates {
                // Read-only screen first; only a qualifying job takes the write lock
                // and is re-checked inside it.
                if evidence(&self.db, &read_job(&self.db, id)?, &rule)?.is_none() {
                    continue;
                }
                let tx = self
                    .db
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                let job = read_job(&tx, id)?;
                let Some(evidence) = evidence(&tx, &job, &rule)? else {
                    continue;
                };
                let pending: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM receipts WHERE job=?1)",
                    [id],
                    |r| r.get(0),
                )?;
                if pending {
                    progress.awaiting_archive += 1;
                    continue;
                }
                // An ID that already carries a durable archive marker is in the
                // archive, whichever path brought it in, so it is not missing. An
                // event archived before the inventory replay floor is never in the
                // inventory: every attempt's diff names it again, so without this
                // the window could neither finish normally nor be excluded.
                let mut withheld = Vec::with_capacity(evidence.ids.len());
                for event_id in &evidence.ids {
                    if dedupe.get_status(event_id)? != Some(crate::EventStatus::Archived) {
                        withheld.push(*event_id);
                    }
                }
                for event_id in &withheld {
                    tx.execute(
                        "INSERT INTO relay_unavailable_exclusions(event_id,job,relay,since,until,
                         first_attempt,last_attempt,attempts,first_reported_at,last_reported_at,
                         span_secs,excluded_at,reason) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                        params![
                            event_id.as_slice(),
                            job.id,
                            job.relay,
                            job.since,
                            job.until,
                            evidence.first_attempt,
                            evidence.last_attempt,
                            evidence.attempts,
                            evidence.first_reported_at,
                            evidence.last_reported_at,
                            evidence.span_secs,
                            now,
                            RELAY_UNAVAILABLE
                        ],
                    )?;
                }
                let reason = if withheld.is_empty() {
                    ALREADY_ARCHIVED
                } else {
                    RELAY_UNAVAILABLE
                };
                let changed = tx.execute(
                    "UPDATE jobs SET state='complete',reason=?3,token=NULL,expires_at=NULL
                     WHERE id=?1 AND state='retry_wait' AND attempt=?2",
                    params![job.id, job.attempt, reason],
                )?;
                if changed != 1 {
                    return Err(LedgerError::Invalid("exclusion candidate changed"));
                }
                if let Some(parent) = job.parent {
                    complete_if_durable(&tx, parent)?;
                }
                tx.commit()?;
                if withheld.is_empty() {
                    progress.already_archived.push(job.id);
                    continue;
                }
                progress.excluded.push(UnavailableExclusion {
                    job: job.id,
                    relay: job.relay,
                    event_ids: withheld,
                    first_attempt: evidence.first_attempt,
                    last_attempt: evidence.last_attempt,
                    attempts: evidence.attempts,
                    span_secs: evidence.span_secs,
                });
            }
        }
        progress.resolved = self.resolve_late_arrivals(now, dedupe)?;
        report_unresolved(&self.db)?;
        Ok(progress)
    }

    /// Mark exclusions resolved when their event later gained a durable archive
    /// marker (live ingestion, another relay or a later sweep). Informational only:
    /// the job is already complete and nothing else changes.
    fn resolve_late_arrivals(
        &mut self,
        now: i64,
        dedupe: &crate::DedupeIndex,
    ) -> Result<u32, LedgerError> {
        let rows = {
            let mut query = self.db.prepare(
                "SELECT id,event_id FROM relay_unavailable_exclusions
                 INDEXED BY relay_unavailable_unresolved
                 WHERE resolved_at IS NULL AND id>?1 ORDER BY id LIMIT ?2",
            )?;
            query
                .query_map(params![self.late_cursor, MAX_LATE_CHECKS], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, [u8; 32]>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        self.late_cursor = match rows.last() {
            Some(&(last, _)) if rows.len() as i64 == MAX_LATE_CHECKS => last,
            _ => 0,
        };
        let mut archived = Vec::new();
        for (id, event_id) in rows {
            if dedupe.get_status(&event_id)? == Some(crate::EventStatus::Archived) {
                archived.push(id);
            }
        }
        if archived.is_empty() {
            return Ok(0);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in &archived {
            tx.execute(
                "UPDATE relay_unavailable_exclusions SET resolved_at=?2 WHERE id=?1 AND resolved_at IS NULL",
                params![id, now],
            )?;
        }
        tx.commit()?;
        Ok(archived.len() as u32)
    }

    /// The exclusion records of one job, oldest first: event ID and the time it
    /// was later found archived, if it was.
    pub fn relay_unavailable_exclusions(
        &self,
        job: i64,
    ) -> Result<Vec<ExclusionStatus>, LedgerError> {
        let mut query = self.db.prepare(
            "SELECT event_id,resolved_at FROM relay_unavailable_exclusions WHERE job=?1 ORDER BY id",
        )?;
        Ok(query
            .query_map([job], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Report time of one failure diagnostic, when it was recorded (schema 10+).
    pub fn failure_reported_at(&self, job: i64, attempt: i64) -> Result<Option<i64>, LedgerError> {
        Ok(self
            .db
            .query_row(
                "SELECT reported_at FROM failure_reports WHERE job=?1 AND attempt=?2",
                params![job, attempt],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }
}

#[cfg(test)]
pub(super) fn strip_for_v9_fixture(db: &Connection) {
    db.execute_batch(
        "DROP INDEX relay_unavailable_unresolved; DROP TABLE relay_unavailable_exclusions;
        ALTER TABLE failure_reports DROP COLUMN reported_at;
        ALTER TABLE completed_coverage DROP COLUMN relay_unavailable; PRAGMA user_version=9;",
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::ipc::ReceiptRecord;
    use crate::sync::jobs::{Lease, LedgerLimits, RetryReason, SplitOutcome};

    const RELAY: &str = "wss://relay.example.com";
    const MISSING: [u8; 32] = [0x08; 32];
    const HOUR: i64 = 3600;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: JobLedger,
        dedupe: std::sync::Arc<crate::DedupeIndex>,
        writer: crate::SegmentWriter,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (dedupe, writer) = super::super::tests::archive(&dir);
        let db = JobLedger::open(&dir.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        Fixture {
            _dir: dir,
            db,
            dedupe,
            writer,
        }
    }

    fn unavailable(ids: &[[u8; 32]]) -> FailureDiagnostic {
        FailureDiagnostic {
            kind: FailureKind::Unavailable,
            missing_count: ids.len() as u64,
            sample: ids.to_vec(),
        }
    }

    impl Fixture {
        /// One leased attempt at `at` that receives `received` archived events and
        /// ends with `report` (or, with `None`, loses its worker without a report).
        fn attempt(&mut self, at: i64, received: u64, report: Option<FailureDiagnostic>) -> Lease {
            let lease = self.db.lease_next(at, 600).unwrap().unwrap();
            for sequence in 1..=received {
                let mut id = [0xee; 32];
                id[31] = sequence as u8;
                self.db
                    .register_received(
                        &lease,
                        &ReceiptRecord {
                            sequence,
                            event_id: id,
                            created_at: lease.job().since as u64,
                            frame_bytes: 400,
                            frame_digest: [sequence as u8; 32],
                        },
                        at,
                    )
                    .unwrap();
                self.dedupe.mark_archived([&id].into_iter()).unwrap();
            }
            match report {
                Some(report) => {
                    self.db
                        .record_failure_report(&lease, received + 1, &report, at + 1)
                        .unwrap();
                    self.db
                        .retry(&lease, at + 1, 60, RetryReason::RelayFailure)
                        .unwrap();
                }
                None => {
                    self.db
                        .retry(&lease, at + 1, 60, RetryReason::WorkerLost)
                        .unwrap();
                }
            }
            self.db
                .maintain_archived(&self.dedupe, &self.writer, 32, 256)
                .unwrap();
            lease
        }

        fn run(&mut self, now: i64) -> UnavailableProgress {
            self.db
                .exclude_relay_unavailable(now, &self.dedupe)
                .unwrap()
        }
    }

    #[test]
    fn identical_hourly_reports_complete_the_window_with_proof() {
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 17, Some(unavailable(&[MISSING])));
            if n < 4 {
                assert!(f.run(1000 + n * HOUR + 2).excluded.is_empty());
            }
        }
        let progress = f.run(1000 + 4 * HOUR + 2);
        assert_eq!(progress.excluded.len(), 1);
        let excluded = &progress.excluded[0];
        assert_eq!(
            (excluded.job, excluded.first_attempt, excluded.last_attempt),
            (job.id, 1, 5)
        );
        assert_eq!(excluded.event_ids, vec![MISSING]);
        assert_eq!(excluded.span_secs, 4 * HOUR);
        let done = f.db.get(job.id).unwrap();
        assert_eq!(done.state, JobState::Complete);
        assert_eq!(done.reason.as_deref(), Some(RELAY_UNAVAILABLE));
        assert_eq!(
            f.db.relay_unavailable_exclusions(job.id).unwrap(),
            vec![(MISSING, None)]
        );
        // Receipts and attempt summaries are untouched: nothing claims archival.
        for attempt in 1..=5 {
            let summary = f.db.attempt_progress(job.id, attempt).unwrap();
            assert_eq!(
                (summary.received, summary.archived, summary.excluded),
                (17, 17, 0)
            );
            assert!(!summary.protocol_done);
        }
        let row: (i64, i64, i64, i64, i64, String) =
            f.db.db
                .query_row(
                    "SELECT first_reported_at,last_reported_at,attempts,since,until,reason
                 FROM relay_unavailable_exclusions",
                    [],
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
                )
                .unwrap();
        assert_eq!(
            row,
            (1001, 1001 + 4 * HOUR, 5, 0, 899, RELAY_UNAVAILABLE.into())
        );
        // Health no longer reports the job as an unresolved failure.
        assert!(
            !f.db
                .progress_observation(20_000)
                .unwrap()
                .persistent_failure
        );
    }

    #[test]
    fn differing_sets_do_not_exclude() {
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..6 {
            let ids = if n == 2 {
                vec![MISSING, [0x09; 32]]
            } else {
                vec![MISSING]
            };
            f.attempt(1000 + n * HOUR, 17, Some(unavailable(&ids)));
        }
        // Attempt 3 differs, so the latest identical run (4-6) is too short.
        assert!(f.run(1000 + 6 * HOUR).excluded.is_empty());
        f.attempt(1000 + 6 * HOUR, 17, Some(unavailable(&[MISSING])));
        assert!(f.run(1000 + 7 * HOUR).excluded.is_empty());
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
        // Attempts 4-8 finally agree for five consecutive hours.
        f.attempt(1000 + 7 * HOUR, 17, Some(unavailable(&[MISSING])));
        let progress = f.run(1000 + 8 * HOUR);
        assert_eq!(progress.excluded.len(), 1);
        assert_eq!(progress.excluded[0].first_attempt, 4);
    }

    #[test]
    fn incompletely_enumerated_sets_do_not_exclude() {
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        let mut report = unavailable(&[MISSING]);
        report.missing_count = 2; // Sampled one of two outstanding IDs.
        for n in 0..6 {
            f.attempt(1000 + n * HOUR, 0, Some(report.clone()));
        }
        assert!(f.run(1000 + 6 * HOUR).excluded.is_empty());
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
    }

    #[test]
    fn diffs_larger_than_one_fetch_batch_or_rule_cap_do_not_exclude() {
        // 127 received + 2 missing exceeds one 128-ID fetch: the missing IDs may
        // include later batches the worker never requested.
        let mut f = fixture();
        f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 127, Some(unavailable(&[MISSING, [9; 32]])));
        }
        assert!(f.run(1000 + 5 * HOUR).excluded.is_empty());
        // Nine identical IDs exceed the default eight-ID cap.
        let mut f = fixture();
        f.db.enqueue("s", RELAY, 0, 899).unwrap();
        let ids: Vec<_> = (1..=9).map(|n| [n; 32]).collect();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&ids)));
        }
        assert!(f.run(1000 + 5 * HOUR).excluded.is_empty());
        f.db.set_relay_unavailable_rule(Some(UnavailableRule {
            max_ids: 9,
            ..UnavailableRule::default()
        }))
        .unwrap();
        assert_eq!(f.run(1000 + 5 * HOUR).excluded[0].event_ids, ids);
    }

    #[test]
    fn short_span_does_not_exclude() {
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        // Five identical reports 14 minutes apart span 56 minutes < 1 hour.
        for n in 0..5 {
            f.attempt(1000 + n * 840, 0, Some(unavailable(&[MISSING])));
        }
        assert!(f.run(1000 + 5 * 840).excluded.is_empty());
        f.attempt(1000 + 5 * 840, 0, Some(unavailable(&[MISSING])));
        // Attempts 2-6 span 56 minutes too; the run is not measured from attempt 1.
        assert!(f.run(1000 + 6 * 840).excluded.is_empty());
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
        f.attempt(1000 + 6 * 840 + 1200, 0, Some(unavailable(&[MISSING])));
        assert_eq!(f.run(1000 + 8 * HOUR).excluded.len(), 1);
    }

    #[test]
    fn non_consecutive_or_mixed_failures_do_not_exclude() {
        let relay = FailureDiagnostic {
            kind: FailureKind::Relay,
            missing_count: 0,
            sample: Vec::new(),
        };
        // A different failure kind between identical reports breaks the run.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..6 {
            let report = if n == 2 {
                relay.clone()
            } else {
                unavailable(&[MISSING])
            };
            f.attempt(1000 + n * HOUR, 0, Some(report));
        }
        assert!(f.run(1000 + 6 * HOUR).excluded.is_empty());
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
        // The latest reported attempt must itself qualify.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING])));
        }
        f.attempt(1000 + 5 * HOUR, 0, Some(relay));
        assert!(f.run(1000 + 6 * HOUR).excluded.is_empty());
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
        // A protocol-complete attempt (the relay delivered everything advertised
        // then) splits identical reports into two runs that do not combine.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..2 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING])));
        }
        let lease = f.db.lease_next(1000 + 2 * HOUR, 600).unwrap().unwrap();
        f.db.record_protocol_done(
            &lease,
            1,
            0,
            crate::sync::ipc::initial_digest(),
            1000 + 2 * HOUR,
        )
        .unwrap();
        f.db.retry_durability(job.id, 3, 1000 + 2 * HOUR + 1, 60)
            .unwrap();
        for n in 3..7 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING])));
        }
        assert!(f.run(1000 + 7 * HOUR).excluded.is_empty());
        f.attempt(1000 + 7 * HOUR, 0, Some(unavailable(&[MISSING])));
        let progress = f.run(1000 + 8 * HOUR);
        assert_eq!(
            (
                progress.excluded[0].first_attempt,
                progress.excluded[0].last_attempt
            ),
            (4, 8)
        );
    }

    #[test]
    fn attempts_without_any_report_are_skipped_not_counted() {
        // A lost worker mid-run and a lease consumed without a session after the
        // last report (job 768's attempt 16) carry no relay evidence.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..6 {
            let report = if n == 2 {
                None
            } else {
                Some(unavailable(&[MISSING]))
            };
            f.attempt(1000 + n * HOUR, 3, report);
        }
        let lease = f.db.lease_next(1000 + 6 * HOUR, 600).unwrap().unwrap();
        assert!(f.db.expire(lease.expires_at(), 60).unwrap());
        assert_eq!(f.db.get(job.id).unwrap().attempt, 7);
        let progress = f.run(1000 + 7 * HOUR);
        let excluded = &progress.excluded[0];
        assert_eq!(
            (
                excluded.first_attempt,
                excluded.last_attempt,
                excluded.attempts
            ),
            (1, 6, 5)
        );
        let attempts: i64 =
            f.db.db
                .query_row(
                    "SELECT attempts FROM relay_unavailable_exclusions WHERE job=?1",
                    [job.id],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(attempts, 5);
        // Four reports around a gap are still too few.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            let report = if n == 2 {
                None
            } else {
                Some(unavailable(&[MISSING]))
            };
            f.attempt(1000 + n * HOUR, 0, report);
        }
        assert!(f.run(1000 + 5 * HOUR).excluded.is_empty());
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
    }

    #[test]
    fn already_archived_ids_count_as_delivered() {
        // Every withheld ID is archived (for example before the replay floor, so
        // the inventory never lists it and each diff names it again): the window
        // completes without an exclusion, because nothing is missing.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 17, Some(unavailable(&[MISSING])));
        }
        f.dedupe.mark_archived([&MISSING].into_iter()).unwrap();
        let progress = f.run(1000 + 5 * HOUR);
        assert!(progress.excluded.is_empty());
        assert_eq!(progress.already_archived, vec![job.id]);
        let done = f.db.get(job.id).unwrap();
        assert_eq!(done.state, JobState::Complete);
        assert_eq!(done.reason.as_deref(), Some(ALREADY_ARCHIVED));
        assert!(
            f.db.relay_unavailable_exclusions(job.id)
                .unwrap()
                .is_empty()
        );
        // Like any completion it advances the durable last-success time.
        let observation = f.db.progress_observation(1000 + 5 * HOUR).unwrap();
        assert!(observation.last_success.is_some());
        assert!(!observation.persistent_failure);

        // Mixed: only the IDs the archive lacks are excluded.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        let other = [0x09; 32];
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING, other])));
        }
        f.dedupe.mark_archived([&MISSING].into_iter()).unwrap();
        let progress = f.run(1000 + 5 * HOUR);
        assert!(progress.already_archived.is_empty());
        assert_eq!(progress.excluded[0].event_ids, vec![other]);
        assert_eq!(
            f.db.get(job.id).unwrap().reason.as_deref(),
            Some(RELAY_UNAVAILABLE)
        );
        assert_eq!(
            f.db.relay_unavailable_exclusions(job.id).unwrap(),
            vec![(other, None)]
        );
    }

    #[test]
    fn a_diff_of_exactly_one_fetch_batch_qualifies() {
        // 127 received + 1 missing = 128: the whole diff was one requested batch.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 127, Some(unavailable(&[MISSING])));
        }
        let progress = f.run(1000 + 5 * HOUR);
        assert_eq!(progress.excluded.len(), 1);
        assert_eq!(progress.excluded[0].job, job.id);
    }

    #[test]
    fn keyset_cursors_cover_more_jobs_and_rows_than_one_turn() {
        // 40 qualifying jobs exceed MAX_JOBS_PER_TURN (32); their 80 exclusion
        // rows exceed MAX_LATE_CHECKS (64).
        const JOBS: i64 = 40;
        const { assert!(JOBS > MAX_JOBS_PER_TURN && 2 * JOBS > MAX_LATE_CHECKS) };
        let mut f = fixture();
        for n in 0..JOBS {
            f.db.enqueue("s", RELAY, n * 900, n * 900 + 899).unwrap();
        }
        let other = [0x09; 32];
        for round in 0..5 {
            for n in 0..JOBS {
                let lease = f.attempt(1000 + round * HOUR, 0, Some(unavailable(&[MISSING, other])));
                assert_eq!(lease.job().id, n + 1);
            }
        }
        let now = 1000 + 5 * HOUR;
        let first = f.run(now);
        let ids: Vec<_> = first.excluded.iter().map(|e| e.job).collect();
        assert_eq!(ids, (1..=MAX_JOBS_PER_TURN).collect::<Vec<_>>());
        let second = f.run(now + 5);
        let ids: Vec<_> = second.excluded.iter().map(|e| e.job).collect();
        assert_eq!(ids, (MAX_JOBS_PER_TURN + 1..=JOBS).collect::<Vec<_>>());
        // The candidate cursor wrapped: nothing is left and nothing repeats.
        assert!(f.run(now + 10).excluded.is_empty());
        let rows: i64 =
            f.db.db
                .query_row(
                    "SELECT count(*) FROM relay_unavailable_exclusions WHERE resolved_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(rows, 2 * JOBS);
        // Both events later reach the archive. Each turn checks at most 64 rows,
        // continuing from where the previous turn stopped, so two turns resolve
        // all 80 and a third finds nothing left.
        f.dedupe
            .mark_archived([&MISSING, &other].into_iter())
            .unwrap();
        let turns = [f.run(now + 15).resolved, f.run(now + 20).resolved];
        assert!(turns.iter().all(|&n| i64::from(n) <= MAX_LATE_CHECKS));
        assert_eq!(i64::from(turns[0] + turns[1]), 2 * JOBS);
        assert_eq!(f.run(now + 25).resolved, 0);
        let unresolved: i64 =
            f.db.db
                .query_row(
                    "SELECT count(*) FROM relay_unavailable_exclusions WHERE resolved_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(unresolved, 0);
    }

    fn relay_failure() -> FailureDiagnostic {
        FailureDiagnostic {
            kind: FailureKind::Relay,
            missing_count: 0,
            sample: Vec::new(),
        }
    }

    #[test]
    fn pause_exempts_only_a_run_on_track_to_exclusion() {
        let at = |n: i64| 1000 + n * HOUR;
        // Three identical reports: unresolved for health, exempt from the pause.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..3 {
            f.attempt(at(n), 0, Some(unavailable(&[MISSING])));
        }
        assert!(f.db.progress_observation(at(3)).unwrap().persistent_failure);
        assert!(f.db.pause_failures().unwrap().is_empty());
        // With the rule disabled nothing will exclude it, so it counts.
        f.db.set_relay_unavailable_rule(None).unwrap();
        assert_eq!(f.db.pause_failures().unwrap(), vec![job.id]);
        f.db.set_relay_unavailable_rule(Some(UnavailableRule::default()))
            .unwrap();
        // A relay failure as the latest report counts; the next identical
        // report restarts a run that is on track again (counter-example (a)).
        f.attempt(at(3), 0, Some(relay_failure()));
        assert_eq!(f.db.pause_failures().unwrap(), vec![job.id]);
        f.attempt(at(4), 0, Some(unavailable(&[MISSING])));
        assert!(f.db.pause_failures().unwrap().is_empty());
        // So does an attempt without a report, inside the ceiling.
        f.attempt(at(5), 0, None);
        assert!(f.db.pause_failures().unwrap().is_empty());

        // A changed set is not on track, even when the latest set repeats.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for ids in [
            vec![MISSING],
            vec![MISSING, [9; 32]],
            vec![MISSING],
            vec![MISSING],
        ] {
            f.attempt(at(0), 0, Some(unavailable(&ids)));
            f.db.db
                .execute("UPDATE jobs SET next_eligible=0 WHERE id=?1", [job.id])
                .unwrap();
        }
        assert_eq!(f.db.pause_failures().unwrap(), vec![job.id]);

        // A diff larger than one fetch batch, or a set above max_ids, cannot qualify.
        for (received, ids) in [
            (128, vec![MISSING]),
            (0, (1..=9).map(|n| [n; 32]).collect::<Vec<_>>()),
        ] {
            let mut f = fixture();
            let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
            for n in 0..3 {
                f.attempt(at(n), received, Some(unavailable(&ids)));
            }
            assert_eq!(f.db.pause_failures().unwrap(), vec![job.id]);
        }

        // The ceiling: past 2 x attempts (10) the job counts whatever it reports.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        f.attempt(at(0), 0, Some(unavailable(&[MISSING])));
        for n in 1..9 {
            // Lost workers carry no evidence; the run never grows.
            f.attempt(at(n), 0, None);
        }
        f.attempt(at(9), 0, Some(unavailable(&[MISSING])));
        assert_eq!(f.db.get(job.id).unwrap().attempt, 10);
        assert!(f.db.pause_failures().unwrap().is_empty());
        f.attempt(at(10), 0, Some(unavailable(&[MISSING])));
        assert_eq!(f.db.pause_failures().unwrap(), vec![job.id]);

        // A run that met the rule but is held back counts again.
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(at(n), 0, Some(unavailable(&[MISSING])));
        }
        f.db.db
            .execute(
                "INSERT INTO attempts(job,attempt,received,bytes,digest) VALUES(?1,99,1,1,zeroblob(32))",
                [job.id],
            )
            .unwrap();
        f.db.db
            .execute(
                "INSERT INTO receipts VALUES(?1,99,1,zeroblob(32),1,1,zeroblob(32))",
                [job.id],
            )
            .unwrap();
        assert_eq!(f.run(at(5)).awaiting_archive, 1);
        assert_eq!(f.db.pause_failures().unwrap(), vec![job.id]);

        // Blocked jobs always count; corruption fails closed.
        let mut f = fixture();
        let blocked = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        f.db.db
            .execute("UPDATE jobs SET state='blocked' WHERE id=?1", [blocked.id])
            .unwrap();
        let job = f.db.enqueue("s", RELAY, 900, 1799).unwrap();
        for n in 0..3 {
            f.attempt(at(n), 0, Some(unavailable(&[MISSING])));
        }
        assert_eq!(f.db.pause_failures().unwrap(), vec![blocked.id]);
        f.db.db
            .execute(
                "UPDATE failure_reports SET report='{}' WHERE job=?1 AND attempt=2",
                [job.id],
            )
            .unwrap();
        assert!(matches!(
            f.db.pause_failures(),
            Err(LedgerError::Invalid(_))
        ));
    }

    #[test]
    fn unarchived_receipts_and_disabled_rule_defer_exclusion() {
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING])));
        }
        f.db.set_relay_unavailable_rule(None).unwrap();
        assert!(f.run(1000 + 5 * HOUR).excluded.is_empty());
        f.db.set_relay_unavailable_rule(Some(UnavailableRule::default()))
            .unwrap();
        // A retained (not yet archived) receipt keeps the job in retry.
        f.db
            .db
            .execute(
                "INSERT INTO attempts(job,attempt,received,bytes,digest) VALUES(?1,99,1,1,zeroblob(32))",
                [job.id],
            )
            .unwrap();
        f.db.db
            .execute(
                "INSERT INTO receipts VALUES(?1,99,1,zeroblob(32),1,1,zeroblob(32))",
                [job.id],
            )
            .unwrap();
        let progress = f.run(1000 + 5 * HOUR);
        assert!(progress.excluded.is_empty());
        assert_eq!(progress.awaiting_archive, 1);
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
        assert!(
            UnavailableRule {
                attempts: 2,
                ..UnavailableRule::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn corrupt_stored_diagnostic_fails_closed() {
        let mut f = fixture();
        let job = f.db.enqueue("s", RELAY, 0, 899).unwrap();
        for n in 0..5 {
            f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING])));
        }
        f.db.db
            .execute(
                "UPDATE failure_reports SET report='{}' WHERE job=?1 AND attempt=3",
                [job.id],
            )
            .unwrap();
        assert!(matches!(
            f.db.exclude_relay_unavailable(1000 + 5 * HOUR, &f.dedupe),
            Err(LedgerError::Invalid(_))
        ));
        assert_eq!(f.db.get(job.id).unwrap().state, JobState::RetryWait);
        assert!(
            f.db.relay_unavailable_exclusions(job.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn split_child_exclusion_completes_parent_and_late_arrival_resolves() {
        let mut f = fixture();
        let parent = f.db.enqueue("s", RELAY, 0, 1).unwrap();
        let lease = f.db.lease_next(10, 60).unwrap().unwrap();
        let SplitOutcome::Children(children) = f.db.split(&lease, 11).unwrap() else {
            panic!("expected children");
        };
        let [first, second] = *children;
        let done = f.db.lease_next(12, 60).unwrap().unwrap();
        assert_eq!(done.job().id, first.id);
        f.db.record_protocol_done(&done, 1, 0, crate::sync::ipc::initial_digest(), 13)
            .unwrap();
        f.db.maintain_archived(&f.dedupe, &f.writer, 32, 256)
            .unwrap();
        for n in 0..5 {
            let lease = f.attempt(1000 + n * HOUR, 0, Some(unavailable(&[MISSING])));
            assert_eq!(lease.job().id, second.id);
        }
        assert_eq!(f.run(1000 + 5 * HOUR).excluded.len(), 1);
        assert_eq!(f.db.get(parent.id).unwrap().state, JobState::Complete);
        // The event later reaches the archive by another path.
        f.dedupe.mark_archived([&MISSING].into_iter()).unwrap();
        let progress = f.run(1000 + 6 * HOUR);
        assert_eq!(progress.resolved, 1);
        assert_eq!(
            f.db.relay_unavailable_exclusions(second.id).unwrap(),
            vec![(MISSING, Some(1000 + 6 * HOUR))]
        );
        assert_eq!(f.run(1000 + 7 * HOUR).resolved, 0);
    }

    #[test]
    fn coverage_counts_relay_unavailable_apart_and_proof_outlives_compaction() {
        let mut f = fixture();
        f.db.configure_planner(&[RELAY]).unwrap();
        assert_eq!(f.db.plan_rolling(1800, 1).unwrap().enqueued, 1);
        for n in 0..5 {
            let lease = f.attempt(2000 + n * HOUR, 3, Some(unavailable(&[MISSING])));
            assert_eq!(lease.job().id, 1);
        }
        assert_eq!(f.run(2000 + 5 * HOUR).excluded.len(), 1);
        let completed: i64 =
            f.db.db
                .query_row("SELECT completed_at FROM jobs WHERE id=1", [], |r| r.get(0))
                .unwrap();
        assert_eq!(
            f.db.retain_completed(completed + 2 * 86_400)
                .unwrap()
                .removed_jobs,
            1
        );
        assert!(f.db.get(1).is_err());
        let coverage: (i64, i64, i64, i64, i64) = f
            .db
            .db
            .query_row(
                "SELECT received,archived,excluded,failures,relay_unavailable FROM completed_coverage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        // 15 archived frames, no policy exclusions, five failures, one withheld ID.
        assert_eq!(coverage, (15, 15, 0, 5, 1));
        assert_eq!(
            f.db.relay_unavailable_exclusions(1).unwrap(),
            vec![(MISSING, None)]
        );
    }

    type Snapshot = Vec<(&'static str, usize, Vec<Vec<rusqlite::types::Value>>)>;

    /// Every row of every pre-change table, in key order.
    fn snapshot(db: &Connection) -> Snapshot {
        [
            "jobs",
            "attempts",
            "receipts",
            "receipt_totals",
            "failure_reports",
            "planner_state",
            "planner_relays",
            "lease_rotation",
            "admission_policy",
            "policy_exclusions",
            "exclusion_totals",
            "lifecycle",
            "completed_coverage",
        ]
        .into_iter()
        .map(|table| {
            let mut query = db
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
                .unwrap();
            let width = query.column_count();
            let rows = query
                .query_map([], |r| {
                    (0..width)
                        .map(|i| r.get::<_, rusqlite::types::Value>(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            (table, width, rows)
        })
        .collect()
    }

    /// Restrict a later snapshot to the columns the earlier one had.
    fn original_columns(before: &Snapshot, after: Snapshot) -> Snapshot {
        before
            .iter()
            .zip(after)
            .map(|((_, width, _), (table, _, rows))| {
                let rows = rows.into_iter().map(|row| row[..*width].to_vec()).collect();
                (table, *width, rows)
            })
            .collect()
    }

    #[test]
    fn schema9_ledger_with_production_shaped_job_768_upgrades_and_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch(include_str!("testdata/ledger_v9_job768.sql"))
            .unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            9
        );
        let before = snapshot(&raw);
        drop(raw);
        let dedupe = crate::DedupeIndex::open(dir.path().join("dedupe")).unwrap();
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        assert_eq!(
            db.db
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            10
        );
        // Additive only: every pre-existing row and column value is unchanged.
        assert_eq!(original_columns(&before, snapshot(&db.db)), before);
        assert_eq!(db.failure_reported_at(768, 15).unwrap(), None);
        let job = db.get(768).unwrap();
        assert_eq!((job.state, job.attempt), (JobState::RetryWait, 15));
        assert!(
            db.progress_observation(1_791_300_000)
                .unwrap()
                .persistent_failure
        );
        // The first evaluation after deploy completes job 768, with no new attempt.
        let progress = db
            .exclude_relay_unavailable(1_791_300_000, &dedupe)
            .unwrap();
        let mut missing = [0u8; 32];
        hex::decode_to_slice(
            "089d1fdc2d1cf0f03be5844014378a3f2bcca0d1092619a70ed6f86b7c286b65",
            &mut missing,
        )
        .unwrap();
        assert_eq!(
            progress.excluded,
            vec![UnavailableExclusion {
                job: 768,
                relay: "wss://premium.primal.net".into(),
                event_ids: vec![missing],
                first_attempt: 1,
                last_attempt: 15,
                attempts: 15,
                // Legacy reports: the backoff floor after attempts 1-14.
                span_secs: 60 + 120 + 240 + 480 + 960 + 1920 + 8 * 3600,
            }]
        );
        let job = db.get(768).unwrap();
        assert_eq!((job.state, job.attempt), (JobState::Complete, 15));
        assert_eq!(job.reason.as_deref(), Some(RELAY_UNAVAILABLE));
        let row: String = db
            .db
            .query_row(
                "SELECT quote(relay)||','||since||','||until||','||attempts||','||
                 quote(first_reported_at)||','||quote(last_reported_at)||','||excluded_at||','||
                 quote(resolved_at) FROM relay_unavailable_exclusions WHERE job=768",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            row,
            "'wss://premium.primal.net',1791222300,1791223199,15,NULL,NULL,1791300000,NULL"
        );
        // Receipts, attempt summaries and failure reports are untouched.
        let after = snapshot(&db.db);
        for table in ["attempts", "receipts", "receipt_totals", "failure_reports"] {
            let old = before.iter().find(|t| t.0 == table).unwrap();
            let new = after.iter().find(|t| t.0 == table).unwrap();
            let new: Vec<_> = new.2.iter().map(|row| row[..old.1].to_vec()).collect();
            assert_eq!(old.2, new, "{table}");
        }
        assert!(
            !db.progress_observation(1_791_300_001)
                .unwrap()
                .persistent_failure
        );
        // Reopening is idempotent, and job IDs continue after 768.
        drop(db);
        let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        assert!(
            db.exclude_relay_unavailable(1_791_300_100, &dedupe)
                .unwrap()
                .excluded
                .is_empty()
        );
        assert_eq!(db.relay_unavailable_exclusions(768).unwrap().len(), 1);
        assert_eq!(
            db.enqueue("after", "wss://premium.primal.net", 0, 899)
                .unwrap()
                .id,
            769
        );
    }

    #[test]
    fn job_768_lease_race_attempt_without_a_session_does_not_reset_the_run() {
        // Production: the 2026-10-13 lease race consumed attempt 16 with no
        // session, receipt or report; the lease then expired into retry.
        for later_failure in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("jobs.sqlite");
            Connection::open(&path)
                .unwrap()
                .execute_batch(include_str!("testdata/ledger_v9_job768.sql"))
                .unwrap();
            let dedupe = crate::DedupeIndex::open(dir.path().join("dedupe")).unwrap();
            let mut db = JobLedger::open(&path, LedgerLimits::default()).unwrap();
            db.set_relay_unavailable_rule(None).unwrap();
            let raced = db.lease_next(1_791_300_000, 600).unwrap().unwrap();
            assert_eq!(raced.job().attempt, 16);
            assert!(db.expire(raced.expires_at(), 60).unwrap());
            let mut now = raced.expires_at() + 3600;
            if later_failure {
                // Attempt 17 fails the same way after deploy, with a report time.
                let lease = db.lease_next(now, 600).unwrap().unwrap();
                db.record_failure_report(
                    &lease,
                    1,
                    &db.failure_report(768, 15).unwrap().unwrap(),
                    now + 30,
                )
                .unwrap();
                db.retry(
                    &lease,
                    now + 30,
                    3600,
                    super::super::RetryReason::RelayFailure,
                )
                .unwrap();
                assert_eq!(db.failure_reported_at(768, 17).unwrap(), Some(now + 30));
                now += 60;
            }
            db.set_relay_unavailable_rule(Some(UnavailableRule::default()))
                .unwrap();
            let progress = db.exclude_relay_unavailable(now, &dedupe).unwrap();
            let excluded = &progress.excluded[0];
            assert_eq!(excluded.job, 768);
            assert_eq!(
                (
                    excluded.first_attempt,
                    excluded.last_attempt,
                    excluded.attempts
                ),
                if later_failure {
                    (1, 17, 16)
                } else {
                    (1, 15, 15)
                }
            );
            let job = db.get(768).unwrap();
            assert_eq!(job.state, JobState::Complete);
            assert_eq!(job.attempt, if later_failure { 17 } else { 16 });
        }
    }

    #[test]
    fn legacy_reports_use_the_backoff_floor() {
        // Five legacy reports for attempts 1-5: floor 60+120+240+480 = 900 s.
        let run: Vec<_> = (1..=5).rev().map(|a| (a, None)).collect();
        assert_eq!(span_lower_bound(&run), 900);
        // Attempts 3-7: 240+480+960+1920 = 3600 s, enough for the default rule.
        let run: Vec<_> = (3..=7).rev().map(|a| (a, None)).collect();
        assert_eq!(span_lower_bound(&run), 3600);
        // Attempts 11-15 (job 768's latest five): four capped hours.
        let run: Vec<_> = (11..=15).rev().map(|a| (a, None)).collect();
        assert_eq!(span_lower_bound(&run), 4 * 3600);
        // Known times are used as recorded; a clock step backwards counts zero.
        assert_eq!(span_lower_bound(&[(2, Some(100)), (1, Some(40))]), 60);
        assert_eq!(span_lower_bound(&[(2, Some(10)), (1, Some(40))]), 0);
        assert_eq!(span_lower_bound(&[(16, Some(10)), (15, None)]), 3600);
    }
}
