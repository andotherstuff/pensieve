//! Raw inputs and validated, owned rows used by the canonical writer.

use nostr::prelude::{Event, EventId, Kind, PublicKey, Signature, Tag, Tags, Timestamp};
use notepack::{Error as NotepackError, SUPPORTED_VERSION};

use crate::{Error, Result};

/// One owned event in the V1 logical shape, before cryptographic validation.
///
/// This is the efficient boundary for decoders that already have typed field
/// values. It avoids a JSON serialization/deserialization round trip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawEvent {
    /// Claimed raw 32-byte Nostr event ID.
    pub id: [u8; 32],
    /// Raw 32-byte author public key.
    pub pubkey: [u8; 32],
    /// Unsigned Nostr creation timestamp.
    pub created_at: u64,
    /// Unsigned Nostr event kind.
    pub kind: u16,
    /// Ordered Nostr tags and their ordered positional elements.
    pub tags: Vec<Vec<String>>,
    /// Exact event content, including an allowed empty string.
    pub content: String,
    /// Raw 64-byte BIP-340 signature.
    pub sig: [u8; 64],
}

/// One validated Nostr event in the exact logical shape stored by V1.
///
/// Fields are private so callers cannot construct rows that bypass ID,
/// signature, or tag-shape validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalEvent {
    pub(crate) id: [u8; 32],
    pub(crate) pubkey: [u8; 32],
    pub(crate) created_at: u64,
    pub(crate) kind: u16,
    pub(crate) tags: Vec<Vec<String>>,
    pub(crate) content: String,
    pub(crate) sig: [u8; 64],
}

impl CanonicalEvent {
    /// Validate an owned, already-decoded event without passing through JSON.
    pub fn from_raw(raw: RawEvent) -> Result<Self> {
        let id_hex = hex::encode(raw.id);
        if let Some(tag_index) = raw.tags.iter().position(Vec::is_empty) {
            return Err(Error::EmptyTag {
                id: id_hex,
                tag_index,
            });
        }

        let public_key = PublicKey::from_slice(&raw.pubkey)
            .map_err(|_| Error::InvalidPublicKey { id: id_hex.clone() })?;
        let tags: Vec<Tag> = raw
            .tags
            .iter()
            .map(|tag| Tag::parse(tag.iter().map(String::as_str)))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| Error::InvalidFile(format!("event {id_hex} contains an invalid tag")))?;
        let created_at = Timestamp::from(raw.created_at);
        let kind = Kind::from_u16(raw.kind);
        let nostr_tags = Tags::from_list(tags.clone());
        let computed_id = EventId::new(&public_key, &created_at, &kind, &nostr_tags, &raw.content);
        if computed_id.as_bytes() != &raw.id {
            return Err(Error::InvalidEventId { id: id_hex });
        }

        let signature = Signature::from_slice(&raw.sig).map_err(|_| Error::InvalidSignature {
            id: computed_id.to_hex(),
        })?;
        let event = Event::new(
            computed_id,
            public_key,
            created_at,
            kind,
            tags,
            raw.content.clone(),
            signature,
        );
        if !event.verify_signature() {
            return Err(Error::InvalidSignature {
                id: event.id.to_hex(),
            });
        }

        Ok(Self {
            id: raw.id,
            pubkey: raw.pubkey,
            created_at: raw.created_at,
            kind: raw.kind,
            tags: raw.tags,
            content: raw.content,
            sig: raw.sig,
        })
    }

    /// Decode and validate one strict notepack payload without converting it to JSON.
    ///
    /// The framed-segment readers bound the payload before calling this method.
    /// Decoding therefore trusts the complete frame size as its allocation
    /// boundary instead of notepack's unrelated 256 KiB convenience limit.
    pub fn from_notepack(payload: &[u8]) -> Result<Self> {
        Self::from_raw(decode_notepack_strict(payload)?)
    }

    /// Validate and copy a typed Nostr event into a canonical archive row.
    pub fn from_event(event: &Event) -> Result<Self> {
        let id_hex = event.id.to_hex();

        if !event.verify_id() {
            return Err(Error::InvalidEventId { id: id_hex });
        }
        if !event.verify_signature() {
            return Err(Error::InvalidSignature { id: id_hex });
        }

        let tags: Vec<Vec<String>> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice().iter().map(ToString::to_string).collect())
            .collect();

        if let Some(tag_index) = tags.iter().position(Vec::is_empty) {
            return Err(Error::EmptyTag {
                id: event.id.to_hex(),
                tag_index,
            });
        }

        Ok(Self {
            id: *event.id.as_bytes(),
            pubkey: *event.pubkey.as_bytes(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags,
            content: event.content.clone(),
            sig: *event.sig.as_ref(),
        })
    }

    /// Return the raw 32-byte event ID.
    pub fn id(&self) -> &[u8; 32] {
        &self.id
    }

    /// Return the raw 32-byte author public key.
    pub fn pubkey(&self) -> &[u8; 32] {
        &self.pubkey
    }

    /// Return the unsigned Nostr creation timestamp.
    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    /// Return the unsigned 16-bit Nostr event kind.
    pub fn kind(&self) -> u16 {
        self.kind
    }

    /// Return the event tags with both levels of ordering preserved.
    pub fn tags(&self) -> &[Vec<String>] {
        &self.tags
    }

    /// Return the exact event content.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Return the raw 64-byte Schnorr signature.
    pub fn signature(&self) -> &[u8; 64] {
        &self.sig
    }

    /// Estimate the row's uncompressed Arrow footprint for deterministic batching.
    ///
    /// This includes fixed fields, UTF-8 payload bytes, and conservative offset
    /// overhead for both nested list levels. It is an operational sizing estimate,
    /// not a canonical property of the event or resulting Parquet file.
    pub fn estimated_uncompressed_bytes(&self) -> usize {
        const FIXED_FIELDS: usize = 32 + 32 + 8 + 2 + 64;
        const OUTER_LIST_OFFSET: usize = 8;
        const INNER_LIST_OFFSET: usize = 8;
        const STRING_OFFSET: usize = 8;

        let tag_bytes = self
            .tags
            .iter()
            .map(|tag| {
                INNER_LIST_OFFSET
                    + tag
                        .iter()
                        .map(|value| STRING_OFFSET + value.len())
                        .sum::<usize>()
            })
            .sum::<usize>();
        FIXED_FIELDS + OUTER_LIST_OFFSET + STRING_OFFSET + self.content.len() + tag_bytes
    }
}

