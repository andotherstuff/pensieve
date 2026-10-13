//! Explicitly enabled, single-worker isolated reconciliation runtime.
//!
//! The ingester owns the listener, ledger, inventory and archive. It never
//! launches or restarts the static systemd worker. All unfinished jobs survive
//! worker loss, shutdown and local faults; a socket exchange is not completion.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use metrics::{counter, gauge};
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use super::SyncStateDb;
use super::binding::{BindingError, WorkerBinding, WorkerCandidate};
use super::failure::FailureKind;
use super::jobs::{
    JobLedger, LedgerError, LedgerLimits, RetryReason, SplitOutcome, UnavailableRule,
};
use super::parent::{ParentError, ParentExecutor, SessionOutcome};
use crate::{DedupeIndex, SegmentWriter};

const TICK: Duration = Duration::from_secs(5);
const LEASE_SECS: u32 = 600;
const SESSION_TIME: Duration = Duration::from_secs(540);
const DATA_RESERVE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Deliberate, non-default local policy. A missing relay list is never filled
/// from the public relay catalog or legacy negentropy defaults.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Same immutable admission policy as the live source; bound to the ledger.
    pub admission_policy: crate::giftwrap_filter::AdmissionPolicy,
    /// Unix socket in an operator-provisioned restricted runtime directory.
    pub socket: PathBuf,
    /// Dedicated, unprivileged systemd worker account's numeric UID.
    pub worker_uid: u32,
    /// Durable ingester-owned SQLite path in an existing directory.
    pub ledger: PathBuf,
    /// Existing sync-state database; never opened by the worker.
    pub inventory: PathBuf,
    /// Exact archive namespace bound to the existing writer.
    pub archive: PathBuf,
    /// Exact writer segment prefix.
    pub segment_prefix: String,
    /// Operator-chosen first segment for sealed replay.
    pub replay_floor: u64,
    /// Explicit small NIP-77 relay allowlist, never catalog-augmented.
    pub relays: Vec<String>,
    /// Exclusive upper timestamp for three fixed 15-minute canary windows.
    /// Requires a fresh ledger and exactly one relay; never runs rolling planning.
    pub canary_until: Option<i64>,
    /// Stop leasing once one job has been a persistent failure (blocked, or in
    /// retry wait after at least three failed attempts) continuously for this
    /// long. The clock is per job, so failures of different jobs never add up,
    /// and a job whose latest reports form an identical unavailable run on track
    /// to a `relay_unavailable_rule` completion is exempt (bounded; see
    /// [`JobLedger::pause_failures`]). Exempt jobs still hold health red. `None`
    /// keeps retrying with paced backoff. The pause holds until no job is past
    /// the grace or the runtime restarts; every gap, receipt and archive
    /// maintenance turn is kept, and health stays red.
    pub pause_on_persistent_failure: Option<Duration>,
    /// Complete a window, with recorded proof, once its relay has persistently
    /// advertised the same IDs without delivering them, or after a single
    /// failure when an earlier window's exclusion already proves that relay
    /// withholds them (carried; see `jobs::unavailable`). `None` disables new
    /// exclusions, carried ones included; recorded ones are kept and still
    /// checked for late arrival.
    pub relay_unavailable_rule: Option<UnavailableRule>,
}

/// A local failure pauses isolated work without changing live archive ownership.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Invalid or unsupported activation policy.
    #[error("invalid isolated reconciliation configuration: {0}")]
    Config(&'static str),
    /// Local socket or filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An existing socket path may belong to a live owner and is never removed.
    #[error("isolated socket path already exists; inspect before operator recovery: {}", .0.display())]
    SocketOccupied(PathBuf),
    /// Durable ledger failure; no obligation is discarded.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// Parent owner/session failure.
    #[error(transparent)]
    Parent(#[from] ParentError),
    /// Worker identity or systemd lifetime proof failed.
    #[error(transparent)]
    Binding(#[from] BindingError),
    /// Archive inventory failure.
    #[error(transparent)]
    Archive(#[from] crate::Error),
    /// Runtime task failed.
    #[error("isolated runtime task failed")]
    Join,
}

/// Joined handle for one explicitly enabled ingester-side listener.
pub struct IsolatedRuntime {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), RuntimeError>>,
}

impl IsolatedRuntime {
    /// Validate policy, bind the socket and create the sole ledger owner before
    /// returning. No service is launched and no live ingestion path is changed.
    pub async fn start(
        config: RuntimeConfig,
        dedupe: Arc<DedupeIndex>,
        writer: Arc<SegmentWriter>,
    ) -> Result<Self, RuntimeError> {
        Self::start_bound::<WorkerCandidate>(config, dedupe, writer, DATA_RESERVE_BYTES).await
    }

    /// [`Self::start`] with the worker identity proof and the data-volume
    /// reserve as parameters. Production passes [`WorkerCandidate`] and
    /// [`DATA_RESERVE_BYTES`]; tests substitute a same-UID binding and a small
    /// reserve so they run on CI hosts without systemd or 20 GiB free.
    async fn start_bound<B: WorkerBinding>(
        config: RuntimeConfig,
        dedupe: Arc<DedupeIndex>,
        writer: Arc<SegmentWriter>,
        reserve_bytes: u64,
    ) -> Result<Self, RuntimeError> {
        if !cfg!(any(target_os = "linux", test)) {
            return Err(RuntimeError::Config("Linux worker binding required"));
        }
        if config.worker_uid == 0 || config.relays.is_empty() || config.relays.len() > 32 {
            return Err(RuntimeError::Config(
                "dedicated UID and 1-32 relays required",
            ));
        }
        if writer.recovery_required() {
            return Err(RuntimeError::Parent(ParentError::RecoveryRequired));
        }
        if fs::symlink_metadata(&config.socket).is_ok() {
            return Err(RuntimeError::SocketOccupied(config.socket.clone()));
        }
        // Bind before consuming the one-shot ledger. A failed startup removes
        // only this listener's socket, never an earlier owner's path or ledger.
        let listener = UnixListener::bind(&config.socket)?;
        let socket_cleanup = SocketCleanup {
            path: config.socket.clone(),
            inode: fs::symlink_metadata(&config.socket)?.ino(),
        };
        fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o660))?;
        if let Some(until) = config.canary_until {
            validate_canary(until, config.relays.len())?;
            // A fresh ledger prevents accidentally leasing unrelated retained work.
            // Refuse reuse, including after a parent restart; preserve it for inspection.
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config.ledger)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        RuntimeError::Config(
                            "canary ledger already exists; retain it and choose a new path",
                        )
                    } else {
                        RuntimeError::Io(error)
                    }
                })?;
        }
        let mut ledger = JobLedger::open_with_policy(
            &config.ledger,
            LedgerLimits::default(),
            config.admission_policy.clone(),
        )?;
        ledger.set_relay_unavailable_rule(config.relay_unavailable_rule)?;
        if let Some(until) = config.canary_until {
            seed_canary(&mut ledger, &config.relays[0], until)?;
        }
        if config.canary_until.is_none() && ledger.is_bounded_canary()? {
            return Err(RuntimeError::Config(
                "bounded canary ledger cannot become a rolling runtime",
            ));
        }
        let inventory = Arc::new(SyncStateDb::open(&config.inventory)?);
        let mut parent = ParentExecutor::new(ledger, inventory, dedupe, writer.clone())?;
        parent.configure(config.relays.clone()).await?;
        let (stop, stopping) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result =
                drive::<B>(&config, listener, parent, writer, stopping, reserve_bytes).await;
            gauge!("negentropy_isolated_ready").set(0.0);
            // A stopped runtime is not healthy merely because ingestion remains up.
            pensieve_core::reconciliation_health::reconciliation_health().fail();
            if let Err(ref error) = result {
                gauge!("negentropy_isolated_unhealthy").set(1.0);
                tracing::error!(error = %error, "isolated reconciliation paused; durable gaps retained");
            }
            // Remove only the socket created here; a replacement is not ours.
            drop(socket_cleanup);
            result
        });
        Ok(Self {
            stop: Some(stop),
            task,
        })
    }

    /// Stop intake, cancel an active exchange and join the ledger owner.
    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.await.map_err(|_| RuntimeError::Join)?
    }
}

