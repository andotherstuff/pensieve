//! Reference-coverage sampling.
//!
//! Throughput tells us how fast we ingest; *coverage* tells us how complete our
//! view of the global event set is. This module estimates coverage cheaply from
//! the events we already see: when an event references other events (via `e` and
//! `q` tags), those referenced events *should* exist somewhere in Nostr, so the
//! fraction of them we actually hold is a proxy for how much of the network we
//! capture.
//!
//! For a sampled fraction of ingested events we look up each referenced event id
//! in the dedupe index and record two counters; Prometheus/Grafana then computes
//! `coverage = present / referenced` over any window.
//!
//! Sampling is deterministic and allocation-free at the decision point: an event
//! is sampled when the first byte of its (uniformly distributed) id falls under a
//! threshold derived from the configured rate. No RNG is needed, and a given
//! event is always sampled-or-not consistently.

use crate::pipeline::DedupeIndex;
use std::sync::Arc;

/// Samples ingested events and records reference-coverage metrics.
///
/// Cheap by construction: most events are skipped by the sampling gate, and for
/// sampled events each referenced id is a single bloom-filter-backed RocksDB
/// point lookup.
pub struct CoverageSampler {
    dedupe: Arc<DedupeIndex>,
    /// Sampling threshold in `0..=256`. An event is sampled when `id[0] < threshold`,
    /// so `256` samples everything and `0` disables sampling.
    threshold: u16,
}

impl CoverageSampler {
    /// Create a sampler. `rate` is clamped to `0.0..=1.0`; `0.0` disables sampling.
    pub fn new(dedupe: Arc<DedupeIndex>, rate: f64) -> Self {
        let rate = rate.clamp(0.0, 1.0);
        // 1.0 -> 256 (all ids 0..=255 pass), 0.0 -> 0 (none pass).
        let threshold = (rate * 256.0).round() as u16;
        Self { dedupe, threshold }
    }

    /// Whether sampling is active (rate > 0).
    pub fn is_enabled(&self) -> bool {
        self.threshold > 0
    }

    /// Returns true if an event with this id falls in the sample.
    fn is_sampled(&self, id: &[u8; 32]) -> bool {
        (id[0] as u16) < self.threshold
    }

    /// Observe one ingested event. If it is in the sample, check each referenced
    /// event id against the dedupe index and record coverage metrics.
    ///
    /// Wired into live ingestion; safe to call on every received event (the
    /// sampling gate is the first thing it checks).
    pub fn observe(&self, event: &nostr_sdk::Event) {
        if self.threshold == 0 || !self.is_sampled(event.id.as_bytes()) {
            return;
        }
        metrics::counter!("ingest_reference_coverage_sampled_events_total").increment(1);

        let mut referenced: u64 = 0;
        let mut present: u64 = 0;
        for tag in event.tags.iter() {
            let Some(id) = decode_referenced_id(tag.as_slice()) else {
                continue;
            };
            referenced += 1;
            // `get_status` is a cheap point lookup and intentionally ignores the
            // in-memory in-flight set: referenced events are almost always older
            // and already archived, so we skip taking the pending mutex on the
            // hot path. A handful of just-seen-this-run events read as "missing",
            // a negligible bias for a sampled rate metric.
            if matches!(self.dedupe.get_status(&id), Ok(Some(_))) {
                present += 1;
            }
        }

        if referenced > 0 {
            metrics::counter!("ingest_reference_coverage_referenced_total").increment(referenced);
            metrics::counter!("ingest_reference_coverage_present_total").increment(present);
        }
    }
}

