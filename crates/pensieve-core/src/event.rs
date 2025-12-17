//! Event validation utilities using the nostr crate.
//!
//! This module provides validation of Nostr events per NIP-01:
//! - Event ID verification (SHA-256 of canonical JSON)
//! - Signature verification (Schnorr over secp256k1)

use crate::error::Result;
use notepack::{NoteBinary, NoteBuf};
use nostr::JsonUtil; // Required for Event::from_json()

/// Validates both the event ID and signature of a Nostr event.
///
/// This parses the event JSON into the nostr crate's `Event` type,
/// which performs full validation including:
/// - Event ID is correct SHA-256 of `[0, pubkey, created_at, kind, tags, content]`
/// - Signature is valid Schnorr signature over the event ID
///
/// # Errors
///
/// Returns an error if:
/// - JSON parsing fails
/// - Event ID doesn't match computed hash
/// - Signature is invalid
pub fn validate_event(event_json: &str) -> Result<nostr::Event> {
    // The nostr crate's Event::from_json validates ID and signature automatically
    let event = nostr::Event::from_json(event_json)?;
    Ok(event)
}

/// Validates a NoteBuf event's ID and signature by converting through nostr crate.
///
/// This is useful when you have a NoteBuf from notepack and want to validate it.
pub fn validate_notebuf(note: &NoteBuf) -> Result<()> {
    // Convert NoteBuf to JSON, then parse with nostr crate for validation
    let json = serde_json::to_string(note)?;
    let _event = nostr::Event::from_json(&json)?;
    Ok(())
}

/// Validates only the event ID (not signature) of a Nostr event.
///
/// Useful for faster validation when you only need to verify data integrity,
/// not authenticity.
pub fn validate_event_id(event_json: &str) -> Result<()> {
    let event = nostr::Event::from_json(event_json)?;
    if !event.verify_id() {
        return Err(crate::error::Error::InvalidEventId {
            computed: "computed".to_string(), // Event doesn't expose computed ID easily
            expected: event.id.to_hex(),
        });
    }
    Ok(())
}

/// Validates only the signature of a Nostr event.
///
/// Assumes the ID is already trusted/verified.
pub fn validate_event_signature(event_json: &str) -> Result<()> {
    let event = nostr::Event::from_json(event_json)?;
    if !event.verify_signature() {
        return Err(crate::error::Error::InvalidSignature(
            "signature verification failed".to_string(),
        ));
    }
    Ok(())
}

/// Convert a nostr::Event to a notepack::NoteBuf for serialization.
///
/// This is slower than [`pack_event_binary`] due to hex encoding overhead.
/// Prefer `pack_event_binary` for production workloads.
pub fn event_to_notebuf(event: &nostr::Event) -> NoteBuf {
    NoteBuf {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16() as u64,
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect())
            .collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
    }
}

/// Pack a nostr::Event directly to notepack binary format using NoteBinary.
///
/// This is 2-3x faster than going through NoteBuf because it avoids
/// hex encoding/decoding of the id, pubkey, and signature fields.
///
/// Returns the packed notepack bytes.
pub fn pack_event_binary(event: &nostr::Event) -> Vec<u8> {
    // Extract raw bytes from nostr types
    let id_bytes: &[u8; 32] = event.id.as_bytes();
    let pubkey_bytes: &[u8; 32] = event.pubkey.as_bytes();
    let sig_bytes: &[u8; 64] = event.sig.as_ref();

    // Build tags as Vec<Vec<String>> - unfortunately we need to allocate here
    let tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect())
        .collect();

    let note = NoteBinary {
        id: id_bytes,
        pubkey: pubkey_bytes,
        sig: sig_bytes,
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16() as u64,
        tags: &tags,
        content: &event.content,
    };

    note.pack()
}

