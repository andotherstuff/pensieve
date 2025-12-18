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
pub fn decode_length_delimited<R>(reader: &mut R) -> Result<Option<ProtoEvent>>
where
    R: std::io::Read,
{
    decode_length_delimited_with_size(reader).map(|opt| opt.map(|(event, _size)| event))
}

/// Decode a length-delimited ProtoEvent from a reader, also returning bytes read.
///
/// Returns `Ok(None)` on EOF, `Ok(Some((event, bytes_read)))` on success, or an error.
/// The `bytes_read` includes the varint length prefix and the message body.
pub fn decode_length_delimited_with_size<R>(reader: &mut R) -> Result<Option<(ProtoEvent, usize)>>
where
    R: std::io::Read,
{
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
fn read_varint_with_size<R>(reader: &mut R) -> std::io::Result<(u64, usize)>
where
    R: std::io::Read,
{
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
    use std::io::Cursor;

    fn make_proto_event(id: &str, content: &str, kind: i32) -> ProtoEvent {
        ProtoEvent {
            id: id.to_string(),
            pubkey: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            created_at: 1700000000,
            kind,
            tags: vec![],
            content: content.to_string(),
            sig: "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001".to_string(),
        }
    }

    // =========================================================================
    // proto_to_json tests
    // =========================================================================

    #[test]
    fn test_proto_to_json_basic() {
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
        assert!(json.contains("\"pubkey\":\"def456\""));
        assert!(json.contains("\"created_at\":1234567890"));
        assert!(json.contains("\"kind\":1"));
        assert!(json.contains("\"content\":\"Hello, Nostr!\""));
        assert!(json.contains("\"sig\":\"sig789\""));
    }

    #[test]
    fn test_proto_to_json_with_tags() {
        let proto = ProtoEvent {
            id: "test".to_string(),
            pubkey: "test".to_string(),
            created_at: 0,
            kind: 1,
            tags: vec![
                Tag {
                    values: vec!["p".to_string(), "pubkey1".to_string()],
                },
                Tag {
                    values: vec!["e".to_string(), "eventid".to_string(), "relay".to_string()],
                },
            ],
            content: "".to_string(),
            sig: "test".to_string(),
        };

        let json = proto_to_json(&proto).unwrap();
        // Parse and verify tags structure
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let tags = parsed["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0][0], "p");
        assert_eq!(tags[0][1], "pubkey1");
        assert_eq!(tags[1][0], "e");
        assert_eq!(tags[1][1], "eventid");
        assert_eq!(tags[1][2], "relay");
    }

    #[test]
    fn test_proto_to_json_empty_tags() {
        let proto = make_proto_event("test", "content", 1);
        let json = proto_to_json(&proto).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["tags"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_proto_to_json_unicode_content() {
        let proto = ProtoEvent {
            id: "test".to_string(),
            pubkey: "test".to_string(),
            created_at: 0,
            kind: 1,
            tags: vec![],
            content: "Hello 🌍 世界 emoji: 🎉".to_string(),
            sig: "test".to_string(),
        };

        let json = proto_to_json(&proto).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["content"], "Hello 🌍 世界 emoji: 🎉");
    }

    #[test]
    fn test_proto_to_json_special_characters_in_content() {
        let proto = ProtoEvent {
            id: "test".to_string(),
            pubkey: "test".to_string(),
            created_at: 0,
            kind: 1,
            tags: vec![],
            content: "quotes: \" backslash: \\ newline: \n tab: \t".to_string(),
            sig: "test".to_string(),
        };

        let json = proto_to_json(&proto).unwrap();
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["content"].as_str().unwrap().contains("quotes"));
    }

    // =========================================================================
    // decode_proto_event tests
    // =========================================================================

    #[test]
    fn test_decode_proto_event_roundtrip() {
        let proto = ProtoEvent {
            id: "abcd1234".to_string(),
            pubkey: "pubkey123".to_string(),
            created_at: 1700000000,
            kind: 1,
            tags: vec![Tag {
                values: vec!["p".to_string(), "target".to_string()],
            }],
            content: "Hello!".to_string(),
            sig: "signature".to_string(),
        };

        let bytes = proto.encode_to_vec();
        let decoded = decode_proto_event(&bytes).unwrap();

        assert_eq!(decoded.id, proto.id);
        assert_eq!(decoded.pubkey, proto.pubkey);
        assert_eq!(decoded.created_at, proto.created_at);
        assert_eq!(decoded.kind, proto.kind);
        assert_eq!(decoded.content, proto.content);
        assert_eq!(decoded.sig, proto.sig);
        assert_eq!(decoded.tags.len(), 1);
    }

    #[test]
    fn test_decode_proto_event_empty_bytes() {
        let result = decode_proto_event(&[]);
        // Empty protobuf is valid (all fields default)
        assert!(result.is_ok());
    }

    #[test]
    fn test_decode_proto_event_invalid_bytes() {
        // Invalid protobuf bytes (truncated varint)
        let result = decode_proto_event(&[0x80, 0x80, 0x80]);
        assert!(result.is_err());
    }

    // =========================================================================
    // decode_length_delimited tests
    // =========================================================================

    #[test]
    fn test_decode_length_delimited_single_event() {
        let proto = make_proto_event("test1", "content", 1);
        let mut buf = Vec::new();
        proto.encode_length_delimited(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let result = decode_length_delimited(&mut cursor).unwrap();

        assert!(result.is_some());
        let decoded = result.unwrap();
        assert_eq!(decoded.id, "test1");
    }

    #[test]
    fn test_decode_length_delimited_multiple_events() {
        let proto1 = make_proto_event("event1", "content1", 1);
        let proto2 = make_proto_event("event2", "content2", 7);

        let mut buf = Vec::new();
        proto1.encode_length_delimited(&mut buf).unwrap();
        proto2.encode_length_delimited(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);

        // Decode first event
        let result1 = decode_length_delimited(&mut cursor).unwrap();
        assert!(result1.is_some());
        assert_eq!(result1.unwrap().id, "event1");

        // Decode second event
        let result2 = decode_length_delimited(&mut cursor).unwrap();
        assert!(result2.is_some());
        assert_eq!(result2.unwrap().id, "event2");

        // EOF
        let result3 = decode_length_delimited(&mut cursor).unwrap();
        assert!(result3.is_none());
    }

    #[test]
    fn test_decode_length_delimited_eof() {
        let buf: Vec<u8> = Vec::new();
        let mut cursor = Cursor::new(buf);

        let result = decode_length_delimited(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    // =========================================================================
    // decode_length_delimited_with_size tests
    // =========================================================================

    #[test]
    fn test_decode_length_delimited_with_size_returns_bytes() {
        let proto = make_proto_event("sized", "content", 1);
        let mut buf = Vec::new();
        proto.encode_length_delimited(&mut buf).unwrap();
        let total_len = buf.len();

        let mut cursor = Cursor::new(buf);
        let result = decode_length_delimited_with_size(&mut cursor).unwrap();

        assert!(result.is_some());
        let (decoded, bytes_read) = result.unwrap();
        assert_eq!(decoded.id, "sized");
        assert_eq!(bytes_read, total_len);
    }

    #[test]
    fn test_decode_length_delimited_with_size_multiple() {
        let proto1 = make_proto_event("first", "a", 1);
        let proto2 = make_proto_event("second", "longer content here", 1);

        let mut buf = Vec::new();
        proto1.encode_length_delimited(&mut buf).unwrap();
        let first_len = buf.len();
        proto2.encode_length_delimited(&mut buf).unwrap();
        let second_len = buf.len() - first_len;

        let mut cursor = Cursor::new(buf);

        let (_, size1) = decode_length_delimited_with_size(&mut cursor)
            .unwrap()
            .unwrap();
        assert_eq!(size1, first_len);

        let (_, size2) = decode_length_delimited_with_size(&mut cursor)
            .unwrap()
            .unwrap();
        assert_eq!(size2, second_len);
    }

    // =========================================================================
    // validate_proto_event tests (requires valid nostr event)
    // =========================================================================

    #[test]
    fn test_validate_proto_event_rejects_invalid_hex() {
        // Invalid hex in ID field
        let proto = ProtoEvent {
            id: "not_valid_hex".to_string(),
            pubkey: "also_not_valid".to_string(),
            created_at: 0,
            kind: 1,
            tags: vec![],
            content: "".to_string(),
            sig: "invalid_sig".to_string(),
        };

        let result = validate_proto_event(&proto);
        assert!(result.is_err());
    }
}
