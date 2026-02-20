//! NIP-19 identifier resolution.
//!
//! Decodes bech32-encoded Nostr identifiers (npub, note, nprofile, nevent, naddr)
//! and dispatches to the appropriate ClickHouse queries.

use nostr::nips::nip19::{FromBech32, Nip19, Nip19Event, Nip19Profile};
use nostr::{EventId, PublicKey, ToBech32};

use crate::error::PreviewError;
use crate::query::{self, EngagementCounts, EventRow, ProfileMetadata};
use crate::state::AppState;

/// The resolved result of a NIP-19 identifier lookup.
#[derive(Debug)]
pub enum ResolvedContent {
    /// A user profile (kind 0).
    Profile {
        /// The pubkey in hex.
        pubkey_hex: String,
        /// The npub bech32 encoding.
        npub: String,
        /// The nprofile bech32 encoding (for nostr: links).
        nprofile: String,
        /// Parsed profile metadata.
        metadata: ProfileMetadata,
        /// Event ID of the kind 0 metadata event.
        profile_event_id: Option<String>,
    },

    /// A single event (any kind).
    Event {
        /// The event data (boxed to reduce enum size).
        event: Box<EventRow>,
        /// The author's profile metadata (if available).
        author: Option<ProfileMetadata>,
        /// Event ID of the author's kind 0 metadata event.
        author_profile_event_id: Option<String>,
        /// The author's npub.
        author_npub: String,
        /// The note/nevent bech32 encoding.
        nevent: String,
        /// Engagement counts.
        engagement: EngagementCounts,
        /// For replies: the parent event's author name.
        reply_to_author: Option<String>,
        /// For replies: the parent event's hex ID.
        reply_to_event_id: Option<String>,
    },
}

/// Decode a NIP-19 identifier string and resolve it against the database.
///
/// Accepts:
/// - `npub1...` - Public key
/// - `nprofile1...` - Profile with relay hints
/// - `note1...` - Event ID
/// - `nevent1...` - Event with relay hints
/// - `naddr1...` - Replaceable event coordinate
/// - 64-character hex string - Treated as event ID first, then pubkey
pub async fn resolve(state: &AppState, identifier: &str) -> Result<ResolvedContent, PreviewError> {
    // Try NIP-19 bech32 decoding first
    if let Ok(nip19) = Nip19::from_bech32(identifier) {
        return match nip19 {
            Nip19::Pubkey(pk) => resolve_profile(state, &pk).await,
            Nip19::Profile(profile) => resolve_profile(state, &profile.public_key).await,
            Nip19::EventId(id) => resolve_event_by_id(state, &id).await,
            Nip19::Event(event) => resolve_event_by_id(state, &event.event_id).await,
            Nip19::Coordinate(coord) => {
                let kind = coord.coordinate.kind.as_u16();
                let pubkey = coord.coordinate.public_key.to_hex();
                let d_tag = coord.coordinate.identifier.as_str();
                resolve_replaceable(state, kind, &pubkey, d_tag).await
            }
            Nip19::Secret(_) => Err(PreviewError::InvalidIdentifier(
                "secret keys cannot be previewed".to_string(),
            )),
        };
    }

    // Try raw hex: 64-char hex could be an event ID or a pubkey
    if identifier.len() == 64 && identifier.chars().all(|c| c.is_ascii_hexdigit()) {
        // Try as event ID first
        if let Ok(event_id) = EventId::from_hex(identifier)
            && let Ok(Some(_)) = query::fetch_event_by_id(&state.clickhouse, identifier).await
        {
            return resolve_event_by_id(state, &event_id).await;
        }

        // Try as pubkey
        if let Ok(pk) = PublicKey::from_hex(identifier) {
            return resolve_profile(state, &pk).await;
        }
    }

    Err(PreviewError::InvalidIdentifier(format!(
        "'{identifier}' is not a valid NIP-19 identifier or hex event ID/pubkey"
    )))
}

