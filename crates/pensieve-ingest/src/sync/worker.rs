//! One isolated worker session. No archive/database handles or parent scheduler.
//! The executable owns process exit; systemd must provide the non-yielding/OOM
//! backstop. Wire/ID bounds do not bound all SDK decoder/allocator memory.

mod capture;
mod reconcile;
mod transport;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout, timeout_at};

use self::capture::Capture;
#[cfg(test)]
pub(super) use self::transport::inventory;
pub use self::transport::{Assignment, Hello, ParentMessage, read_message, write_message};
pub use self::transport::{INVENTORY_CHUNK_ITEMS, hash_item, inventory_hasher};
use super::failure::{FailureDiagnostic, FailureKind};
use super::ipc::{Frame, Message, ProtocolError, VERSION};

const WALL_TIME: Duration = Duration::from_secs(9 * 60);
const IDLE_TIME: Duration = Duration::from_secs(2 * 60);
const RELAY_MESSAGE_BYTES: u32 = 5 * 1024 * 1024;

fn relay_limits() -> RelayLimits {
    // Archive admission validates signatures/IDs, not arbitrary tag counts.
    // Restore a finite wire cap; IPC/output limits remain independently enforced.
    let mut limits = RelayLimits::disable();
    limits.messages.max_size = Some(RELAY_MESSAGE_BYTES);
    limits
}

/// Sanitized worker errors; never include wire payloads, tokens or remote errors.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Socket/codec/ordering/limit failure. No completion is sent.
    #[error("worker IPC failed")]
    Protocol(#[from] ProtocolError),
    /// Wrong Unix peer, rejected before any Hello or capability exchange.
    #[error("worker parent peer UID mismatch")]
    Peer,
    /// Entire lifecycle or useful-progress budget expired.
    #[error("worker lifecycle or idle deadline exceeded")]
    Deadline,
    /// SDK error or some advertised missing IDs were not captured and fetched.
    #[error("worker relay reconciliation incomplete")]
    Incomplete,
    /// EOSE arrived with advertised IDs missing. This does not prove a volume
    /// limit or permanent absence: SDK policy drops and relay withholding look
    /// identical. Preserve the gap; do not automatically split or skip it.
    #[error("worker advertised events unavailable at EOSE")]
    Unavailable(FailureDiagnostic),
    /// Distinct verified in-window candidates exceed the attempt byte budget.
    /// A future authenticated parent may split, retaining all receipt obligations.
    #[error("worker attempt volume limit exceeded")]
    Volume,
    /// One verified event exceeds the IPC frame cap; splitting cannot fix it.
    #[error("worker individual event frame limit exceeded")]
    EventSize,
}

impl WorkerError {
    /// Dedicated-process exit classification, never an archive completion proof.
    /// Unknown exits/signals must not be interpreted as a volume limit.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Unavailable(_) => 2,
            Self::Volume => 3,
            Self::EventSize => 4,
            _ => 1,
        }
    }

    fn diagnostic(&self) -> FailureDiagnostic {
        let kind = match self {
            Self::Unavailable(report) => return report.clone(),
            Self::Volume => FailureKind::Volume,
            Self::EventSize => FailureKind::EventSize,
            _ => FailureKind::Relay,
        };
        FailureDiagnostic {
            kind,
            missing_count: 0,
            sample: Vec::new(),
        }
    }
}

/// Connect to one authenticated parent and execute at most one assignment.
/// Caller must exit this dedicated process on return, including failure. The
/// outer deadline covers connect, inventory, SDK connect/sync/disconnect and drain.
pub async fn run(socket: &Path, parent_uid: u32) -> Result<(), WorkerError> {
    run_with_limits(socket, parent_uid, WALL_TIME, IDLE_TIME).await
}

async fn run_with_limits(
    socket: &Path,
    parent_uid: u32,
    wall: Duration,
    idle: Duration,
) -> Result<(), WorkerError> {
    timeout(wall, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(ProtocolError::Io)?;
        if stream.peer_cred().map_err(ProtocolError::Io)?.uid() != parent_uid {
            return Err(WorkerError::Peer);
        }
        write_message(&mut stream, &Hello { version: VERSION }).await?;
        let (assignment, items) = timeout(idle, transport::inventory(&mut stream))
            .await
            .map_err(|_| WorkerError::Deadline)??;
        let remaining = assignment.remaining()?;
        timeout(remaining, attempt(stream, assignment, items, idle))
            .await
            .map_err(|_| WorkerError::Deadline)?
    })
    .await
    .map_err(|_| WorkerError::Deadline)?
}

