//! Explicitly enabled, single-worker isolated reconciliation runtime.
//!
//! The ingester owns the listener, ledger, inventory and archive. It never
//! launches or restarts the static systemd worker. All unfinished jobs survive
//! worker loss, shutdown and local faults; a socket exchange is not completion.

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
use super::binding::{BindingError, WorkerCandidate};
use super::failure::FailureKind;
use super::jobs::{JobLedger, LedgerError, LedgerLimits, RetryReason, SplitOutcome};
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
            let result = drive(&config, listener, parent, writer, stopping).await;
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

async fn drive(
    config: &RuntimeConfig,
    listener: UnixListener,
    mut parent: ParentExecutor,
    writer: Arc<SegmentWriter>,
    stopping: oneshot::Receiver<()>,
) -> Result<(), RuntimeError> {
    let stopping = stop_signal(
        stopping,
        config.canary_until.map(|_| Duration::from_secs(25 * 60)),
    );
    tokio::pin!(stopping);
    let mut interval = tokio::time::interval(TICK);
    let mut peer: Option<(UnixStream, WorkerCandidate)> = None;
    let mut canary_attempts = 0;
    let mut canary_replays = 0;
    let started = tokio::time::Instant::now();
    gauge!("negentropy_isolated_ready").set(1.0);
    let result: Result<(), RuntimeError> = async {
    loop {
        tokio::select! {
            biased;
            _ = &mut stopping => break Ok(()),
            accepted = listener.accept(), if peer.is_none() => {
                let (socket, _) = accepted?;
                match WorkerCandidate::inspect(&socket, config.worker_uid).await {
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
                observation.capacity_exhausted |= !data_has_headroom(&config.ledger)?;
                pensieve_core::reconciliation_health::reconciliation_health().observe(observation);
                if config.canary_until.is_some() && canary_session_budget(started.elapsed(), canary_attempts).is_none() {
                    // Drain already-admitted work through sealing/receipt maintenance.
                    // Do not replay, plan, lease or exit merely because intake is over.
                    continue;
                }
                if !data_has_headroom(&config.ledger)? {
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
                observation.capacity_exhausted |= !data_has_headroom(&config.ledger)?;
                pensieve_core::reconciliation_health::reconciliation_health().observe(observation);
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
                            tracing::warn!(error = %error, job = lease.job().id, "worker changed before assignment");
                            parent.retry(lease, 60, RetryReason::WorkerLost).await?;
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
                            let split = parent.split(lease).await?;
                            if split == SplitOutcome::Blocked {
                                counter!("negentropy_dense_timestamp_blocked_total").increment(1);
                                tracing::error!(job, attempt, "isolated worker volume failure at one second");
                            }
                        }
                        Ok(SessionOutcome::Failed(_)) => {
                            parent.retry(lease, retry_delay(job, attempt), RetryReason::RelayFailure).await?;
                        }
                        Err(ParentError::TooDense) => {
                            let split = parent.split(lease).await?;
                            if split == SplitOutcome::Blocked {
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
                                parent.retry(lease, retry_delay(job, attempt), RetryReason::WorkerLost).await?;
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

fn data_has_headroom(ledger: &Path) -> Result<bool, RuntimeError> {
    let directory = ledger
        .parent()
        .ok_or(RuntimeError::Config("ledger has no parent"))?;
    let stats = nix::sys::statvfs::statvfs(directory)
        .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
    let available = available_bytes(stats.blocks_available(), stats.fragment_size());
    Ok(available >= DATA_RESERVE_BYTES)
}

fn available_bytes<B: Into<u64>>(blocks: B, fragment_size: u64) -> u64 {
    blocks.into().saturating_mul(fragment_size)
}

#[cfg(test)]
mod canary_tests {
    use super::*;

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

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;
    use crate::SegmentConfig;
    use crate::sync::ipc::{Frame, Message, initial_digest};
    use crate::sync::jobs::JobState;
    use crate::sync::worker::{Hello, ParentMessage, read_message, write_message};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        };
        assert!(matches!(
            IsolatedRuntime::start(config, dedupe, writer).await,
            Err(RuntimeError::SocketOccupied(path)) if path == socket
        ));
        let _peer = UnixStream::connect(&socket).await.unwrap();
        let (_accepted, _) = active.accept().await.unwrap();
        assert!(!root.path().join("jobs.sqlite").exists());
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
        };
        let runtime = IsolatedRuntime::start(config.clone(), dedupe.clone(), writer.clone())
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
            IsolatedRuntime::start(config.clone(), dedupe.clone(), writer.clone()).await,
            Err(RuntimeError::Config(_))
        ));
        let mut rolling = config;
        rolling.canary_until = None;
        assert!(matches!(
            IsolatedRuntime::start(rolling, dedupe, writer).await,
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
        let uid = std::process::Command::new("id").arg("-u").output().unwrap();
        let uid: u32 = String::from_utf8(uid.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
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
        };
        let runtime = IsolatedRuntime::start(config.clone(), dedupe, writer)
            .await
            .unwrap();
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
}
