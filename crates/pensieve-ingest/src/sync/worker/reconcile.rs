//! Pensieve-owned download-only NIP-77 state machine over SDK relay transport.
//! Unlike the pinned SDK convenience sync loop, notification lag/closure is an
//! error, never a successful empty diff. No second support-check subscriber.

use std::borrow::Cow;
use std::collections::HashSet;

use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use super::super::ipc::MAX_EVENTS;
use super::progress::{Phase, Progress};
use super::{Assignment, WorkerError};

const NEG_FRAME: usize = 60_000;
const FETCH_BATCH: usize = 128;

fn record_reply(replies: &mut HashSet<[u8; 32]>, bytes: &[u8]) -> Result<(), WorkerError> {
    if replies.len() >= 4096 || !replies.insert(Sha256::digest(bytes).into()) {
        return Err(WorkerError::Incomplete);
    }
    Ok(())
}

/// Constructed only after a complete diff and every requested fetch reaches EOSE.
#[cfg_attr(test, derive(Default))]
pub(super) struct DownloadProof {
    pub remote: HashSet<EventId>,
    pub received: HashSet<EventId>,
}

async fn notification(
    receiver: &mut broadcast::Receiver<RelayNotification>,
) -> Result<RelayNotification, WorkerError> {
    // In particular Lagged and Closed must not fall through as success.
    receiver.recv().await.map_err(|_| WorkerError::Incomplete)
}

#[cfg(test)]
pub(super) async fn download(
    relay: &Relay,
    job: &Assignment,
    items: Vec<(EventId, Timestamp)>,
) -> Result<DownloadProof, WorkerError> {
    let progress = Progress::new(super::IDLE_TIME, super::COMPARE_TIME);
    download_with_progress(relay, job, items, &progress).await
}

pub(super) async fn download_with_progress(
    relay: &Relay,
    job: &Assignment,
    items: Vec<(EventId, Timestamp)>,
    progress: &Progress,
) -> Result<DownloadProof, WorkerError> {
    let mut storage = negentropy::NegentropyStorageVector::with_capacity(items.len());
    for (id, timestamp) in items {
        storage
            .insert(
                timestamp.as_secs(),
                negentropy::Id::from_byte_array(id.to_bytes()),
            )
            .map_err(|_| WorkerError::Incomplete)?;
    }
    storage.seal().map_err(|_| WorkerError::Incomplete)?;
    let mut engine = negentropy::Negentropy::borrowed(&storage, NEG_FRAME as u64)
        .map_err(|_| WorkerError::Incomplete)?;
    let initial = engine.initiate().map_err(|_| WorkerError::Incomplete)?;
    let id = SubscriptionId::generate();
    let mut notifications = relay.notifications();
    relay
        .send_msg(ClientMessage::NegOpen {
            subscription_id: Cow::Borrowed(&id),
            filter: Cow::Owned(
                Filter::new()
                    .since(Timestamp::from(job.since))
                    .until(Timestamp::from(job.until)),
            ),
            id_size: None,
            initial_message: Cow::Owned(hex::encode(initial)),
        })
        .map_err(|_| WorkerError::Incomplete)?;
    let mut remote = HashSet::new();
    // Bound both bookkeeping and protocol-only work. Repeated replies are not
    // useful progress, even if they decode; never let chatter extend a lease.
    let mut replies = HashSet::new();
    loop {
        match notification(&mut notifications).await? {
            RelayNotification::Message {
                message:
                    RelayMessage::NegMsg {
                        subscription_id,
                        message,
                    },
            } if subscription_id.as_ref() == &id => {
                // Our outbound frame target does not constrain the responder.
                // Accept larger bounded replies from strfry/khatru as well.
                if message.len() > super::RELAY_MESSAGE_BYTES as usize {
                    return Err(WorkerError::Incomplete);
                }
                let bytes = hex::decode(message.as_ref()).map_err(|_| WorkerError::Incomplete)?;
                record_reply(&mut replies, &bytes)?;
                let mut have = Vec::new();
                let mut need = Vec::new();
                let next = engine
                    .reconcile_with_ids(&bytes, &mut have, &mut need)
                    .map_err(|_| WorkerError::Incomplete)?;
                for id in need {
                    remote.insert(EventId::from_byte_array(id.to_bytes()));
                    if remote.len() > MAX_EVENTS as usize {
                        // Relay-advertised IDs are not verified events. Do not
                        // treat this alone as proven volume for automatic splitting.
                        return Err(WorkerError::Incomplete);
                    }
                }
                progress.round(remote.len());
                let state = progress.state();
                tracing::info!(relay = %job.relay, phase = "compare", rounds = state.rounds, missing = state.missing, "isolated comparison progress");
                match next {
                    Some(next) => relay
                        .send_msg(ClientMessage::NegMsg {
                            subscription_id: Cow::Borrowed(&id),
                            message: Cow::Owned(hex::encode(next)),
                        })
                        .map_err(|_| WorkerError::Incomplete)?,
                    None => break, // The only successful exit from the diff loop.
                }
            }
            RelayNotification::Message {
                message:
                    RelayMessage::NegErr {
                        subscription_id, ..
                    }
                    | RelayMessage::Closed {
                        subscription_id, ..
                    },
            } if subscription_id.as_ref() == &id => return Err(WorkerError::Incomplete),
            RelayNotification::RelayStatus { status } if status != RelayStatus::Connected => {
                return Err(WorkerError::Incomplete);
            }
            RelayNotification::Shutdown => return Err(WorkerError::Incomplete),
            _ => (),
        }
    }
    relay
        .send_msg(ClientMessage::NegClose {
            subscription_id: Cow::Borrowed(&id),
        })
        .map_err(|_| WorkerError::Incomplete)?;
    let mut ids: Vec<_> = remote.iter().copied().collect();
    ids.sort_unstable();
    let mut received = HashSet::new();
    progress.phase(Phase::Fetch);
    tracing::info!(relay = %job.relay, phase = "fetch", missing = ids.len(), batches = ids.len().div_ceil(FETCH_BATCH), "isolated fetch started");
    for batch in ids.chunks(FETCH_BATCH) {
        // A relay may cap (or default) the number of results per REQ below the
        // batch and send EOSE early. On a window whose delivered events were
        // archived before the replay floor the diff never shrinks, so the same
        // tail would be missing on every attempt and could be excluded although
        // the relay would deliver it to a smaller REQ. Re-request exactly the
        // still-missing IDs, at least once and again while each REQ delivers
        // something new. Every REQ but the first and the last delivers at least
        // one new ID, so a batch of n IDs takes at most n + 1 REQs. Only IDs
        // that a REQ naming exactly them did not return are unavailable.
        let mut request = batch.to_vec();
        let mut follow_up = false;
        loop {
            let delivered = fetch(relay, &mut notifications, &request, &mut received).await?;
            progress.batch();
            tracing::info!(relay = %job.relay, phase = "fetch", batches = progress.state().batches, requested = request.len(), delivered, received = received.len(), missing = ids.len(), "isolated fetch progress");
            request.retain(|id| !received.contains(id));
            if request.is_empty() {
                break;
            }
            if follow_up && delivered == 0 {
                let remaining: Vec<_> = ids.iter().filter(|id| !received.contains(id)).collect();
                return Err(WorkerError::Unavailable(super::FailureDiagnostic {
                    kind: super::FailureKind::Unavailable,
                    missing_count: remaining.len() as u64,
                    sample: remaining
                        .into_iter()
                        .take(crate::sync::failure::MAX_FAILURE_SAMPLE)
                        .map(|id| id.to_bytes())
                        .collect(),
                }));
            }
            follow_up = true;
        }
    }
    Ok(DownloadProof { remote, received })
}