struct SocketCleanup {
    path: PathBuf,
    inode: u64,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn drive<B: WorkerBinding>(
    config: &RuntimeConfig,
    listener: UnixListener,
    mut parent: ParentExecutor,
    writer: Arc<SegmentWriter>,
    stopping: oneshot::Receiver<()>,
    reserve_bytes: u64,
) -> Result<(), RuntimeError> {
    let stopping = stop_signal(
        stopping,
        config.canary_until.map(|_| Duration::from_secs(25 * 60)),
    );
    tokio::pin!(stopping);
    let mut interval = tokio::time::interval(TICK);
    let mut peer: Option<(UnixStream, B)> = None;
    let mut canary_attempts = 0;
    let mut canary_replays = 0;
    let mut failing_since = HashMap::new();
    let mut paused = false;
    let started = tokio::time::Instant::now();
    gauge!("negentropy_isolated_ready").set(1.0);
    // Present (as 0) from the start, not only after the first pause change.
    gauge!("negentropy_isolated_failure_pause").set(0.0);
    let result: Result<(), RuntimeError> = async {
    loop {
        tokio::select! {
            biased;
            _ = &mut stopping => break Ok(()),
            accepted = listener.accept(), if peer.is_none() => {
                let (socket, _) = accepted?;
                match B::inspect(&socket, config.worker_uid).await {
                    Ok(candidate) => peer = Some((socket, candidate)),
                    Err(error) => {
                        counter!("negentropy_worker_binding_rejections_total").increment(1);
                        tracing::warn!(error = %error, "rejected isolated worker identity");
                    }
                }
            }
            _ = interval.tick() => {
                if writer.recovery_required() {
                    gauge!("negentropy_isolated_recovery_required").set(1.0);
                    break Err(RuntimeError::Parent(ParentError::RecoveryRequired));
                }
                gauge!("negentropy_isolated_recovery_required").set(0.0);
                parent.expire(60).await?;
                let recovered = parent.maintain_archived(32, 256).await?;
                gauge!("negentropy_isolated_recovery_checked").set(recovered.checked as f64);
                let mut observation = parent.lifecycle().await?;
                observation.capacity_exhausted |= !data_has_headroom(&config.ledger, reserve_bytes)?;
                pensieve_core::reconciliation_health::reconciliation_health().observe(observation);
                if config.canary_until.is_some() && canary_session_budget(started.elapsed(), canary_attempts).is_none() {
                    // Drain already-admitted work through sealing/receipt maintenance.
                    // Do not replay, plan, lease or exit merely because intake is over.
                    continue;
                }
                if !data_has_headroom(&config.ledger, reserve_bytes)? {
                    gauge!("negentropy_isolated_ledger_space_pause").set(1.0);
                    continue;
                }
                gauge!("negentropy_isolated_ledger_space_pause").set(0.0);
                // Replay one sealed source segment before serving an assignment.
                if config.canary_until.is_none() || canary_replays < 3 {
                tokio::select! {
                    biased;
                    _ = &mut stopping => break Ok(()),
                    result = parent.replay_next(
                        config.archive.clone(), config.segment_prefix.clone(), config.replay_floor,
                    ) => {
                        if result? { canary_replays += 1; }
                    }
                }
                }
                if config.canary_until.is_none() {
                    let planned = parent.plan(32).await?;
                    gauge!("negentropy_isolated_planner_backpressured").set(f64::from(planned.backpressured));
                }
                // Replay and a worker session have separate durations. Refresh
                // owner facts after replay so their sum cannot age a healthy
                // observation past the watchdog before the session returns.
                let mut observation = parent.lifecycle().await?;
                observation.capacity_exhausted |= !data_has_headroom(&config.ledger, reserve_bytes)?;
                // Evaluated even without the pause so its gauges stay meaningful.
                let failing = parent.pause_failures().await?;
                let pause = failure_pause(
                    &mut failing_since,
                    &failing,
                    config.pause_on_persistent_failure,
                    tokio::time::Instant::now(),
                );
                pensieve_core::reconciliation_health::reconciliation_health().observe(observation);
                if pause != paused {
                    paused = pause;
                    gauge!("negentropy_isolated_failure_pause").set(f64::from(u8::from(paused)));
                    if paused {
                        tracing::error!("isolated reconciliation paused leasing: persistent failure outlasted the configured grace; gaps retained for operator decision");
                    } else {
                        tracing::info!("isolated reconciliation resumed leasing: persistent failure cleared");
                    }
                }
                if let Some((socket, candidate)) = peer.take() {
                    // Revalidate the same pidfd, invocation and remaining service
                    // lifetime immediately before exporting a lease capability.
                    match candidate.recheck().await {
                        Ok(_) => {}
                        Err(error) => {
                            counter!("negentropy_worker_binding_rejections_total").increment(1);
                            tracing::warn!(error = %error, "worker changed while idle");
                            continue;
                        }
                    }
                    let session_time = match config.canary_until {
                        Some(_) => match canary_session_budget(started.elapsed(), canary_attempts) {
                            Some(budget) => budget,
                            None => { peer = Some((socket, candidate)); continue; }
                        },
                        None => SESSION_TIME,
                    };
                    if paused {
                        peer = Some((socket, candidate));
                        continue;
                    }
                    let Some(lease) = parent.lease_next_fair(LEASE_SECS).await? else {
                        peer = Some((socket, candidate));
                        continue;
                    };
                    if config.canary_until.is_some() {
                        canary_attempts += 1;
                    }
                    let session_time = match candidate.recheck().await {
                        Ok(remaining) => session_time.min(remaining),
                        Err(error) => {
                            let (job, attempt) = (lease.job().id, lease.job().attempt);
                            tracing::warn!(error = %error, job, "worker changed before assignment");
                            attempt_local(parent.retry(lease, 60, RetryReason::WorkerLost).await, job, attempt, "retry")?;
                            continue;
                        }
                    };
                    tracing::info!(job = lease.job().id, session_seconds = session_time.as_secs(), "isolated worker session budget");
                    let job = lease.job().id;
                    let attempt = lease.job().attempt;
                    let outcome = tokio::select! {
                        biased;
                        _ = &mut stopping => break Ok(()),
                        outcome = parent.serve(socket, config.worker_uid, lease.clone(), session_time) => outcome,
                    };
                    // Keep the pidfd alive through the whole exchange, and never
                    // infer success from process exit, EOF or a transport ACK.
                    let _binding = candidate;
                    match outcome {
                        Ok(SessionOutcome::ProtocolDone) => {
                            counter!("negentropy_worker_protocol_done_total").increment(1);
                            parent.maintain_archived(32, 256).await?;
                        }
                        Ok(SessionOutcome::Failed(FailureKind::Volume)) => {
                            let split = attempt_local(parent.split(lease).await, job, attempt, "split")?;
                            if split == Some(SplitOutcome::Blocked) {
                                counter!("negentropy_dense_timestamp_blocked_total").increment(1);
                                tracing::error!(job, attempt, "isolated worker volume failure at one second");
                            }
                        }
                        Ok(SessionOutcome::Failed(kind)) => {
                            let delay = retry_delay(job, attempt);
                            // Bounded, sanitized diagnostic: never event IDs or relay text.
                            let missing = parent.failure_report(job, attempt).await.ok().flatten().map(|report| report.missing_count);
                            counter!("negentropy_worker_attempt_failed_total").increment(1);
                            tracing::warn!(job, attempt, relay = %lease.job().relay, kind = ?kind, missing_count = ?missing, retry_in_secs = delay, "isolated worker attempt failed; gap retained for retry");
                            attempt_local(parent.retry(lease, delay, RetryReason::RelayFailure).await, job, attempt, "retry")?;
                        }
                        Err(ParentError::TooDense) => {
                            let split = attempt_local(parent.split(lease).await, job, attempt, "split")?;
                            if split == Some(SplitOutcome::Blocked) {
                                counter!("negentropy_dense_timestamp_blocked_total").increment(1);
                                tracing::error!(job, attempt, "isolated inventory became too dense at one second");
                            }
                        }
                        Err(ParentError::RecoveryRequired) => {
                            break Err(RuntimeError::Parent(ParentError::RecoveryRequired));
                        }
                        Err(error) => {
                            tracing::warn!(job, attempt, error = %error, "isolated worker attempt incomplete");
                            // A lost result can race a committed terminal record.
                            // Reread the durable job before choosing any retry.
                            let current = parent.job(job).await?;
                            if current.attempt == attempt && current.state == super::jobs::JobState::Leased {
                                attempt_local(parent.retry(lease, retry_delay(job, attempt), RetryReason::WorkerLost).await, job, attempt, "retry")?;
                            }
                        }
                    }
                }
            }
        }
    }
    }.await;
    let shutdown = parent.shutdown().await;
    result.and(shutdown.map(|_| ()).map_err(Into::into))
}

/// Releasing an attempt whose lease is no longer active (it lapsed during an
/// uninterruptible disk operation, or the attempt already ended) concerns only
/// that attempt. The job, its receipts and its window stay durable; `expire`
/// recovers a lapsed lease on a later tick. Any other error stops the runtime.
fn attempt_local<T>(
    result: Result<T, ParentError>,
    job: i64,
    attempt: i64,
    action: &'static str,
) -> Result<Option<T>, RuntimeError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ParentError::Ledger(LedgerError::StaleLease)) => {
            counter!("negentropy_isolated_stale_release_total").increment(1);
            tracing::warn!(
                job,
                attempt,
                action,
                "isolated attempt lease no longer active; job retained for expiry recovery"
            );
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// Whether leasing should pause: some single job in `failing` (the ledger's
/// non-exempt persistent failures, sorted by ID) has been in it at every
/// observation for at least `grace`. A job that leaves the set loses its clock,
/// so separate failures never add up to one grace. Observations are taken
/// between sessions, never while the single worker holds a lease, so a retried
/// job is observed continuously. The start times are in memory only, so a
/// restart grants one more grace period of paced retries before pausing again.
fn failure_pause(
    since: &mut HashMap<i64, tokio::time::Instant>,
    failing: &[i64],
    grace: Option<Duration>,
    now: tokio::time::Instant,
) -> bool {
    since.retain(|job, _| failing.binary_search(job).is_ok());
    for &job in failing {
        since.entry(job).or_insert(now);
    }
    grace.is_some_and(|grace| {
        since
            .values()
            .any(|&started| now.saturating_duration_since(started) >= grace)
    })
}

fn canary_session_budget(elapsed: Duration, attempts: u32) -> Option<Duration> {
    let left = Duration::from_secs(20 * 60).checked_sub(elapsed)?;
    (attempts < 6 && !left.is_zero()).then_some(left.min(SESSION_TIME))
}

fn validate_canary(until: i64, relay_count: usize) -> Result<(), RuntimeError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| RuntimeError::Config("system clock precedes epoch"))?
        .as_secs();
    if relay_count != 1 || until < 2700 || until % 900 != 0 || until as u64 > now {
        return Err(RuntimeError::Config(
            "canary requires one relay and a past 15-minute boundary",
        ));
    }
    Ok(())
}

