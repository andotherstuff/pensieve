//! Explicitly enabled, single-worker isolated reconciliation runtime.
//!
//! The ingester owns the listener, ledger, inventory and archive. It never
//! launches or restarts the static systemd worker. All unfinished jobs survive
//! worker loss, shutdown and local faults; a socket exchange is not completion.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
        let ledger = JobLedger::open(&config.ledger, LedgerLimits::default())?;
        let inventory = Arc::new(SyncStateDb::open(&config.inventory)?);
        let mut parent = ParentExecutor::new(ledger, inventory, dedupe, writer.clone())?;
        parent.configure(config.relays.clone()).await?;
        // Refuse an existing path; never unlink an unknown listener or replace a
        // socket belonging to another process.
        let listener = UnixListener::bind(&config.socket)?;
        fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o660))?;
        let inode = fs::symlink_metadata(&config.socket)?.ino();
        let (stop, stopping) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = drive(&config, listener, parent, writer, stopping).await;
            gauge!("negentropy_isolated_ready").set(0.0);
            if let Err(ref error) = result {
                tracing::error!(error = %error, "isolated reconciliation paused; durable gaps retained");
            }
            // Remove only the socket created here; a replacement is not ours.
            if fs::symlink_metadata(&config.socket).is_ok_and(|meta| meta.ino() == inode) {
                let _ = fs::remove_file(&config.socket);
            }
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

async fn drive(
    config: &RuntimeConfig,
    listener: UnixListener,
    mut parent: ParentExecutor,
    writer: Arc<SegmentWriter>,
    mut stopping: oneshot::Receiver<()>,
) -> Result<(), RuntimeError> {
    let mut interval = tokio::time::interval(TICK);
    let mut peer: Option<(UnixStream, WorkerCandidate)> = None;
    gauge!("negentropy_isolated_ready").set(1.0);
    let result: Result<(), RuntimeError> = async {
    loop {
        tokio::select! {
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
                if !data_has_headroom(&config.ledger)? {
                    gauge!("negentropy_isolated_ledger_space_pause").set(1.0);
                    continue;
                }
                gauge!("negentropy_isolated_ledger_space_pause").set(0.0);
                // Replay one sealed source segment before serving an assignment.
                parent.replay_next(
                    config.archive.clone(), config.segment_prefix.clone(), config.replay_floor,
                ).await?;
                let planned = parent.plan(32).await?;
                gauge!("negentropy_isolated_planner_backpressured").set(f64::from(planned.backpressured));
                if let Some((socket, candidate)) = peer.take() {
                    // Revalidate the same pidfd, invocation and remaining service
                    // lifetime immediately before exporting a lease capability.
                    match candidate.recheck().await {
                        Ok(()) => {}
                        Err(error) => {
                            counter!("negentropy_worker_binding_rejections_total").increment(1);
                            tracing::warn!(error = %error, "worker changed while idle");
                            continue;
                        }
                    }
                    let Some(lease) = parent.lease_next_fair(LEASE_SECS).await? else {
                        peer = Some((socket, candidate));
                        continue;
                    };
                    if let Err(error) = candidate.recheck().await {
                        tracing::warn!(error = %error, job = lease.job().id, "worker changed before assignment");
                        parent.retry(lease, 60, RetryReason::WorkerLost).await?;
                        continue;
                    }
                    let job = lease.job().id;
                    let attempt = lease.job().attempt;
                    let outcome = tokio::select! {
                        _ = &mut stopping => break Ok(()),
                        outcome = parent.serve(socket, config.worker_uid, lease.clone(), SESSION_TIME) => outcome,
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
    let available = u64::from(stats.blocks_available()).saturating_mul(stats.fragment_size());
    Ok(available >= DATA_RESERVE_BYTES)
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
