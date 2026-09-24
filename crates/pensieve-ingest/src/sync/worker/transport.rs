//! Versioned bounded parent exchange. Socket peer authentication precedes this.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nostr_sdk::prelude::{EventId, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::super::ipc::{self, Accepted, Header, ProtocolError, VERSION};
use super::super::{MAX_WINDOW_ITEMS, jobs::Lease};

/// Fixed greeting, before receiving any lease capability.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// Must equal the supported upload protocol version.
    pub version: u32,
}

/// One parent-owned attempt. No Debug: header contains its secret capability.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub(super) identity: Header,
    /// Canonical single relay URL, assigned by the parent's explicit allowlist.
    pub relay: String,
    /// Inclusive event-time window.
    pub since: u64,
    /// Inclusive event-time window.
    pub until: u64,
    /// Exclusive Unix-second lease deadline.
    pub expires_at: u64,
}

impl Assignment {
    /// Export only after authenticating the Unix worker UID. Does not itself
    /// authenticate a socket or replace the parent's persisted lease checks.
    pub fn for_lease(lease: &Lease) -> Self {
        Self {
            identity: Header::for_lease(lease, 0),
            relay: lease.job().relay.clone(),
            since: lease.job().since as u64,
            until: lease.job().until as u64,
            expires_at: lease.expires_at() as u64,
        }
    }

    pub(super) fn remaining(&self) -> Result<Duration, ProtocolError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProtocolError::State)?
            .as_secs();
        let remaining = self
            .expires_at
            .checked_sub(now)
            .filter(|n| *n > 0 && *n <= 600)
            .ok_or(ProtocolError::State)?;
        Ok(Duration::from_secs(remaining))
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        if self.identity.version != VERSION
            || self.identity.sequence != 0
            || self.identity.job <= 0
            || self.identity.attempt <= 0
            || self.since > self.until
            || self.until > i64::MAX as u64
            || self.until - self.since >= 900
            || self.relay.len() > 2048
            || self.relay.contains(['@', '?', '#'])
        {
            return Err(ProtocolError::State);
        }
        let relay = RelayUrl::parse(&self.relay).map_err(|_| ProtocolError::State)?;
        if relay.to_string() != self.relay {
            return Err(ProtocolError::State);
        }
        self.remaining()?;
        Ok(())
    }

    /// Header for the next message in that direction. Parent inventory sequence
    /// starts at one; worker upload sequence independently starts at one.
    pub fn header(&self, sequence: u64) -> Header {
        Header {
            sequence,
            ..self.identity.clone()
        }
    }

    pub(super) fn matches(&self, header: &Header, sequence: u64) -> bool {
        header.version == VERSION
            && header.job == self.identity.job
            && header.attempt == self.identity.attempt
            && header.token == self.identity.token
            && header.sequence == sequence
    }
}

/// Maximum fixed inventory records in one framed chunk.
pub const INVENTORY_CHUNK_ITEMS: usize = 256;

/// Parent-to-worker traffic. No variant means the archive job is complete.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParentMessage {
    /// Must be first; exactly one job per worker process.
    Job { assignment: Assignment },
    /// Strict timestamp/ID ordering, at most 256 records; no duplicates.
    InventoryChunk {
        header: Header,
        items: Vec<([u8; 32], u64)>,
    },
    /// Explicit complete input, including empty inventory. Digest hashes the
    /// fixed records (big-endian timestamp then ID), not their JSON encoding.
    InventoryEnd {
        header: Header,
        count: usize,
        digest: [u8; 32],
    },
    /// Echoes the admitted event's upload identity and sequence, not durability.
    Accepted { header: Header },
    /// Cancellation/any unexpected message makes this worker fail closed.
    Cancel { header: Header },
}

impl ParentMessage {
    /// Wrap the typed parent admission result without changing its identity.
    pub fn accepted(receipt: Accepted) -> Self {
        Self::Accepted {
            header: receipt.header,
        }
    }
}

/// Read a length-prefixed JSON value with pre-allocation length validation.
/// The caller must wrap the whole exchange in a deadline, not each individual read.
pub async fn read_message<R, T>(reader: &mut R) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let wire = read_wire(reader).await?;
    serde_json::from_slice(&wire[4..]).map_err(|_| ProtocolError::Malformed)
}

pub(super) async fn read_wire<R>(reader: &mut R) -> Result<Vec<u8>, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > ipc::MAX_FRAME_BYTES {
        return Err(ProtocolError::Limit);
    }
    let mut wire = vec![0; length + 4];
    wire[..4].copy_from_slice(&(length as u32).to_be_bytes());
    reader.read_exact(&mut wire[4..]).await?;
    Ok(wire)
}

/// Bounded serialization, using the same cap as worker event uploads.
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    writer.write_all(&ipc::encode_value(message)?).await?;
    Ok(())
}

/// Initial canonical inventory digest state. Shared with the future parent.
pub fn inventory_hasher() -> sha2::Sha256 {
    use sha2::Digest;
    sha2::Sha256::new_with_prefix(b"pensieve-negentropy-inventory-v1\0")
}

/// Update the inventory digest with one fixed-width record.
pub fn hash_item(hash: &mut sha2::Sha256, id: &[u8; 32], timestamp: u64) {
    use sha2::Digest;
    hash.update(timestamp.to_be_bytes());
    hash.update(id);
}

pub(in crate::sync) async fn inventory<R>(
    reader: &mut R,
) -> Result<(Assignment, Vec<(EventId, Timestamp)>), ProtocolError>
where
    R: AsyncRead + Unpin,
{
    use sha2::Digest;
    let ParentMessage::Job { assignment } = read_message(reader).await? else {
        return Err(ProtocolError::State);
    };
    assignment.validate()?;
    let mut items = Vec::new();
    let mut previous = None;
    let mut seen_ids = HashSet::new();
    let mut hash = inventory_hasher();
    let mut sequence = 1;
    loop {
        match read_message(reader).await? {
            ParentMessage::InventoryChunk {
                header,
                items: chunk,
            } => {
                if !assignment.matches(&header, sequence)
                    || chunk.is_empty()
                    || chunk.len() > INVENTORY_CHUNK_ITEMS
                    || items.len() + chunk.len() > MAX_WINDOW_ITEMS
                {
                    return Err(ProtocolError::Limit);
                }
                for (id, timestamp) in chunk {
                    if timestamp < assignment.since
                        || timestamp > assignment.until
                        || previous.is_some_and(|p| p >= (timestamp, id))
                        || !seen_ids.insert(id)
                    {
                        return Err(ProtocolError::State);
                    }
                    previous = Some((timestamp, id));
                    hash_item(&mut hash, &id, timestamp);
                    items.push((EventId::from_byte_array(id), Timestamp::from(timestamp)));
                }
                sequence += 1;
            }
            ParentMessage::InventoryEnd {
                header,
                count,
                digest,
            } => {
                if !assignment.matches(&header, sequence)
                    || count != items.len()
                    || digest != <[u8; 32]>::from(hash.finalize())
                {
                    return Err(ProtocolError::State);
                }
                return Ok((assignment, items));
            }
            _ => return Err(ProtocolError::State),
        }
    }
}
