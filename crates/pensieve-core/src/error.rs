//! Error types for the Pensieve ingestion pipeline.

use thiserror::Error;

/// Result type alias using the crate's error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during event processing.
#[derive(Error, Debug)]
pub enum Error {
    /// Event ID validation failed - computed ID doesn't match claimed ID.
    #[error("invalid event ID: computed {computed}, expected {expected}")]
    InvalidEventId {
        /// The ID we computed by hashing the event.
        computed: String,
        /// The ID claimed in the event.
        expected: String,
    },

    /// Event signature is invalid.
    #[error("invalid event signature: {0}")]
    InvalidSignature(String),

    /// Event has an invalid field format (e.g., wrong hex length).
    #[error("invalid field '{field}': {reason}")]
    InvalidField {
        /// The name of the invalid field.
        field: &'static str,
        /// Description of what's wrong.
        reason: String,
    },

    /// JSON parsing error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Notepack encoding/decoding error.
    #[error("notepack error: {0}")]
    Notepack(#[from] notepack::Error),

    /// Nostr library error (for crypto operations).
    #[error("nostr error: {0}")]
    Nostr(#[from] nostr::event::Error),

    /// Hex decoding error.
    #[error("hex decode error: {0}")]
    HexDecode(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Protobuf decoding error.
    #[error("protobuf decode error: {0}")]
    ProtobufDecode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Error Display formatting tests
    // =========================================================================

    #[test]
    fn test_invalid_event_id_display() {
        let err = Error::InvalidEventId {
            computed: "abc123".to_string(),
            expected: "def456".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("abc123"));
        assert!(msg.contains("def456"));
        assert!(msg.contains("invalid event ID"));
    }

    #[test]
    fn test_invalid_signature_display() {
        let err = Error::InvalidSignature("verification failed".to_string());
        let msg = err.to_string();
        assert!(msg.contains("invalid event signature"));
        assert!(msg.contains("verification failed"));
    }

    #[test]
    fn test_invalid_field_display() {
        let err = Error::InvalidField {
            field: "pubkey",
            reason: "not 64 hex characters".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("pubkey"));
        assert!(msg.contains("not 64 hex characters"));
    }

    #[test]
    fn test_hex_decode_display() {
        let err = Error::HexDecode("invalid character 'g'".to_string());
        let msg = err.to_string();
        assert!(msg.contains("hex decode error"));
        assert!(msg.contains("invalid character"));
    }

    #[test]
    fn test_protobuf_decode_display() {
        let err = Error::ProtobufDecode("unexpected EOF".to_string());
        let msg = err.to_string();
        assert!(msg.contains("protobuf decode error"));
        assert!(msg.contains("unexpected EOF"));
    }

    // =========================================================================
    // Error From conversions
    // =========================================================================

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_from_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("not valid json").unwrap_err();
        let err: Error = json_err.into();
        assert!(matches!(err, Error::Json(_)));
        assert!(err.to_string().contains("JSON error"));
    }

    // =========================================================================
    // Error Debug formatting
    // =========================================================================

    #[test]
    fn test_error_debug_format() {
        let err = Error::InvalidEventId {
            computed: "abc".to_string(),
            expected: "def".to_string(),
        };
        let debug = format!("{:?}", err);
        // Debug format should include variant name and fields
        assert!(debug.contains("InvalidEventId"));
        assert!(debug.contains("abc"));
        assert!(debug.contains("def"));
    }

    // =========================================================================
    // Result type alias
    // =========================================================================

    #[test]
    fn test_result_type_ok() {
        let result: Result<i32> = Ok(42);
        assert!(result.is_ok());
        assert!(matches!(result, Ok(42)));
    }

    #[test]
    fn test_result_type_err() {
        let result: Result<i32> = Err(Error::HexDecode("bad hex".to_string()));
        assert!(result.is_err());
    }
}
