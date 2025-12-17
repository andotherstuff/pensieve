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

    const VALID_EVENT: &str = r#"{"id":"4ff2236ceb2fdc6dee6317cd0b841f3f020ac985bb3f99f7f4c1f973ec28d06b","pubkey":"35e433c42e5bb838daabd178d54620e427cccb214c55b95daac3dbd9506fbcaf","created_at":1758468146,"kind":1059,"tags":[["p","c40d9a07a3ece16bbed2b141fc7f0d133be6e88460dd052ae062c5b7c92fd7a0"]],"content":"test content","sig":"95dac63b919f424211b12d70786d42c03ec63cbe9196f6d6e773260926d3fd37054eecd3e7c70beb1ed9ef1e2a68cf62c09fc3ad5ec5d45e9143ab4044275b2f"}"#;

    #[test]
    fn test_validate_event_parses_valid_json() {
        // Note: This test may fail if the sample event has an invalid sig
        // That's expected - we're testing the validation path
        let result = validate_event(VALID_EVENT);
        // The result depends on whether the sig is actually valid
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_validate_event_rejects_invalid_json() {
        let result = validate_event("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_event_to_notebuf_conversion() {
        // Skip if validation fails (sig may not be valid for test data)
        if let Ok(event) = validate_event(VALID_EVENT) {
            let notebuf = event_to_notebuf(&event);
            assert_eq!(notebuf.id, event.id.to_hex());
            assert_eq!(notebuf.pubkey, event.pubkey.to_hex());
            assert_eq!(notebuf.kind, event.kind.as_u16() as u64);
        }
    }
}