/// Decode the complete notepack V1 payload using the enclosing frame as the
/// allocation boundary.
///
/// `notepack::NoteParser::into_note_strict` imposes a fixed 256 KiB content
/// limit even when the caller has already bounded the payload. Pensieve accepts
/// frames up to 16 MiB, so parsing the small stable wire shape locally keeps
/// strict trailing-byte validation without rejecting otherwise valid events.
fn decode_notepack_strict(payload: &[u8]) -> Result<RawEvent> {
    let mut input = payload;
    let version = read_varint(&mut input)?;
    if version != u64::from(SUPPORTED_VERSION) {
        return Err(NotepackError::UnsupportedVersion(version).into());
    }

    let id = read_array(&mut input)?;
    let pubkey = read_array(&mut input)?;
    let sig = read_array(&mut input)?;
    let created_at = read_varint(&mut input)?;
    let kind_value = read_varint(&mut input)?;
    let kind = u16::try_from(kind_value).map_err(|_| Error::KindOutOfRange { kind: kind_value })?;

    let content_length = read_varint(&mut input)?;
    let content_bytes = read_bytes(content_length, &mut input)?;
    let content = std::str::from_utf8(content_bytes)
        .map_err(NotepackError::from)?
        .to_owned();

    let tag_count = read_varint(&mut input)?;
    ensure_count_can_fit(tag_count, input.len())?;
    let mut tags = Vec::with_capacity(initial_capacity(tag_count));
    for _ in 0..tag_count {
        let element_count = read_varint(&mut input)?;
        ensure_count_can_fit(element_count, input.len())?;
        let mut tag = Vec::with_capacity(initial_capacity(element_count));
        for _ in 0..element_count {
            let tagged_length = read_varint(&mut input)?;
            let is_bytes = tagged_length & 1 != 0;
            let element = read_bytes(tagged_length >> 1, &mut input)?;
            tag.push(if is_bytes {
                hex::encode(element)
            } else {
                std::str::from_utf8(element)
                    .map_err(NotepackError::from)?
                    .to_owned()
            });
        }
        tags.push(tag);
    }

    if !input.is_empty() {
        return Err(NotepackError::TrailingBytes.into());
    }

    Ok(RawEvent {
        id,
        pubkey,
        created_at,
        kind,
        tags,
        content,
        sig,
    })
}

fn initial_capacity(count: u64) -> usize {
    usize::try_from(count.min(4_096)).expect("bounded capacity fits usize")
}

fn ensure_count_can_fit(count: u64, remaining_bytes: usize) -> Result<()> {
    if count > remaining_bytes as u64 {
        return Err(NotepackError::Truncated.into());
    }
    Ok(())
}

fn read_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N]> {
    let bytes = read_bytes(N as u64, input)?;
    Ok(bytes.try_into().expect("read_bytes returned exact length"))
}

fn read_bytes<'a>(length: u64, input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let length = usize::try_from(length).map_err(|_| NotepackError::VarintOverflow)?;
    if length > input.len() {
        return Err(NotepackError::Truncated.into());
    }
    let (value, remaining) = input.split_at(length);
    *input = remaining;
    Ok(value)
}

fn read_varint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;

    for (index, byte) in input.iter().copied().enumerate() {
        let chunk = u64::from(byte & 0x7f);
        if shift == 63 && chunk & 0x7e != 0 {
            return Err(NotepackError::VarintOverflow.into());
        }
        value |= chunk << shift;

        if byte & 0x80 == 0 {
            *input = &input[index + 1..];
            return Ok(value);
        }

        shift += 7;
        if shift >= 64 {
            return Err(NotepackError::VarintOverflow.into());
        }
    }

    Err(NotepackError::VarintUnterminated.into())
}

impl TryFrom<&Event> for CanonicalEvent {
    type Error = Error;

    fn try_from(event: &Event) -> Result<Self> {
        Self::from_event(event)
    }
}

impl TryFrom<RawEvent> for CanonicalEvent {
    type Error = Error;

    fn try_from(raw: RawEvent) -> Result<Self> {
        Self::from_raw(raw)
    }
}
