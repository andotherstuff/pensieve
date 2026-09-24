//! One isolated worker session. No archive/database handles or parent scheduler.
//! The executable owns process exit; systemd must provide the non-yielding/OOM
//! backstop. Wire bounds do not bound the pinned SDK's remote reconciliation sets.

mod capture;
mod transport;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout, timeout_at};

use self::capture::Capture;
pub use self::transport::{Assignment, Hello, ParentMessage, read_message, write_message};
pub use self::transport::{INVENTORY_CHUNK_ITEMS, hash_item, inventory_hasher};
use super::ipc::{Frame, Message, ProtocolError, VERSION};

const WALL_TIME: Duration = Duration::from_secs(9 * 60);
const IDLE_TIME: Duration = Duration::from_secs(2 * 60);

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
    let client = Client::builder().database(capture.clone()).build();
    let relay_url = assignment.relay.clone();
    let sdk = async {
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
        let result = relay
            .sync_with_items(
                Filter::new()
                    .since(Timestamp::from(assignment.since))
                    .until(Timestamp::from(assignment.until)),
                items,
                &SyncOptions::default().direction(SyncDirection::Down),
            )
            .await;
        tracing::info!(relay = %relay_url, phase = "disconnect", "isolated reconciliation");
        client.disconnect().await;
        // Close under the callback lock, not by hoping SDK Arc destruction closes
        // its sender. Every callback accepted before this cut is already queued.
        capture.finish(result.as_ref().ok()).await
    };
    tokio::pin!(sdk);
    let mut result = None;
    let mut idle = Instant::now() + idle_time;
    loop {
        tokio::select! {
            value = &mut sdk, if result.is_none() => {
                result = Some(value?);
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
                idle = Instant::now() + idle_time;
            }
        }
    }
    let (count, digest) = match result {
        Some(summary) => summary,
        None => timeout_at(idle, &mut sdk)
            .await
            .map_err(|_| WorkerError::Deadline)??,
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

#[cfg(test)]
mod tests;