async fn attempt(
    mut stream: UnixStream,
    assignment: Assignment,
    items: Vec<(EventId, Timestamp)>,
    idle_time: Duration,
) -> Result<(), WorkerError> {
    let (capture, mut events) = Capture::new(&assignment);
    let capture = Arc::new(capture);
    let client = Client::builder()
        .opts(
            ClientOptions::default()
                .relay_limits(relay_limits())
                .verify_subscriptions(true),
        )
        .database(capture.clone())
        .build();
    let relay_url = assignment.relay.clone();
    let sdk = async {
        let result = async {
            client
                .add_relay(&relay_url)
                .await
                .map_err(|_| WorkerError::Incomplete)?;
            let relay = client
                .relay(&relay_url)
                .await
                .map_err(|_| WorkerError::Incomplete)?;
            tracing::info!(relay = %relay_url, phase = "connect", "isolated reconciliation");
            relay
                .try_connect(Duration::from_secs(20))
                .await
                .map_err(|_| WorkerError::Incomplete)?;
            tracing::info!(relay = %relay_url, phase = "sync", "isolated reconciliation");
            reconcile::download(&relay, &assignment, items).await
        }
        .await;
        tracing::info!(relay = %relay_url, phase = "disconnect", "isolated reconciliation");
        client.disconnect().await;
        // Close under the callback lock, not by hoping SDK Arc destruction closes
        // its sender. Every callback accepted before this cut is already queued.
        capture.finish_download(result).await
    };
    tokio::pin!(sdk);
    let mut result = None;
    let mut admitted = 0;
    let mut idle = Instant::now() + idle_time;
    loop {
        tokio::select! {
            value = &mut sdk, if result.is_none() => {
                result = Some(value);
            }
            _ = tokio::time::sleep_until(idle) => return Err(WorkerError::Deadline),
            ready = stream.readable() => {
                ready.map_err(ProtocolError::Io)?;
                // No ACK is legal when there is no outstanding upload. Cancel,
                // EOF and unsolicited traffic all terminate this one-shot worker.
                let mut byte = [0];
                match stream.try_read(&mut byte) {
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
                    _ => return Err(ProtocolError::State.into()),
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                timeout_at(idle, async {
                    use tokio::io::AsyncWriteExt;
                    stream.write_all(&event.wire).await.map_err(ProtocolError::Io)?;
                    let ack: ParentMessage = read_message(&mut stream).await?;
                    match ack {
                        ParentMessage::Accepted { header } if assignment.matches(&header, event.sequence) => Ok(()),
                        _ => Err(ProtocolError::State),
                    }
                }).await.map_err(|_| WorkerError::Deadline)??;
                // Credit is held until a matching parent admission ACK. Arbitrary
                // SDK progress and socket chatter do not refresh this deadline.
                drop(event);
                admitted += 1;
                idle = Instant::now() + idle_time;
            }
        }
    }
    let outcome = match result {
        Some(summary) => summary,
        None => timeout_at(idle, &mut sdk)
            .await
            .map_err(|_| WorkerError::Deadline)?,
    };
    let (count, digest) = match outcome {
        Ok(summary) => summary,
        Err(error) => {
            return Err(report_failure(&mut stream, &assignment, admitted, idle, error).await);
        }
    };
    tracing::info!(relay = %relay_url, phase = "drained", count, "isolated reconciliation");
    let wire = Frame::encode(&Message::ProtocolDone {
        header: assignment.header(count + 1),
        count,
        digest,
    })?;
    use tokio::io::AsyncWriteExt;
    timeout_at(idle, stream.write_all(&wire))
        .await
        .map_err(|_| WorkerError::Deadline)?
        .map_err(ProtocolError::Io)?;
    // This is protocol completion only. Parent alone decides archive completion.
    Ok(())
}

// Already captured candidates have been drained/ACKed before this best-effort
// terminal report. A broken parent socket must not replace the verified cause.
async fn report_failure(
    stream: &mut UnixStream,
    assignment: &Assignment,
    admitted: u64,
    idle: Instant,
    error: WorkerError,
) -> WorkerError {
    let _ = timeout_at(
        idle,
        write_message(
            stream,
            &Message::AttemptFailed {
                header: assignment.header(admitted + 1),
                report: error.diagnostic(),
            },
        ),
    )
    .await;
    error
}

#[cfg(test)]
mod tests;