/// Resolve a profile by public key.
async fn resolve_profile(
    state: &AppState,
    pubkey: &PublicKey,
) -> Result<ResolvedContent, PreviewError> {
    let pubkey_hex = pubkey.to_hex();
    let npub = pubkey.to_bech32().unwrap_or_else(|_| pubkey_hex.clone());
    let nprofile = {
        let p = Nip19Profile::new(pubkey.to_owned(), []);
        p.to_bech32().unwrap_or_else(|_| npub.clone())
    };

    // Fetch profile from ClickHouse
    let profile_row = query::fetch_profile(&state.clickhouse, &pubkey_hex).await?;

    let (metadata, profile_event_id) = match profile_row {
        Some(ref row) => (
            ProfileMetadata::from_json(&row.content),
            Some(row.event_id.clone()),
        ),
        None => (ProfileMetadata::default(), None),
    };

    Ok(ResolvedContent::Profile {
        pubkey_hex,
        npub,
        nprofile,
        metadata,
        profile_event_id,
    })
}

/// Resolve an event by its ID.
async fn resolve_event_by_id(
    state: &AppState,
    event_id: &EventId,
) -> Result<ResolvedContent, PreviewError> {
    let id_hex = event_id.to_hex();

    let event = query::fetch_event_by_id(&state.clickhouse, &id_hex)
        .await?
        .ok_or_else(|| PreviewError::NotFound(format!("event {id_hex}")))?;

    build_event_content(state, event).await
}

/// Resolve a replaceable event by coordinate.
async fn resolve_replaceable(
    state: &AppState,
    kind: u16,
    pubkey: &str,
    d_tag: &str,
) -> Result<ResolvedContent, PreviewError> {
    let event = query::fetch_replaceable(&state.clickhouse, kind, pubkey, d_tag)
        .await?
        .ok_or_else(|| {
            PreviewError::NotFound(format!("replaceable event {kind}:{pubkey}:{d_tag}"))
        })?;

    build_event_content(state, event).await
}

/// Build a `ResolvedContent::Event` from an event row, fetching author profile
/// and engagement counts in parallel.
async fn build_event_content(
    state: &AppState,
    event: EventRow,
) -> Result<ResolvedContent, PreviewError> {
    let event_id_hex = event.id.clone();
    let author_pubkey_hex = event.pubkey.clone();

    // Determine if this is a reply and get the parent info
    let reply_parent = find_reply_parent(&event);
    let reply_parent_pubkey = reply_parent.as_ref().and_then(|(pk, _)| pk.clone());
    let reply_to_event_id = reply_parent.as_ref().and_then(|(_, eid)| eid.clone());

    // Fetch author profile, engagement, and reply parent name in parallel
    let (author_profile, engagement, reply_author_name) = tokio::try_join!(
        query::fetch_profile(&state.clickhouse, &author_pubkey_hex),
        query::fetch_engagement(&state.clickhouse, &event_id_hex),
        async {
            if let Some(parent_pk) = &reply_parent_pubkey {
                let names =
                    query::fetch_display_names(&state.clickhouse, std::slice::from_ref(parent_pk))
                        .await?;
                Ok(names.get(parent_pk).cloned())
            } else {
                Ok(None)
            }
        },
    )?;

    let (author_metadata, author_profile_event_id) = match author_profile {
        Some(p) => (
            Some(ProfileMetadata::from_json(&p.content)),
            Some(p.event_id),
        ),
        None => (None, None),
    };

    let author_npub = PublicKey::from_hex(&author_pubkey_hex)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| author_pubkey_hex.clone());

    let nevent = EventId::from_hex(&event_id_hex)
        .ok()
        .and_then(|id| Nip19Event::new(id).to_bech32().ok())
        .unwrap_or_else(|| event_id_hex.clone());

    Ok(ResolvedContent::Event {
        event: Box::new(event),
        author: author_metadata,
        author_profile_event_id,
        author_npub,
        nevent,
        engagement,
        reply_to_author: reply_author_name,
        reply_to_event_id,
    })
}

