//! Pensieve-owned download-only NIP-77 state machine over SDK relay transport.
//! Unlike the pinned SDK convenience sync loop, notification lag/closure is an
//! error, never a successful empty diff. No second support-check subscriber.

use std::borrow::Cow;
use std::collections::HashSet;

use nostr_sdk::prelude::*;
use tokio::sync::broadcast;

use super::super::ipc::MAX_EVENTS;
use super::{Assignment, WorkerError};

const NEG_FRAME: usize = 60_000;
const FETCH_BATCH: usize = 128;

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

pub(super) async fn download(
    relay: &Relay,
    job: &Assignment,
    items: Vec<(EventId, Timestamp)>,
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
                let mut have = Vec::new();
                let mut need = Vec::new();
                let next = engine
                    .reconcile_with_ids(&bytes, &mut have, &mut need)
                    .map_err(|_| WorkerError::Incomplete)?;
                for id in need {
                    remote.insert(EventId::from_byte_array(id.to_bytes()));
                    if remote.len() > MAX_EVENTS as usize {
                        return Err(WorkerError::Incomplete);
                    }
                }
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
    for batch in ids.chunks(FETCH_BATCH) {
        let fetch = SubscriptionId::generate();
        relay
            .subscribe_with_id(
                fetch.clone(),
                Filter::new().ids(batch.iter().copied()),
                SubscribeOptions::default(),
            )
            .await
            .map_err(|_| WorkerError::Incomplete)?;
        loop {
            match notification(&mut notifications).await? {
                RelayNotification::Message {
                    message:
                        RelayMessage::Event {
                            subscription_id,
                            event,
                        },
                } if subscription_id.as_ref() == &fetch => {
                    if batch.binary_search(&event.id).is_err() {
                        return Err(WorkerError::Incomplete);
                    }
                    received.insert(event.id);
                }
                RelayNotification::Message {
                    message: RelayMessage::EndOfStoredEvents(subscription_id),
                } if subscription_id.as_ref() == &fetch => {
                    if !batch.iter().all(|id| received.contains(id)) {
                        return Err(WorkerError::Unavailable);
                    }
                    relay
                        .unsubscribe(&fetch)
                        .await
                        .map_err(|_| WorkerError::Incomplete)?;
                    break;
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
    Ok(DownloadProof { remote, received })
}

#[cfg(test)]
mod tests {
    use super::*;

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