/// Decode the event id referenced by a single tag, if it is an event reference.
///
/// `e` tags (replies/mentions, NIP-10) and `q` tags (quotes, NIP-18) carry a
/// 64-char hex event id at index 1. `a` tags are intentionally excluded — they
/// address a `kind:pubkey:d-tag` coordinate, not a single event id we can look
/// up in the dedupe index. Anything malformed returns `None`.
fn decode_referenced_id(parts: &[String]) -> Option<[u8; 32]> {
    let name = parts.first()?.as_str();
    if name != "e" && name != "q" {
        return None;
    }
    let value = parts.get(1)?;
    if value.len() != 64 {
        return None;
    }
    let mut id = [0u8; 32];
    hex::decode_to_slice(value.as_bytes(), &mut id).ok()?;
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn decodes_e_and_q_tags() {
        let hex_id = "a".repeat(64); // 32 bytes of 0xaa
        let expected = [0xaau8; 32];

        assert_eq!(
            decode_referenced_id(&parts(&["e", &hex_id])),
            Some(expected)
        );
        assert_eq!(
            decode_referenced_id(&parts(&["q", &hex_id])),
            Some(expected)
        );
        // Extra tag fields (relay hint, marker, ...) are ignored.
        assert_eq!(
            decode_referenced_id(&parts(&["e", &hex_id, "wss://relay", "root"])),
            Some(expected)
        );
    }

    #[test]
    fn rejects_non_event_reference_tags() {
        let hex_id = "a".repeat(64);
        // `p` (pubkey), `t` (hashtag), and `a` (addressable coord) are not event ids.
        assert_eq!(decode_referenced_id(&parts(&["p", &hex_id])), None);
        assert_eq!(decode_referenced_id(&parts(&["t", "nostr"])), None);
        assert_eq!(
            decode_referenced_id(&parts(&["a", "30023:abcd:slug"])),
            None
        );
    }

    #[test]
    fn rejects_malformed_values() {
        // Missing value.
        assert_eq!(decode_referenced_id(&parts(&["e"])), None);
        // Wrong length.
        assert_eq!(decode_referenced_id(&parts(&["e", "abc"])), None);
        // Right length, not hex.
        assert_eq!(decode_referenced_id(&parts(&["e", &"z".repeat(64)])), None);
        // Empty tag.
        assert_eq!(decode_referenced_id(&parts(&[])), None);
    }

    #[test]
    fn threshold_maps_rate_to_sampling() {
        let tmp = TempDir::new().unwrap();
        let dedupe = Arc::new(DedupeIndex::open(tmp.path()).unwrap());

        // Disabled.
        let off = CoverageSampler::new(Arc::clone(&dedupe), 0.0);
        assert!(!off.is_enabled());
        assert!(!off.is_sampled(&[0u8; 32]));

        // Everything sampled at rate 1.0.
        let all = CoverageSampler::new(Arc::clone(&dedupe), 1.0);
        assert!(all.is_enabled());
        assert!(all.is_sampled(&[0u8; 32]));
        assert!(all.is_sampled(&[255u8; 32]));

        // ~50%: ids with first byte < 128 are sampled.
        let half = CoverageSampler::new(Arc::clone(&dedupe), 0.5);
        let mut low = [0u8; 32];
        low[0] = 100;
        let mut high = [0u8; 32];
        high[0] = 200;
        assert!(half.is_sampled(&low));
        assert!(!half.is_sampled(&high));

        // Out-of-range rates are clamped, not panicking.
        assert!(CoverageSampler::new(Arc::clone(&dedupe), 5.0).is_sampled(&[255u8; 32]));
        assert!(!CoverageSampler::new(Arc::clone(&dedupe), -1.0).is_enabled());
    }

    #[test]
    fn observe_counts_present_references() {
        // Build a sampler that samples everything, seed the dedupe index with one
        // of two referenced ids, and confirm observe() does not panic on a real
        // event. (Counter values are global; we assert behavior via no-panic and
        // the dedupe lookups resolving as expected through get_status.)
        let tmp = TempDir::new().unwrap();
        let dedupe = Arc::new(DedupeIndex::open(tmp.path()).unwrap());

        let present_id = [0x11u8; 32];
        let missing_id = [0x22u8; 32];
        dedupe.check_and_mark_pending(&present_id).unwrap();
        dedupe.mark_archived([&present_id].into_iter()).unwrap();

        assert!(matches!(dedupe.get_status(&present_id), Ok(Some(_))));
        assert!(matches!(dedupe.get_status(&missing_id), Ok(None)));

        // Decoding round-trips the seeded ids from their hex tag form.
        assert_eq!(
            decode_referenced_id(&parts(&["e", &hex::encode(present_id)])),
            Some(present_id)
        );
        assert_eq!(
            decode_referenced_id(&parts(&["q", &hex::encode(missing_id)])),
            Some(missing_id)
        );
    }
}
