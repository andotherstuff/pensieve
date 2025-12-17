//! Protobuf types and conversion utilities.
//!
//! This module provides:
//! - Generated protobuf types from `nostr.proto`
//! - Conversion from `ProtoEvent` to validated `nostr::Event`
//! - Utilities for reading length-delimited protobuf streams

use crate::error::{Error, Result};
use nostr::JsonUtil;
use prost::Message;

// Include the generated protobuf types
pub mod nostr_proto {
    include!(concat!(env!("OUT_DIR"), "/nostr.rs"));
}

pub use nostr_proto::{EventBatch, ProtoEvent, Tag};

/// Convert a ProtoEvent to a validated nostr::Event.
///
/// This performs full NIP-01 validation:
/// - Event ID verification (SHA-256 of canonical JSON)
/// - Signature verification (Schnorr over secp256k1)
///
/// # Errors
///
/// Returns an error if:
/// - The protobuf data has invalid hex strings
/// - Event ID doesn't match computed hash
/// - Signature is invalid
pub fn validate_proto_event(proto: &ProtoEvent) -> Result<nostr::Event> {
    // Convert ProtoEvent to JSON for validation via nostr crate
    // This is the most reliable path since nostr crate validates everything
    let json = proto_to_json(proto)?;
    let event = nostr::Event::from_json(&json)?;
    Ok(event)
}

/// Convert a ProtoEvent to its JSON representation.
///
/// This is useful for validation or interop with JSON-based tools.
pub fn proto_to_json(proto: &ProtoEvent) -> Result<String> {
    let tags: Vec<Vec<&str>> = proto
        .tags
        .iter()
        .map(|t| t.values.iter().map(|s| s.as_str()).collect())
        .collect();

    let json = serde_json::json!({
        "id": proto.id,
        "pubkey": proto.pubkey,
        "created_at": proto.created_at,
        "kind": proto.kind,
        "tags": tags,
        "content": proto.content,
        "sig": proto.sig,
    });

    Ok(json.to_string())
}

/// Decode a ProtoEvent from bytes.
pub fn decode_proto_event(bytes: &[u8]) -> Result<ProtoEvent> {
    ProtoEvent::decode(bytes).map_err(|e| Error::ProtobufDecode(e.to_string()))
}

/// Decode a length-delimited ProtoEvent from a reader.
///
/// Returns `Ok(None)` on EOF, `Ok(Some(event))` on success, or an error.
pub fn decode_length_delimited<R: std::io::Read>(reader: &mut R) -> Result<Option<ProtoEvent>> {
    decode_length_delimited_with_size(reader).map(|opt| opt.map(|(event, _size)| event))
}

/// Decode a length-delimited ProtoEvent from a reader, also returning bytes read.
///
/// Returns `Ok(None)` on EOF, `Ok(Some((event, bytes_read)))` on success, or an error.
/// The `bytes_read` includes the varint length prefix and the message body.
pub fn decode_length_delimited_with_size<R: std::io::Read>(
    reader: &mut R,
) -> Result<Option<(ProtoEvent, usize)>> {
    // Read the varint length prefix
    let (len, varint_size) = match read_varint_with_size(reader) {
        Ok((len, size)) => (len, size),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };

    // Read the message bytes
    let msg_len = len as usize;
    let mut buf = vec![0u8; msg_len];
    reader.read_exact(&mut buf).map_err(Error::Io)?;

    // Decode the protobuf message
    let event = decode_proto_event(&buf)?;
    let total_bytes = varint_size + msg_len;
    Ok(Some((event, total_bytes)))
}

/// Read a varint from a reader, returning the value and number of bytes read.
fn read_varint_with_size<R: std::io::Read>(reader: &mut R) -> std::io::Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut bytes_read = 0;

    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        bytes_read += 1;

        let value = (byte[0] & 0x7F) as u64;
        result |= value << shift;

        if byte[0] & 0x80 == 0 {
            break;
        }

        shift += 7;
        if shift >= 64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "varint too long",
            ));
        }
    }

    Ok((result, bytes_read))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proto_to_json() {
        let proto = ProtoEvent {
            id: "abc123".to_string(),
            pubkey: "def456".to_string(),
            created_at: 1234567890,
            kind: 1,
            tags: vec![Tag {
                values: vec!["p".to_string(), "somepubkey".to_string()],
            }],
            content: "Hello, Nostr!".to_string(),
            sig: "sig789".to_string(),
        };

        let json = proto_to_json(&proto).unwrap();
        assert!(json.contains("\"id\":\"abc123\""));
        assert!(json.contains("\"kind\":1"));
        assert!(json.contains("Hello, Nostr!"));
    }

    #[test]
    fn test_decode_proto_event() {
        let proto = ProtoEvent {
            id: "test".to_string(),
            pubkey: "test".to_string(),
            created_at: 0,
            kind: 1,
            tags: vec![],
            content: "".to_string(),
            sig: "test".to_string(),
        };

        let bytes = proto.encode_to_vec();
        let decoded = decode_proto_event(&bytes).unwrap();
        assert_eq!(decoded.id, "test");
    }
}