/// Extract reply parent info (pubkey and event ID) if this is a reply.
///
/// Follows NIP-10 conventions:
/// - Looks for `e` tags with `reply` or `root` marker for the event ID
/// - Falls back to the first `p` tag for the pubkey
fn find_reply_parent(event: &EventRow) -> Option<(Option<String>, Option<String>)> {
    if event.kind != 1 {
        return None;
    }

    let mut parent_event_id = None;
    let mut parent_pubkey = None;

    // Look for `e` tag with `reply` marker first, fall back to `root`
    let mut root_event_id = None;
    for tag in &event.tags {
        if tag.len() >= 4 && tag[0] == "e" {
            if tag[3] == "reply" {
                parent_event_id = Some(tag[1].clone());
                break;
            } else if tag[3] == "root" && root_event_id.is_none() {
                root_event_id = Some(tag[1].clone());
            }
        }
    }
    if parent_event_id.is_none() {
        parent_event_id = root_event_id;
    }

    // If no marked e tag, fall back to last `e` tag (NIP-10 positional)
    if parent_event_id.is_none() {
        for tag in event.tags.iter().rev() {
            if tag.len() >= 2 && tag[0] == "e" && tag[1].len() == 64 {
                parent_event_id = Some(tag[1].clone());
                break;
            }
        }
    }

    // Only treat as a reply if we found an actual `e` tag reference.
    // Notes with only `q` tags (quotes) and `p` tags are not replies.
    parent_event_id.as_ref()?;

    // Get the first `p` tag as the reply-to pubkey
    for tag in &event.tags {
        if tag.len() >= 2 && tag[0] == "p" && tag[1].len() == 64 {
            parent_pubkey = Some(tag[1].clone());
            break;
        }
    }

    Some((parent_pubkey, parent_event_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ToBech32;

    #[test]
    fn test_nip19_npub_roundtrip() {
        // Generate a valid npub from a known hex pubkey
        let hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let pk = PublicKey::from_hex(hex).unwrap();
        let npub = pk.to_bech32().unwrap();

        // Decode it back
        let result = Nip19::from_bech32(&npub);
        assert!(result.is_ok(), "Failed to decode npub: {:?}", result.err());
        match result.unwrap() {
            Nip19::Pubkey(decoded_pk) => {
                assert_eq!(decoded_pk.to_hex(), hex);
            }
            other => panic!("Expected Pubkey, got {:?}", other),
        }
    }

    #[test]
    fn test_nip19_note_roundtrip() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let event_id = EventId::from_hex(hex).unwrap();
        let note = event_id.to_bech32().unwrap();

        let result = Nip19::from_bech32(&note);
        assert!(result.is_ok(), "Failed to decode note: {:?}", result.err());
        match result.unwrap() {
            Nip19::EventId(decoded_id) => {
                assert_eq!(decoded_id.to_hex(), hex);
            }
            other => panic!("Expected EventId, got {:?}", other),
        }
    }

    #[test]
    fn test_hex_identifier_detection() {
        let hex_id = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        assert_eq!(hex_id.len(), 64);
        assert!(hex_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_invalid_identifier_rejected() {
        let result = Nip19::from_bech32("not_a_valid_identifier");
        assert!(result.is_err());
    }

    #[test]
    fn test_nip19_nevent_roundtrip() {
        let hex = "a84c5de86efc2ec2cff7bad077c4171e09146b633b7ad117fffe088d9579ac33";
        let event_id = EventId::from_hex(hex).unwrap();
        let nevent = Nip19Event::new(event_id);
        let bech32 = nevent.to_bech32().unwrap();

        assert!(bech32.starts_with("nevent1"));
        let decoded = Nip19::from_bech32(&bech32).unwrap();
        match decoded {
            Nip19::Event(e) => assert_eq!(e.event_id.to_hex(), hex),
            other => panic!("Expected Event, got {:?}", other),
        }
    }

    #[test]
    fn test_nip19_nprofile_roundtrip() {
        let hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let pk = PublicKey::from_hex(hex).unwrap();
        let nprofile = Nip19Profile::new(pk, []);
        let bech32 = nprofile.to_bech32().unwrap();

        assert!(bech32.starts_with("nprofile1"));
        let decoded = Nip19::from_bech32(&bech32).unwrap();
        match decoded {
            Nip19::Profile(p) => assert_eq!(p.public_key.to_hex(), hex),
            other => panic!("Expected Profile, got {:?}", other),
        }
    }

    #[test]
    fn test_empty_identifier_rejected() {
        let result = Nip19::from_bech32("");
        assert!(result.is_err());
    }

    #[test]
    fn test_garbage_bech32_rejected() {
        let result = Nip19::from_bech32("npub1invalidchecksum");
        assert!(result.is_err());
    }

    #[test]
    fn test_hex_too_short_not_valid() {
        let short_hex = "abcdef";
        assert_ne!(short_hex.len(), 64);
        assert!(short_hex.chars().all(|c| c.is_ascii_hexdigit()));
        // A 6-char hex is not a valid 64-char identifier
    }

    #[test]
    fn test_hex_too_long_not_valid() {
        let long_hex = "a".repeat(128);
        assert_ne!(long_hex.len(), 64);
    }

    #[test]
    fn test_hex_64_chars_valid_format() {
        let hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hex_64_with_uppercase() {
        let hex = "82341F882B6EABCD2BA7F1EF90AAD961CF074AF15B9EF44A09F9D2A8FBFBE6A2";
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hex_64_with_non_hex_chars() {
        let bad = "g2341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        assert_eq!(bad.len(), 64);
        assert!(!bad.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_find_reply_parent_no_tags() {
        let event = EventRow {
            id: String::new(),
            pubkey: String::new(),
            created_at: 0,
            kind: 1,
            content: String::new(),
            tags: vec![],
            d_tag: String::new(),
            title: String::new(),
            thumbnail: String::new(),
            video_url: String::new(),
        };
        assert!(find_reply_parent(&event).is_none());
    }

    #[test]
    fn test_find_reply_parent_p_tag_only_is_not_reply() {
        // A note with only p tags (no e tags) is not a reply — could be a quote
        let pk_hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let event = EventRow {
            id: String::new(),
            pubkey: String::new(),
            created_at: 0,
            kind: 1,
            content: String::new(),
            tags: vec![vec!["p".to_string(), pk_hex.to_string()]],
            d_tag: String::new(),
            title: String::new(),
            thumbnail: String::new(),
            video_url: String::new(),
        };
        assert!(find_reply_parent(&event).is_none());
    }

    #[test]
    fn test_find_reply_parent_with_e_and_p_tags() {
        let pk_hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let eid_hex = "a84c5de86efc2ec2cff7bad077c4171e09146b633b7ad117fffe088d9579ac33";
        let event = EventRow {
            id: String::new(),
            pubkey: String::new(),
            created_at: 0,
            kind: 1,
            content: String::new(),
            tags: vec![
                vec![
                    "e".to_string(),
                    eid_hex.to_string(),
                    "".to_string(),
                    "reply".to_string(),
                ],
                vec!["p".to_string(), pk_hex.to_string()],
            ],
            d_tag: String::new(),
            title: String::new(),
            thumbnail: String::new(),
            video_url: String::new(),
        };
        let result = find_reply_parent(&event);
        assert!(result.is_some());
        let (pubkey, event_id) = result.unwrap();
        assert_eq!(pubkey, Some(pk_hex.to_string()));
        assert_eq!(event_id, Some(eid_hex.to_string()));
    }

    #[test]
    fn test_find_reply_parent_wrong_kind() {
        let pk_hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let event = EventRow {
            id: String::new(),
            pubkey: String::new(),
            created_at: 0,
            kind: 30023,
            content: String::new(),
            tags: vec![vec!["p".to_string(), pk_hex.to_string()]],
            d_tag: String::new(),
            title: String::new(),
            thumbnail: String::new(),
            video_url: String::new(),
        };
        assert!(find_reply_parent(&event).is_none());
    }

    #[test]
    fn test_find_reply_parent_short_p_tag() {
        let event = EventRow {
            id: String::new(),
            pubkey: String::new(),
            created_at: 0,
            kind: 1,
            content: String::new(),
            tags: vec![vec!["p".to_string(), "tooshort".to_string()]],
            d_tag: String::new(),
            title: String::new(),
            thumbnail: String::new(),
            video_url: String::new(),
        };
        assert!(find_reply_parent(&event).is_none());
    }
}
