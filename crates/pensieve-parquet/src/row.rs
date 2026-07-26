//! Raw inputs and validated, owned rows used by the canonical writer.

use nostr::prelude::{Event, EventId, Kind, PublicKey, Signature, Tag, Tags, Timestamp};
use notepack::{NoteParser, StringType};

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
    pub fn from_notepack(payload: &[u8]) -> Result<Self> {
        let note = NoteParser::new(payload).into_note_strict()?;
        let kind =
            u16::try_from(note.kind).map_err(|_| Error::KindOutOfRange { kind: note.kind })?;
        let mut tags_cursor = note.tags.clone();
        let mut tags = Vec::with_capacity(tags_cursor.len().try_into().unwrap_or(usize::MAX));

        while let Some(mut elements) = tags_cursor.next_tag()? {
            let mut tag = Vec::with_capacity(elements.remaining().try_into().unwrap_or(usize::MAX));
            for element in &mut elements {
                tag.push(match element? {
                    StringType::Str(value) => value.to_owned(),
                    StringType::Bytes(value) => hex::encode(value),
                });
            }
            tags.push(tag);
        }

        Self::from_raw(RawEvent {
            id: *note.id,
            pubkey: *note.pubkey,
            created_at: note.created_at,
            kind,
            tags,
            content: note.content.to_owned(),
            sig: *note.sig,
        })
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