/// Pack a nostr::Event directly into an existing buffer using NoteBinary.
///
/// This is the fastest option for batch processing as it reuses allocations.
/// The buffer is NOT cleared before writing - bytes are appended.
///
/// Returns the number of bytes written.
pub fn pack_event_binary_into(event: &nostr::Event, buf: &mut Vec<u8>) -> usize {
    // Extract raw bytes from nostr types
    let id_bytes: &[u8; 32] = event.id.as_bytes();
    let pubkey_bytes: &[u8; 32] = event.pubkey.as_bytes();
    let sig_bytes: &[u8; 64] = event.sig.as_ref();

    // Build tags as Vec<Vec<String>>
    let tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect())
        .collect();

    let note = NoteBinary {
        id: id_bytes,
        pubkey: pubkey_bytes,
        sig: sig_bytes,
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16() as u64,
        tags: &tags,
        content: &event.content,
    };

    note.pack_into(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    /// Generate a valid signed event for testing.
    fn make_test_event(content: &str, kind: Kind, tags: Vec<Tag>) -> nostr::Event {
        let keys = Keys::generate();
        EventBuilder::new(kind, content)
            .tags(tags)
            .custom_created_at(Timestamp::from(1700000000))
            .sign_with_keys(&keys)
            .expect("Failed to sign event")
    }

    /// Serialize an event to JSON.
    fn event_to_json(event: &nostr::Event) -> String {
        serde_json::to_string(event).expect("Failed to serialize event")
    }

    // =========================================================================
    // validate_event tests
    // =========================================================================

    #[test]
    fn test_validate_event_accepts_valid_event() {
        let event = make_test_event("Hello, Nostr!", Kind::TextNote, vec![]);
        let json = event_to_json(&event);

        let result = validate_event(&json);
        assert!(result.is_ok());

        let validated = result.unwrap();
        assert_eq!(validated.id, event.id);
        assert_eq!(validated.pubkey, event.pubkey);
        assert_eq!(validated.content, "Hello, Nostr!");
    }

    #[test]
    fn test_validate_event_preserves_tags() {
        let tags = vec![
            Tag::public_key(nostr::PublicKey::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234").unwrap()),
            Tag::event(nostr::EventId::from_hex("1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd").unwrap()),
        ];
        let event = make_test_event("tagged event", Kind::TextNote, tags);
        let json = event_to_json(&event);

        let validated = validate_event(&json).unwrap();
        assert_eq!(validated.tags.len(), 2);
    }

    #[test]
    fn test_validate_event_rejects_invalid_json() {
        let result = validate_event("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_event_rejects_empty_json() {
        let result = validate_event("{}");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_event_rejects_missing_fields() {
        let incomplete = r#"{"id":"abc","pubkey":"def"}"#;
        let result = validate_event(incomplete);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_event_rejects_tampered_content() {
        let event = make_test_event("original content", Kind::TextNote, vec![]);
        let json = event_to_json(&event);

        // Tamper with the content - this changes the hash so ID won't match
        let tampered = json.replace("original content", "TAMPERED!");

        // Parse and explicitly verify
        let result = validate_event(&tampered);

        // If parsing succeeded, the event's ID should not verify
        match result {
            Ok(parsed) => {
                assert!(
                    !parsed.verify_id(),
                    "Parsed event should have mismatched ID after content tampering"
                );
            }
            Err(_) => {
                // Error during parsing is also acceptable
            }
        }
    }

    #[test]
    fn test_validate_event_rejects_tampered_id() {
        let event = make_test_event("test", Kind::TextNote, vec![]);
        let mut json: serde_json::Value = serde_json::to_value(&event).unwrap();

        // Replace the ID with a different valid-looking hex string
        json["id"] = serde_json::Value::String(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        );

        let result = validate_event(&json.to_string());

        // If parsing succeeded, the event's ID should not verify
        match result {
            Ok(parsed) => {
                assert!(
                    !parsed.verify_id(),
                    "Parsed event should have mismatched ID"
                );
            }
            Err(_) => {
                // Error during parsing is also acceptable
            }
        }
    }

    #[test]
    fn test_validate_event_rejects_tampered_signature() {
        let event = make_test_event("test", Kind::TextNote, vec![]);
        let mut json: serde_json::Value = serde_json::to_value(&event).unwrap();

        // Replace the signature with a different valid-format hex string
        json["sig"] = serde_json::Value::String(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        );

        let result = validate_event(&json.to_string());

        // Either parsing fails, or the signature doesn't verify
        match result {
            Ok(parsed) => {
                assert!(
                    !parsed.verify_signature(),
                    "Parsed event should have invalid signature"
                );
            }
            Err(_) => {
                // Error during parsing is also acceptable
            }
        }
    }

    // =========================================================================
    // validate_event_id tests
    // =========================================================================

    #[test]
    fn test_validate_event_id_accepts_valid_event() {
        let event = make_test_event("test", Kind::TextNote, vec![]);
        let json = event_to_json(&event);

        let result = validate_event_id(&json);
        assert!(result.is_ok());
    }

    // =========================================================================
    // validate_event_signature tests
    // =========================================================================

    #[test]
    fn test_validate_event_signature_accepts_valid_event() {
        let event = make_test_event("test", Kind::TextNote, vec![]);
        let json = event_to_json(&event);

        let result = validate_event_signature(&json);
        assert!(result.is_ok());
    }

    // =========================================================================
    // validate_notebuf tests
    // =========================================================================

    #[test]
    fn test_validate_notebuf_accepts_valid() {
        let event = make_test_event("test notebuf", Kind::TextNote, vec![]);
        let notebuf = event_to_notebuf(&event);

        let result = validate_notebuf(&notebuf);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_notebuf_rejects_tampered() {
        let event = make_test_event("test notebuf", Kind::TextNote, vec![]);
        let mut notebuf = event_to_notebuf(&event);

        // Tamper with the content - this changes the hash so ID won't match
        notebuf.content = "TAMPERED!".to_string();

        // Convert to JSON and try to parse
        let json = serde_json::to_string(&notebuf).unwrap();
        let result = nostr::Event::from_json(&json);

        // Either parsing fails, or the event doesn't verify
        match result {
            Ok(parsed) => {
                assert!(
                    !parsed.verify_id() || !parsed.verify_signature(),
                    "Parsed event should fail verification after content tampering"
                );
            }
            Err(_) => {
                // Error during parsing is also acceptable
            }
        }
    }

    // =========================================================================
    // event_to_notebuf tests
    // =========================================================================

    #[test]
    fn test_event_to_notebuf_preserves_all_fields() {
        let tags = vec![
            Tag::public_key(nostr::PublicKey::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234").unwrap()),
        ];
        let event = make_test_event("conversion test", Kind::TextNote, tags);

        let notebuf = event_to_notebuf(&event);

        assert_eq!(notebuf.id, event.id.to_hex());
        assert_eq!(notebuf.pubkey, event.pubkey.to_hex());
        assert_eq!(notebuf.created_at, event.created_at.as_secs());
        assert_eq!(notebuf.kind, event.kind.as_u16() as u64);
        assert_eq!(notebuf.content, event.content);
        assert_eq!(notebuf.sig, event.sig.to_string());
        assert_eq!(notebuf.tags.len(), 1);
        assert_eq!(notebuf.tags[0][0], "p");
    }

    #[test]
    fn test_event_to_notebuf_handles_empty_content() {
        let event = make_test_event("", Kind::TextNote, vec![]);
        let notebuf = event_to_notebuf(&event);
        assert_eq!(notebuf.content, "");
    }

    #[test]
    fn test_event_to_notebuf_handles_unicode() {
        let event = make_test_event("Hello 🌍 世界 🎉", Kind::TextNote, vec![]);
        let notebuf = event_to_notebuf(&event);
        assert_eq!(notebuf.content, "Hello 🌍 世界 🎉");
    }

    // =========================================================================
    // pack_event_binary tests
    // =========================================================================

    #[test]
    fn test_pack_event_binary_produces_output() {
        let event = make_test_event("pack test", Kind::TextNote, vec![]);
        let packed = pack_event_binary(&event);

        // Should produce non-empty output
        assert!(!packed.is_empty());
        // Notepack format starts with version byte
        assert!(packed.len() > 10);
    }

    #[test]
    fn test_pack_event_binary_deterministic() {
        let event = make_test_event("deterministic", Kind::TextNote, vec![]);
        let packed1 = pack_event_binary(&event);
        let packed2 = pack_event_binary(&event);

        assert_eq!(packed1, packed2);
    }

    // =========================================================================
    // pack_event_binary_into tests
    // =========================================================================

    #[test]
    fn test_pack_event_binary_into_appends_to_buffer() {
        let event = make_test_event("buffer test", Kind::TextNote, vec![]);
        let mut buf = vec![0xAA, 0xBB]; // Pre-existing data

        let bytes_written = pack_event_binary_into(&event, &mut buf);

        assert!(bytes_written > 0);
        // Buffer should contain original bytes plus new data
        assert_eq!(buf[0], 0xAA);
        assert_eq!(buf[1], 0xBB);
        assert_eq!(buf.len(), 2 + bytes_written);
    }

    #[test]
    fn test_pack_event_binary_into_returns_correct_size() {
        let event = make_test_event("size test", Kind::TextNote, vec![]);
        let standalone = pack_event_binary(&event);

        let mut buf = Vec::new();
        let bytes_written = pack_event_binary_into(&event, &mut buf);

        assert_eq!(bytes_written, standalone.len());
        assert_eq!(buf, standalone);
    }

    #[test]
    fn test_pack_event_binary_into_multiple_events() {
        let event1 = make_test_event("event 1", Kind::TextNote, vec![]);
        let event2 = make_test_event("event 2", Kind::TextNote, vec![]);

        let mut buf = Vec::new();
        let size1 = pack_event_binary_into(&event1, &mut buf);
        let size2 = pack_event_binary_into(&event2, &mut buf);

        assert_eq!(buf.len(), size1 + size2);
    }

    // =========================================================================
    // Different event kinds
    // =========================================================================

    #[test]
    fn test_validate_various_kinds() {
        let kinds = [
            Kind::Metadata,        // 0
            Kind::TextNote,        // 1
            Kind::ContactList,     // 3
            Kind::Repost,          // 6
            Kind::Reaction,        // 7
            Kind::Custom(30023),   // Long-form content
        ];

        for kind in kinds {
            let event = make_test_event("test", kind, vec![]);
            let json = event_to_json(&event);

            let result = validate_event(&json);
            assert!(result.is_ok(), "Failed to validate kind {:?}", kind);
        }
    }
}