fn seed_canary(ledger: &mut JobLedger, relay: &str, until: i64) -> Result<(), RuntimeError> {
    for offset in 0..3 {
        let since = until - 2700 + offset * 900;
        ledger.enqueue("bounded-canary-v1", relay, since, since + 899)?;
    }
    Ok(())
}

async fn stop_signal(stopping: oneshot::Receiver<()>, limit: Option<Duration>) {
    let deadline = async {
        if let Some(limit) = limit {
            tokio::time::sleep(limit).await;
            tracing::info!("bounded canary deadline reached; unfinished jobs retained");
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = stopping => {},
        _ = deadline => {},
    }
}

fn retry_delay(job: i64, attempt: i64) -> u32 {
    let power = u32::try_from(attempt.saturating_sub(1)).unwrap_or(6).min(6);
    let base = 60u32.saturating_mul(1u32 << power).min(3600);
    let jitter = (job as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (attempt as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    base.saturating_add((jitter % 31) as u32).min(3600)
}

fn data_has_headroom(ledger: &Path, reserve_bytes: u64) -> Result<bool, RuntimeError> {
    let directory = ledger
        .parent()
        .ok_or(RuntimeError::Config("ledger has no parent"))?;
    let stats = nix::sys::statvfs::statvfs(directory)
        .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
    let available = available_bytes(stats.blocks_available(), stats.fragment_size());
    Ok(available >= reserve_bytes)
}

fn available_bytes<B: Into<u64>>(blocks: B, fragment_size: u64) -> u64 {
    blocks.into().saturating_mul(fragment_size)
}

#[cfg(test)]
mod canary_tests {
    use super::*;
    use crate::sync::failure::FailureDiagnostic;
    use crate::sync::jobs::JobState;

    #[test]
    fn canary_rejects_unsafe_boundaries_and_multiple_relays() {
        assert!(validate_canary(3600, 1).is_ok());
        for (until, relays) in [(0, 1), (3599, 1), (3600, 2), (i64::MAX, 1)] {
            assert!(validate_canary(until, relays).is_err());
        }
    }

    #[test]
    fn canary_seeds_exactly_three_disjoint_windows_idempotently() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let mut ledger = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        seed_canary(&mut ledger, "wss://a.example", 3600).unwrap();
        seed_canary(&mut ledger, "wss://a.example", 3600).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        let mut query = db
            .prepare("SELECT since,until FROM jobs ORDER BY since")
            .unwrap();
        let rows = query
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows, vec![(900, 1799), (1800, 2699), (2700, 3599)]);
        drop(ledger);
        let mut reopened = JobLedger::open(&path, LedgerLimits::default()).unwrap();
        assert!(reopened.is_bounded_canary().unwrap());
        reopened.configure_planner(&["wss://a.example"]).unwrap();
        assert!(reopened.plan_rolling(7200, 32).is_err());
    }

    #[test]
    fn persistent_failure_pauses_only_after_one_jobs_continuous_grace() {
        let start = tokio::time::Instant::now();
        let hour = Duration::from_secs(3600);
        let minutes = |m: u64| start + Duration::from_secs(60 * m);
        // Disabled: never pauses, however long the failure lasts.
        let mut since = HashMap::new();
        assert!(!failure_pause(&mut since, &[1], None, start));
        assert!(!failure_pause(&mut since, &[1], None, start + 10 * hour));
        let mut since = HashMap::new();
        assert!(!failure_pause(&mut since, &[1], Some(hour), start));
        assert!(!failure_pause(&mut since, &[1], Some(hour), minutes(30)));
        assert!(failure_pause(&mut since, &[1], Some(hour), minutes(60)));
        // A cleared failure resets that job's grace; a new failure starts it again.
        assert!(!failure_pause(&mut since, &[], Some(hour), minutes(120)));
        assert!(!failure_pause(&mut since, &[1], Some(hour), minutes(180)));
        assert!(failure_pause(&mut since, &[1], Some(hour), minutes(240)));
        // Overlapping failures of different jobs never add up to one grace:
        // job 1 fails 0-50 min, job 2 30-80 min, job 3 from 70 min.
        let mut since = HashMap::new();
        for (at, failing) in [
            (0, &[1][..]),
            (30, &[1, 2]),
            (50, &[2]),
            (70, &[2, 3]),
            (80, &[3]),
            (129, &[3]),
        ] {
            assert!(
                !failure_pause(&mut since, failing, Some(hour), minutes(at)),
                "{at}"
            );
        }
        assert!(failure_pause(&mut since, &[3], Some(hour), minutes(130)));
        // Zero grace pauses as soon as a counted failure is observed.
        let mut since = HashMap::new();
        assert!(!failure_pause(&mut since, &[], Some(Duration::ZERO), start));
        assert!(failure_pause(&mut since, &[7], Some(Duration::ZERO), start));
    }

    /// The single global clock of #90, kept to show the counter-examples it
    /// paused on: it started when any job became a persistent failure and reset
    /// only when none was.
    fn legacy_failure_pause(
        since: &mut Option<tokio::time::Instant>,
        failing: bool,
        grace: Duration,
        now: tokio::time::Instant,
    ) -> bool {
        if !failing {
            *since = None;
            return false;
        }
        now.saturating_duration_since(*since.get_or_insert(now)) >= grace
    }

    #[derive(Debug, Default)]
    struct Simulation {
        /// `(job, attempt, seconds after start)` of each relay-unavailable exclusion.
        excluded: Vec<(i64, i64, i64)>,
        /// When the per-job, exemption-aware pause fired.
        paused: Option<i64>,
        /// When the global clock of #90 would have paused.
        legacy_paused: Option<i64>,
        /// `(job, attempt, seconds after start)` of each carried exclusion.
        carried: Vec<(i64, i64, i64)>,
        /// Seconds after start of each observation with health red
        /// (`unresolved_failures`: some job is a persistent failure).
        red: Vec<i64>,
    }

    const GRACE: Duration = Duration::from_secs(7200);
    const X: [u8; 32] = [8; 32];
    const Y: [u8; 32] = [9; 32];

    fn withheld(ids: &[[u8; 32]]) -> Option<FailureDiagnostic> {
        Some(FailureDiagnostic {
            kind: FailureKind::Unavailable,
            missing_count: ids.len() as u64,
            sample: ids.to_vec(),
        })
    }

    fn relay_down() -> Option<FailureDiagnostic> {
        Some(FailureDiagnostic {
            kind: FailureKind::Relay,
            missing_count: 0,
            sample: Vec::new(),
        })
    }

    /// Drive the runtime's tick order against a real ledger with a virtual clock:
    /// the exclusion turn and health observation precede the pause check and the
    /// lease, as in `drive`. A session blocks the loop for `session` seconds and
    /// ends with `report(job_index, attempt)` (or, with `None`, a lost worker),
    /// released at the runtime's backoff. Job `n` is enqueued `enqueue[n]`
    /// seconds after the start, for its own 15-minute window. Stops at the
    /// pause, once every job is complete, or after `horizon` seconds.
    fn simulate(
        enqueue: &[i64],
        session: i64,
        horizon: i64,
        report: impl Fn(usize, i64) -> Option<FailureDiagnostic>,
    ) -> Simulation {
        let jobs: Vec<_> = (0..).zip(enqueue).map(|(n, &at)| (at, n)).collect();
        simulate_windows(&jobs, session, horizon, report)
    }

    /// [`simulate`] where job `n` is `(enqueued seconds after start, window
    /// index)`; jobs sharing a window index revisit it, as a later planner
    /// pass over history does.
    fn simulate_windows(
        enqueue: &[(i64, i64)],
        session: i64,
        horizon: i64,
        report: impl Fn(usize, i64) -> Option<FailureDiagnostic>,
    ) -> Simulation {
        let root = tempfile::tempdir().unwrap();
        let mut ledger =
            JobLedger::open(&root.path().join("jobs.sqlite"), LedgerLimits::default()).unwrap();
        ledger
            .set_relay_unavailable_rule(Some(UnavailableRule::default()))
            .unwrap();
        let dedupe = DedupeIndex::open(root.path().join("dedupe")).unwrap();
        let origin = tokio::time::Instant::now();
        let start = 10_000_i64;
        let mut jobs = Vec::new();
        let mut since = HashMap::new();
        let mut legacy = None;
        let mut sim = Simulation::default();
        let mut now = start;
        while now < start + horizon {
            while jobs.len() < enqueue.len() && now >= start + enqueue[jobs.len()].0 {
                let (n, window) = (jobs.len(), enqueue[jobs.len()].1);
                let sweep = format!("pass-{n}");
                let job =
                    ledger.enqueue(&sweep, "wss://a.example", window * 900, window * 900 + 899);
                jobs.push(job.unwrap().id);
            }
            let progress = ledger.exclude_relay_unavailable(now, &dedupe).unwrap();
            for excluded in progress.excluded {
                let attempt = ledger.get(excluded.job).unwrap().attempt;
                sim.excluded.push((excluded.job, attempt, now - start));
            }
            for carried in progress.carried {
                let attempt = ledger.get(carried.job).unwrap().attempt;
                sim.carried.push((carried.job, attempt, now - start));
            }
            if jobs.len() == enqueue.len()
                && jobs
                    .iter()
                    .all(|&id| ledger.get(id).unwrap().state == JobState::Complete)
            {
                break;
            }
            let at = origin + Duration::from_secs((now - start) as u64);
            let observation = ledger.progress_observation(now).unwrap();
            if observation.persistent_failure {
                sim.red.push(now - start);
            }
            if legacy_failure_pause(&mut legacy, observation.persistent_failure, GRACE, at)
                && sim.legacy_paused.is_none()
            {
                sim.legacy_paused = Some(now - start);
            }
            let failing = ledger.pause_failures().unwrap();
            if failure_pause(&mut since, &failing, Some(GRACE), at) {
                sim.paused = Some(now - start);
                break;
            }
            if let Some(lease) = ledger.lease_next(now, LEASE_SECS).unwrap() {
                let (job, attempt) = (lease.job().id, lease.job().attempt);
                let index = jobs.iter().position(|&id| id == job).unwrap();
                now += session;
                let reason = match report(index, attempt) {
                    Some(report) => {
                        ledger
                            .record_failure_report(&lease, 1, &report, now)
                            .unwrap();
                        RetryReason::RelayFailure
                    }
                    None => RetryReason::WorkerLost,
                };
                ledger
                    .retry(&lease, now, retry_delay(job, attempt), reason)
                    .unwrap();
            }
            now += TICK.as_secs() as i64;
        }
        sim
    }

    #[test]
    fn withheld_window_is_excluded_without_pausing() {
        // Identical reports from the first attempt: excluded at attempt 7 for
        // short sessions, at attempt 6 for nine-minute sessions.
        for (session, attempt) in [(30, 7), (540, 6)] {
            let sim = simulate(&[0], session, 12 * 3600, |_, _| withheld(&[X]));
            assert_eq!(sim.paused, None, "session {session}");
            assert_eq!(sim.excluded.len(), 1);
            assert_eq!(sim.excluded[0].1, attempt, "session {session}");
        }
    }

    #[test]
    fn a_revisited_withheld_window_is_carried_without_health_red_or_pause() {
        // Production 2026-10-13: job 768 was excluded, then a later pass over
        // history enqueued the same window as job 1984, which withheld the same
        // ID. Before carry-forward 1984 had to re-earn the exclusion (five
        // reports over an hour) and held health red from its third failure.
        const REVISIT: i64 = 12 * 3600;
        for session in [30, 540] {
            let sim = simulate_windows(&[(0, 0), (REVISIT, 0)], session, 24 * 3600, |_, _| {
                withheld(&[X])
            });
            assert_eq!(sim.paused, None, "session {session}");
            assert_eq!(sim.excluded.len(), 1, "{sim:?}");
            // The revisit completes on the lifecycle turn after its first
            // failure, long before its 60 s retry is due.
            assert_eq!(sim.carried.len(), 1, "{sim:?}");
            let (job, attempt, at) = sim.carried[0];
            assert_eq!((job, attempt), (2, 1), "{sim:?}");
            assert!(at < REVISIT + session + 60, "{sim:?}");
            // Health was red only while the original earned its exclusion.
            let original_excluded = sim.excluded[0].2;
            assert!(!sim.red.is_empty(), "{sim:?}");
            assert!(
                sim.red.iter().all(|&red| red < original_excluded),
                "{sim:?}"
            );
        }
        // A different window is not carried: it earns its own exclusion.
        let sim = simulate_windows(&[(0, 0), (REVISIT, 1)], 30, 24 * 3600, |_, _| {
            withheld(&[X])
        });
        assert!(sim.carried.is_empty(), "{sim:?}");
        assert_eq!(sim.excluded.len(), 2, "{sim:?}");
        assert!(sim.red.iter().any(|&red| red > REVISIT), "{sim:?}");
    }

    #[test]
    fn counter_example_a_interrupted_run_no_longer_pauses_before_exclusion() {
        // A relay (or deadline) report on attempt 3 restarts the identical run at
        // attempt 4, so the fifth identical report is attempt 8, more than 7200 s
        // after the third failure. The global clock paused first, at attempt 7.
        let script = |_: usize, attempt: i64| {
            if attempt == 3 {
                relay_down()
            } else {
                withheld(&[X])
            }
        };
        let sim = simulate(&[0], 30, 12 * 3600, script);
        let (_, attempt, excluded_at) = sim.excluded[0];
        assert_eq!(attempt, 8);
        assert!(sim.legacy_paused.unwrap() < excluded_at, "{sim:?}");
        assert_eq!(sim.paused, None);
    }

    #[test]
    fn counter_example_b_overlapping_withheld_windows_no_longer_pause() {
        // Two withheld windows an hour apart: the second becomes a persistent
        // failure just before the first is excluded, so the global clock never
        // reset and paused before the second exclusion.
        let sim = simulate(&[0, 3600], 30, 12 * 3600, |_, _| withheld(&[X]));
        assert_eq!(sim.excluded.len(), 2, "{sim:?}");
        assert!(sim.legacy_paused.unwrap() < sim.excluded[1].2, "{sim:?}");
        assert_eq!(sim.paused, None);
    }

    #[test]
    fn a_relay_that_stays_down_still_pauses_after_the_grace() {
        let sim = simulate(&[0], 30, 12 * 3600, |_, _| relay_down());
        assert!(sim.excluded.is_empty());
        let paused = sim.paused.unwrap();
        // The clock starts at the third failure, a few minutes in.
        assert!((7200..7200 + 600).contains(&paused), "{paused}");
        // A withheld window alongside a genuine failure: the genuine one still
        // pauses a grace after its own third failure.
        let sim = simulate(&[0, 600], 30, 12 * 3600, |index, _| {
            if index == 0 {
                withheld(&[X])
            } else {
                relay_down()
            }
        });
        assert_eq!(sim.excluded.len(), 1);
        assert!(sim.paused.unwrap() >= 600 + 7200, "{sim:?}");
    }

    #[test]
    fn a_shrinking_withheld_set_is_excluded_without_pausing() {
        // The relay withholds {X,Y}, then starts delivering Y at attempt
        // `shrink`. The {X} run starts there and still completes by attempt 10;
        // the earlier, larger set must not count the job toward the pause.
        for shrink in [4, 5, 6] {
            let sim = simulate(&[0], 30, 24 * 3600, |_, attempt| {
                withheld(if attempt < shrink { &[X, Y] } else { &[X] })
            });
            assert_eq!(sim.paused, None, "shrink at {shrink}: {sim:?}");
            assert_eq!(sim.excluded.len(), 1, "shrink at {shrink}: {sim:?}");
            let attempt = sim.excluded[0].1;
            assert!(
                (shrink + 4..=exemption_ceiling_for_default()).contains(&attempt),
                "shrink at {shrink}: {sim:?}"
            );
        }
    }

    fn exemption_ceiling_for_default() -> i64 {
        2 * i64::from(UnavailableRule::default().attempts)
    }

    #[test]
    fn exemption_is_bounded_for_runs_that_cannot_qualify() {
        // A set that keeps changing restarts its run every attempt and never
        // qualifies: exempt through attempt 10, then counted, so the pause
        // fires a grace later instead of never.
        let sim = simulate(&[0], 30, 24 * 3600, |_, attempt| {
            withheld(if attempt % 2 == 0 { &[X] } else { &[Y] })
        });
        assert!(sim.excluded.is_empty());
        let paused = sim.paused.unwrap();
        assert!(paused > 7200 + 600, "{sim:?}");
        assert!(paused < 12 * 3600, "{sim:?}");
        // One report followed only by lost workers: exempt through attempt 10,
        // then counted, so the pause fires a grace later instead of never.
        let sim = simulate(&[0], 30, 24 * 3600, |_, attempt| {
            if attempt == 1 { withheld(&[X]) } else { None }
        });
        assert!(sim.excluded.is_empty());
        let paused = sim.paused.unwrap();
        assert!(paused > 7200 + 600, "{sim:?}");
        assert!(paused < 12 * 3600, "{sim:?}");
    }

    #[test]
    fn only_a_lapsed_lease_release_is_attempt_local() {
        assert_eq!(attempt_local(Ok(7), 1, 1, "retry").unwrap(), Some(7));
        assert_eq!(
            attempt_local::<()>(
                Err(ParentError::Ledger(LedgerError::StaleLease)),
                1,
                1,
                "retry"
            )
            .unwrap(),
            None
        );
        for error in [
            ParentError::InvalidRequest,
            ParentError::Ledger(LedgerError::Budget),
            ParentError::Ledger(LedgerError::Invalid("corrupt")),
            ParentError::Stopped,
        ] {
            assert!(attempt_local::<()>(Err(error), 1, 1, "retry").is_err());
        }
    }

    #[test]
    fn exhausted_intake_budget_leaves_time_for_durability_drain() {
        assert_eq!(canary_session_budget(Duration::ZERO, 0), Some(SESSION_TIME));
        assert_eq!(
            canary_session_budget(Duration::from_secs(1190), 5),
            Some(Duration::from_secs(10))
        );
        assert_eq!(canary_session_budget(Duration::ZERO, 6), None);
        assert_eq!(canary_session_budget(Duration::from_secs(1200), 0), None);
        assert_eq!(canary_session_budget(Duration::from_secs(1501), 0), None);
    }

    #[tokio::test]
    async fn canary_deadline_stops_without_parent_shutdown_signal() {
        let (_sender, receiver) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(1),
            stop_signal(receiver, Some(Duration::from_millis(20))),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn normal_runtime_still_requires_shutdown_signal() {
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(stop_signal(receiver, None));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        sender.send(()).unwrap();
        task.await.unwrap();
    }
}

/// Runtime loop tests. They run on every platform, CI's Linux runners
/// included, as an unprivileged user: the systemd/cgroup/pidfd worker proof is
/// replaced by [`SameUidPeer`] and the 20 GiB data reserve by a small one. The
/// peer-UID check, the parent's own `serve` UID check and everything after
/// identity are the production code.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::SegmentConfig;
    use crate::sync::binding::SameUidPeer;
    use crate::sync::ipc::{Frame, Message, initial_digest};
    use crate::sync::jobs::JobState;
    use crate::sync::worker::{Hello, ParentMessage, read_message, write_message};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TEST_RESERVE_BYTES: u64 = 1024 * 1024;

    async fn start(
        config: RuntimeConfig,
        dedupe: Arc<DedupeIndex>,
        writer: Arc<SegmentWriter>,
    ) -> Result<IsolatedRuntime, RuntimeError> {
        IsolatedRuntime::start_bound::<SameUidPeer>(config, dedupe, writer, TEST_RESERVE_BYTES)
            .await
    }

    /// The test process's UID, used as the worker UID so a same-process peer
    /// passes the UID checks. The runtime refuses worker UID 0, so these tests
    /// need an unprivileged user (as on CI).
    fn worker_uid() -> u32 {
        let uid = nix::unistd::getuid().as_raw();
        assert_ne!(uid, 0, "runtime tests need a non-root user");
        uid
    }

    #[tokio::test]
    async fn occupied_socket_is_preserved_and_reconciliation_stays_closed() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("worker.sock");
        let active = UnixListener::bind(&socket).unwrap();
        let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
        let archive = root.path().join("archive");
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let config = RuntimeConfig {
            canary_until: Some(3600),
            admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
            socket: socket.clone(),
            worker_uid: 501,
            ledger: root.path().join("jobs.sqlite"),
            inventory: root.path().join("inventory"),
            archive,
            segment_prefix: "segment".to_owned(),
            replay_floor: 0,
            relays: vec!["wss://a.example".to_owned()],
            pause_on_persistent_failure: None,
            relay_unavailable_rule: Some(UnavailableRule::default()),
        };
        assert!(matches!(
            start(config, dedupe, writer).await,
            Err(RuntimeError::SocketOccupied(path)) if path == socket
        ));
        let _peer = UnixStream::connect(&socket).await.unwrap();
        let (_accepted, _) = active.accept().await.unwrap();
        assert!(!root.path().join("jobs.sqlite").exists());
    }

    #[tokio::test]
    async fn production_binding_rejects_a_same_uid_peer_outside_the_worker_service() {
        // The public entry point binds with the real proof. A same-UID peer that
        // is not the systemd worker (here: the test process itself) is dropped
        // before any assignment, on Linux and elsewhere.
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("archive");
        let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let config = RuntimeConfig {
            canary_until: None,
            admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
            socket: root.path().join("worker.sock"),
            worker_uid: worker_uid(),
            ledger: root.path().join("jobs.sqlite"),
            inventory: root.path().join("inventory"),
            archive,
            segment_prefix: "segment".to_owned(),
            replay_floor: 0,
            relays: vec!["wss://a.example".to_owned()],
            pause_on_persistent_failure: None,
            relay_unavailable_rule: Some(UnavailableRule::default()),
        };
        let runtime = IsolatedRuntime::start(config.clone(), dedupe, writer)
            .await
            .unwrap();
        let mut worker = UnixStream::connect(&config.socket).await.unwrap();
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(10), worker.read(&mut byte))
            .await
            .expect("rejected peer is disconnected");
        assert!(matches!(read, Ok(0) | Err(_)), "no assignment: {read:?}");
        drop(worker);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_canary_never_runs_rolling_planning_or_reuses_ledger() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("archive");
        let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let config = RuntimeConfig {
            admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
            socket: root.path().join("worker.sock"),
            worker_uid: 501,
            ledger: root.path().join("jobs.sqlite"),
            inventory: root.path().join("inventory"),
            archive,
            segment_prefix: "segment".to_owned(),
            replay_floor: 0,
            relays: vec!["wss://a.example".to_owned()],
            canary_until: Some(3600),
            pause_on_persistent_failure: None,
            relay_unavailable_rule: Some(UnavailableRule::default()),
        };
        let runtime = start(config.clone(), dedupe.clone(), writer.clone())
            .await
            .unwrap();
        // Two real planning ticks would normally seed dozens of rolling jobs.
        tokio::time::sleep(Duration::from_secs(6)).await;
        runtime.shutdown().await.unwrap();
        assert!(!config.socket.exists());
        let db = rusqlite::Connection::open(&config.ledger).unwrap();
        let count: u32 = db
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let queued: u32 = db
            .query_row("SELECT count(*) FROM jobs WHERE state='queued'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(queued, 3);
        assert!(matches!(
            start(config.clone(), dedupe.clone(), writer.clone()).await,
            Err(RuntimeError::Config(_))
        ));
        let mut rolling = config;
        rolling.canary_until = None;
        assert!(matches!(
            start(rolling, dedupe, writer).await,
            Err(RuntimeError::Config(_))
        ));
        let retained: u32 = db
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(retained, 3);
    }

    #[tokio::test]
    async fn opt_in_listener_reaches_durable_zero_event_completion() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("archive");
        let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let uid = worker_uid();
        let ledger_path = root.path().join("jobs.sqlite");
        let config = RuntimeConfig {
            canary_until: None,
            admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
            socket: root.path().join("worker.sock"),
            worker_uid: uid,
            ledger: ledger_path.clone(),
            inventory: root.path().join("inventory"),
            archive,
            segment_prefix: "segment".to_owned(),
            replay_floor: 0,
            relays: vec!["wss://a.example".to_owned()],
            pause_on_persistent_failure: None,
            relay_unavailable_rule: Some(UnavailableRule::default()),
        };
        let runtime = start(config.clone(), dedupe, writer).await.unwrap();
        let mut worker = UnixStream::connect(&config.socket).await.unwrap();
        write_message(
            &mut worker,
            &Hello {
                version: super::super::ipc::VERSION,
            },
        )
        .await
        .unwrap();
        let job = tokio::time::timeout(Duration::from_secs(15), async {
            let ParentMessage::Job { assignment } = read_message(&mut worker).await.unwrap() else {
                panic!("expected assignment");
            };
            assignment
        })
        .await
        .unwrap();
        let ParentMessage::InventoryEnd { count, .. } = read_message(&mut worker).await.unwrap()
        else {
            panic!("expected inventory end");
        };
        assert_eq!(count, 0);
        let done = Message::ProtocolDone {
            header: job.header(1),
            count: 0,
            digest: initial_digest(),
        };
        worker
            .write_all(&Frame::encode(&done).unwrap())
            .await
            .unwrap();
        let mut eof = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(5), worker.read(&mut eof))
            .await
            .unwrap()
            .unwrap();
        drop(worker);
        let mut lost_worker = UnixStream::connect(&config.socket).await.unwrap();
        write_message(
            &mut lost_worker,
            &Hello {
                version: super::super::ipc::VERSION,
            },
        )
        .await
        .unwrap();
        let lost = tokio::time::timeout(Duration::from_secs(15), async {
            let ParentMessage::Job { assignment } = read_message(&mut lost_worker).await.unwrap()
            else {
                panic!("expected second assignment");
            };
            assignment
        })
        .await
        .unwrap();
        let ParentMessage::InventoryEnd { .. } = read_message(&mut lost_worker).await.unwrap()
        else {
            panic!("expected second inventory end");
        };
        drop(lost_worker); // EOF is not ProtocolDone or durable success.
        tokio::time::sleep(Duration::from_millis(200)).await;
        runtime.shutdown().await.unwrap();
        let ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
        assert_eq!(
            ledger.get(job.header(0).job).unwrap().state,
            JobState::Complete
        );
        assert_eq!(
            ledger.get(lost.header(0).job).unwrap().state,
            JobState::RetryWait
        );
    }

    #[tokio::test]
    async fn persistent_failure_pause_withholds_leases_until_restarted_without_it() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("archive");
        let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let uid = worker_uid();
        let ledger_path = root.path().join("jobs.sqlite");
        // A due gap after three relay failures: health reports unresolved_failures.
        let gap = {
            let mut ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
            let id = ledger
                .enqueue("manual", "wss://a.example", 0, 899)
                .unwrap()
                .id;
            for at in [1_000, 2_000, 3_000] {
                let lease = ledger.lease_next(at, 60).unwrap().unwrap();
                ledger
                    .retry(&lease, at, 60, RetryReason::RelayFailure)
                    .unwrap();
            }
            id
        };
        let mut config = RuntimeConfig {
            canary_until: None,
            admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
            socket: root.path().join("worker.sock"),
            worker_uid: uid,
            ledger: ledger_path.clone(),
            inventory: root.path().join("inventory"),
            archive,
            segment_prefix: "segment".to_owned(),
            replay_floor: 0,
            relays: vec!["wss://a.example".to_owned()],
            pause_on_persistent_failure: Some(Duration::ZERO),
            relay_unavailable_rule: Some(UnavailableRule::default()),
        };
        let hello = || Hello {
            version: super::super::ipc::VERSION,
        };
        let runtime = start(config.clone(), dedupe.clone(), writer.clone())
            .await
            .unwrap();
        let mut worker = UnixStream::connect(&config.socket).await.unwrap();
        write_message(&mut worker, &hello()).await.unwrap();
        // Two ticks pass without an assignment; the runtime keeps running.
        assert!(
            tokio::time::timeout(
                Duration::from_secs(11),
                read_message::<_, ParentMessage>(&mut worker)
            )
            .await
            .is_err()
        );
        drop(worker);
        runtime.shutdown().await.unwrap();
        let ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
        let job = ledger.get(gap).unwrap();
        assert_eq!((job.state, job.attempt), (JobState::RetryWait, 3));
        drop(ledger);
        let attempt_rows: u32 = rusqlite::Connection::open(&ledger_path)
            .unwrap()
            .query_row("SELECT count(*) FROM attempts", [], |r| r.get(0))
            .unwrap();
        let attempts: i64 = rusqlite::Connection::open(&ledger_path)
            .unwrap()
            .query_row("SELECT coalesce(sum(attempt),0) FROM jobs", [], |r| {
                r.get(0)
            })
            .unwrap();
        // No planned fresh root or the gap consumed an attempt while paused.
        assert_eq!((attempt_rows, attempts), (0, 3));
        // Without the pause the same ledger resumes paced retries.
        config.pause_on_persistent_failure = None;
        let runtime = start(config.clone(), dedupe, writer).await.unwrap();
        let mut worker = UnixStream::connect(&config.socket).await.unwrap();
        write_message(&mut worker, &hello()).await.unwrap();
        let ParentMessage::Job { .. } = tokio::time::timeout(
            Duration::from_secs(15),
            read_message::<_, ParentMessage>(&mut worker),
        )
        .await
        .unwrap()
        .unwrap() else {
            panic!("expected assignment");
        };
        drop(worker);
        tokio::time::sleep(Duration::from_millis(200)).await;
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn withheld_windows_neither_pause_nor_stall_the_real_runtime_loop() {
        use crate::sync::failure::FailureDiagnostic;
        // Five identical hourly reports meet the rule: the first tick's lifecycle
        // turn excludes the window. Three are an in-progress run: exempt. Either
        // way a zero-grace pause must not withhold the next lease.
        for reports in [5, 3] {
            let root = tempfile::tempdir().unwrap();
            let archive = root.path().join("archive");
            let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
            let writer = Arc::new(
                SegmentWriter::new(
                    SegmentConfig {
                        output_dir: archive.clone(),
                        compress: false,
                        ..SegmentConfig::default()
                    },
                    None,
                    Some(dedupe.clone()),
                )
                .unwrap(),
            );
            let uid = worker_uid();
            let ledger_path = root.path().join("jobs.sqlite");
            let gap = {
                let mut ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
                let id = ledger
                    .enqueue("manual", "wss://a.example", 0, 899)
                    .unwrap()
                    .id;
                for n in 0..reports {
                    let at = 1_000 + n * 3600;
                    let lease = ledger.lease_next(at, 600).unwrap().unwrap();
                    let report = FailureDiagnostic {
                        kind: FailureKind::Unavailable,
                        missing_count: 1,
                        sample: vec![[8; 32]],
                    };
                    ledger
                        .record_failure_report(&lease, 1, &report, at + 1)
                        .unwrap();
                    ledger
                        .retry(&lease, at + 1, 60, RetryReason::RelayFailure)
                        .unwrap();
                }
                assert!(
                    ledger
                        .progress_observation(30_000)
                        .unwrap()
                        .persistent_failure
                );
                id
            };
            let config = RuntimeConfig {
                canary_until: None,
                admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
                socket: root.path().join("worker.sock"),
                worker_uid: uid,
                ledger: ledger_path.clone(),
                inventory: root.path().join("inventory"),
                archive,
                segment_prefix: "segment".to_owned(),
                replay_floor: 0,
                relays: vec!["wss://a.example".to_owned()],
                pause_on_persistent_failure: Some(Duration::ZERO),
                relay_unavailable_rule: Some(UnavailableRule::default()),
            };
            let runtime = start(config.clone(), dedupe, writer).await.unwrap();
            let mut worker = UnixStream::connect(&config.socket).await.unwrap();
            write_message(
                &mut worker,
                &Hello {
                    version: super::super::ipc::VERSION,
                },
            )
            .await
            .unwrap();
            let ParentMessage::Job { .. } = tokio::time::timeout(
                Duration::from_secs(15),
                read_message::<_, ParentMessage>(&mut worker),
            )
            .await
            .unwrap()
            .unwrap() else {
                panic!("expected assignment");
            };
            drop(worker);
            tokio::time::sleep(Duration::from_millis(200)).await;
            runtime.shutdown().await.unwrap();
            let ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
            let job = ledger.get(gap).unwrap();
            let exclusions = ledger.relay_unavailable_exclusions(gap).unwrap();
            if reports == 5 {
                assert_eq!(job.state, JobState::Complete);
                assert_eq!(
                    job.reason.as_deref(),
                    Some(super::super::jobs::RELAY_UNAVAILABLE)
                );
                assert_eq!(exclusions, vec![([8; 32], None)]);
            } else {
                assert_ne!(job.state, JobState::Complete);
                assert!(exclusions.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn a_revisited_withheld_window_is_carried_by_the_real_runtime_loop() {
        use crate::sync::failure::FailureDiagnostic;
        // Job 768 / 1984 shape: an excluded window, then a later pass's job for
        // the same window that failed once withholding the same ID. The first
        // tick's lifecycle turn completes it before any lease, and a zero-grace
        // pause never fires.
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("archive");
        let dedupe = Arc::new(DedupeIndex::open(root.path().join("dedupe")).unwrap());
        let writer = Arc::new(
            SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap(),
        );
        let ledger_path = root.path().join("jobs.sqlite");
        let report = FailureDiagnostic {
            kind: FailureKind::Unavailable,
            missing_count: 1,
            sample: vec![[8; 32]],
        };
        let (original, revisit) = {
            let mut ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
            let original = ledger
                .enqueue("pass-1", "wss://a.example", 0, 899)
                .unwrap()
                .id;
            let fail = |ledger: &mut JobLedger, at: i64| {
                let lease = ledger.lease_next(at, 600).unwrap().unwrap();
                ledger
                    .record_failure_report(&lease, 1, &report, at + 1)
                    .unwrap();
                ledger
                    .retry(&lease, at + 1, 60, RetryReason::RelayFailure)
                    .unwrap();
                lease.job().id
            };
            for n in 0..5 {
                assert_eq!(fail(&mut ledger, 1_000 + n * 3600), original);
            }
            let progress = ledger.exclude_relay_unavailable(20_000, &dedupe).unwrap();
            assert_eq!(progress.excluded[0].job, original);
            let revisit = ledger
                .enqueue("pass-2", "wss://a.example", 0, 899)
                .unwrap()
                .id;
            assert_eq!(fail(&mut ledger, 30_000), revisit);
            (original, revisit)
        };
        let config = RuntimeConfig {
            canary_until: None,
            admission_policy: crate::giftwrap_filter::AdmissionPolicy::default(),
            socket: root.path().join("worker.sock"),
            worker_uid: worker_uid(),
            ledger: ledger_path.clone(),
            inventory: root.path().join("inventory"),
            archive,
            segment_prefix: "segment".to_owned(),
            replay_floor: 0,
            relays: vec!["wss://a.example".to_owned()],
            pause_on_persistent_failure: Some(Duration::ZERO),
            relay_unavailable_rule: Some(UnavailableRule::default()),
        };
        let runtime = start(config.clone(), dedupe, writer).await.unwrap();
        let mut worker = UnixStream::connect(&config.socket).await.unwrap();
        write_message(
            &mut worker,
            &Hello {
                version: super::super::ipc::VERSION,
            },
        )
        .await
        .unwrap();
        let ParentMessage::Job { assignment } = tokio::time::timeout(
            Duration::from_secs(15),
            read_message::<_, ParentMessage>(&mut worker),
        )
        .await
        .unwrap()
        .unwrap() else {
            panic!("expected assignment");
        };
        // The revisit was completed, not leased again.
        assert_ne!(assignment.header(0).job, revisit);
        drop(worker);
        tokio::time::sleep(Duration::from_millis(200)).await;
        runtime.shutdown().await.unwrap();
        let ledger = JobLedger::open(&ledger_path, LedgerLimits::default()).unwrap();
        let job = ledger.get(revisit).unwrap();
        assert_eq!((job.state, job.attempt), (JobState::Complete, 1));
        assert_eq!(
            job.reason.as_deref(),
            Some(super::super::jobs::RELAY_UNAVAILABLE)
        );
        assert_eq!(
            ledger.relay_unavailable_exclusions(revisit).unwrap(),
            vec![([8; 32], None)]
        );
        let original_row = ledger.relay_unavailable_carried_from(revisit).unwrap()[0];
        assert!(original_row.is_some());
        assert_eq!(
            ledger.relay_unavailable_carried_from(original).unwrap(),
            vec![None]
        );
    }
}
