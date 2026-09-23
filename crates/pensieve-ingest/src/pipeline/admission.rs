//! Validate untrusted SDK events before dedupe or other archive side effects.

use nostr_sdk::Event;

use crate::{Error, Result};

/// Recompute the ID and verify the signature independently of SDK caches.
pub fn validate_archive_event(event: &Event) -> Result<()> {
    event
        .verify()
        .map_err(|error| Error::Validation(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::DedupeIndex;
    use nostr_sdk::prelude::*;

    #[test]
    fn forged_replays_cannot_claim_valid_event_id() {
        let dir = tempfile::tempdir().unwrap();
        let dedupe = DedupeIndex::open(dir.path()).unwrap();
        let keys = Keys::generate();
        let event = EventBuilder::text_note("valid")
            .sign_with_keys(&keys)
            .unwrap();
        let mut forged = event.clone();
        forged.content = "forged".to_string();
        for _ in 0..2 {
            assert!(validate_archive_event(&forged).is_err());
            assert!(dedupe.is_new(event.id.as_bytes()).unwrap());
        }
        validate_archive_event(&event).unwrap();
        let claim = dedupe.reserve(event.id.as_bytes()).unwrap().unwrap();
        assert!(dedupe.reserve(event.id.as_bytes()).unwrap().is_none());
        assert!(!dedupe.is_new(event.id.as_bytes()).unwrap());
        drop(claim);
        assert!(dedupe.is_new(event.id.as_bytes()).unwrap());
    }

    #[test]
    fn invalid_signature_is_rejected_even_with_correct_id() {
        let keys = Keys::generate();
        let mut event = EventBuilder::text_note("valid")
            .sign_with_keys(&keys)
            .unwrap();
        let other = EventBuilder::text_note("other")
            .sign_with_keys(&keys)
            .unwrap();
        event.sig = other.sig;
        assert!(validate_archive_event(&event).is_err());
    }

    #[test]
    fn pre_admission_unwind_releases_claim_but_writer_ownership_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let dedupe = DedupeIndex::open(dir.path()).unwrap();
        let id = [42; 32];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _claim = dedupe.reserve(&id).unwrap().unwrap();
            panic!("packing failed");
        }));
        assert!(result.is_err());
        assert!(dedupe.is_new(&id).unwrap());
        dedupe.reserve(&id).unwrap().unwrap().retain();
        assert!(!dedupe.is_new(&id).unwrap());
        dedupe.mark_archived(std::iter::once(&id)).unwrap();
        assert!(!dedupe.is_new(&id).unwrap());
    }

    #[test]
    fn segment_open_failure_releases_claim_but_blocks_writer() {
        use crate::pipeline::{PackedEvent, SegmentConfig, SegmentWriter};
        let dir = tempfile::tempdir().unwrap();
        let dedupe = DedupeIndex::open(dir.path().join("dedupe")).unwrap();
        let output = dir.path().join("segments");
        let writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: output.clone(),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        // Replace an empty test-only output directory with a file to force failure.
        std::fs::remove_dir(&output).unwrap();
        std::fs::write(&output, b"not a directory").unwrap();
        let id = [7; 32];
        let claim = dedupe.reserve(&id).unwrap().unwrap();
        assert!(
            writer
                .write_reserved(
                    PackedEvent {
                        event_id: id,
                        created_at: 1,
                        data: vec![1],
                    },
                    claim
                )
                .is_err()
        );
        assert!(dedupe.is_new(&id).unwrap());
        assert!(writer.recovery_required());
        assert!(writer.seal().is_err());
    }
}