/// One REQ for exactly `request` (sorted), until its EOSE. Every event must be
/// one of the requested IDs; it is recorded in `received` (capture has already
/// verified and queued it through the SDK database callback). Returns how many
/// requested IDs arrived that had not been received before.
async fn fetch(
    relay: &Relay,
    notifications: &mut broadcast::Receiver<RelayNotification>,
    request: &[EventId],
    received: &mut HashSet<EventId>,
) -> Result<usize, WorkerError> {
    let fetch = SubscriptionId::generate();
    relay
        .subscribe_with_id(
            fetch.clone(),
            Filter::new().ids(request.iter().copied()),
            SubscribeOptions::default(),
        )
        .await
        .map_err(|_| WorkerError::Incomplete)?;
    let mut delivered = 0;
    loop {
        match notification(notifications).await? {
            RelayNotification::Message {
                message:
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    },
            } if subscription_id.as_ref() == &fetch => {
                if request.binary_search(&event.id).is_err() {
                    return Err(WorkerError::Incomplete);
                }
                delivered += usize::from(received.insert(event.id));
            }
            RelayNotification::Message {
                message: RelayMessage::EndOfStoredEvents(subscription_id),
            } if subscription_id.as_ref() == &fetch => {
                relay
                    .unsubscribe(&fetch)
                    .await
                    .map_err(|_| WorkerError::Incomplete)?;
                return Ok(delivered);
            }
            RelayNotification::Message {
                message:
                    RelayMessage::Closed {
                        subscription_id, ..
                    },
            } if subscription_id.as_ref() == &fetch => return Err(WorkerError::Incomplete),
            RelayNotification::RelayStatus { status } if status != RelayStatus::Connected => {
                return Err(WorkerError::Incomplete);
            }
            RelayNotification::Shutdown => return Err(WorkerError::Incomplete),
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_exclusion_rule_matches_the_fetch_batch() {
        // The parent treats a diff of at most this size as fully requested.
        assert_eq!(FETCH_BATCH as u64, crate::sync::jobs::WORKER_FETCH_BATCH);
    }

    #[test]
    fn repeated_and_excessive_replies_fail_closed() {
        let mut replies = HashSet::new();
        record_reply(&mut replies, b"reply").unwrap();
        assert!(matches!(
            record_reply(&mut replies, b"reply"),
            Err(WorkerError::Incomplete)
        ));
        replies.clear();
        for n in 0u32..4096 {
            record_reply(&mut replies, &n.to_be_bytes()).unwrap();
        }
        assert!(matches!(
            record_reply(&mut replies, b"new reply"),
            Err(WorkerError::Incomplete)
        ));
    }

    #[tokio::test]
    async fn lagged_or_closed_notification_stream_is_incomplete() {
        let (tx, mut rx) = broadcast::channel(2);
        for _ in 0..4 {
            tx.send(RelayNotification::Shutdown).unwrap();
        }
        assert!(matches!(
            notification(&mut rx).await,
            Err(WorkerError::Incomplete)
        ));
        let (tx, mut rx) = broadcast::channel(2);
        drop(tx);
        assert!(matches!(
            notification(&mut rx).await,
            Err(WorkerError::Incomplete)
        ));
    }
}
