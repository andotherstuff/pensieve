//! NIP-66 relay catalog consumer.
//!
//! Monitors publish kind-`30166` "relay discovery" events describing relays they
//! probe: supported NIPs, network type, round-trip times, requirements, geohash.
//! These events arrive through the normal firehose (the relay source subscribes
//! to all kinds), and this module parses them into [`RelayCatalogEntry`] rows that
//! the relay manager persists — keyed by `(relay_url, monitor_pubkey)` so multiple
//! monitors are aggregated rather than trusted individually (NIP-66 advises against
//! trusting a single monitor).
//!
//! The catalog is the foundation for non-graph relay discovery and dynamic
//! negentropy targeting (relays advertising NIP-77 via [`RelayCatalogEntry::supports_nip`]).
//!
//! See <https://github.com/nostr-protocol/nips/blob/master/66.md>.

/// Kind for NIP-66 relay discovery events.
pub const KIND_RELAY_DISCOVERY: u16 = 30166;
/// Kind for NIP-66 relay monitor announcements.
pub const KIND_RELAY_MONITOR: u16 = 10166;

/// A relay's characteristics as reported by one monitor's kind-`30166` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCatalogEntry {
    /// Normalized relay URL (the `d` tag).
    pub relay_url: String,
    /// Hex pubkey of the monitor that reported this.
    pub monitor_pubkey: String,
    /// Network type (`clearnet`, `tor`, `i2p`, `loki`), from the `n` tag.
    pub network: Option<String>,
    /// NIPs the relay supports, from repeated `N` tags. Kept as raw strings since
    /// some NIP identifiers are alphanumeric (e.g. `C7`).
    pub supported_nips: Vec<String>,
    /// Relay requirements (`auth`, `payment`, ...; negated values are prefixed `!`),
    /// from repeated `R` tags.
    pub requirements: Vec<String>,
    /// Round-trip time in ms for opening a connection (`rtt-open`).
    pub rtt_open: Option<u32>,
    /// Round-trip time in ms for a read (`rtt-read`).
    pub rtt_read: Option<u32>,
    /// Round-trip time in ms for a write (`rtt-write`).
    pub rtt_write: Option<u32>,
    /// NIP-52 geohash, from the `g` tag.
    pub geohash: Option<String>,
    /// The event's `created_at` — when the monitor observed the relay.
    pub observed_at: u64,
}

impl RelayCatalogEntry {
    /// Whether the relay advertises support for the given NIP (e.g. `"77"` for negentropy).
    pub fn supports_nip(&self, nip: &str) -> bool {
        self.supported_nips.iter().any(|n| n == nip)
    }
}

/// Parse a NIP-66 kind-`30166` relay discovery event into a catalog entry.
///
/// Returns `None` if the event is not a relay discovery event or lacks a `d` tag.
pub fn parse_relay_discovery(event: &nostr_sdk::Event) -> Option<RelayCatalogEntry> {
    if event.kind.as_u16() != KIND_RELAY_DISCOVERY {
        return None;
    }
    build_entry(
        event.pubkey.to_hex(),
        event.created_at.as_secs(),
        event.tags.iter().map(|t| t.as_slice()),
    )
}

/// Build a catalog entry from a monitor pubkey, timestamp, and the event's tags.
///
/// Split out from [`parse_relay_discovery`] so the tag logic is unit-testable
/// without constructing a signed event.
fn build_entry<'a>(
    monitor_pubkey: String,
    observed_at: u64,
    tags: impl Iterator<Item = &'a [String]>,
) -> Option<RelayCatalogEntry> {
    let mut relay_url: Option<String> = None;
    let mut network = None;
    let mut supported_nips = Vec::new();
    let mut requirements = Vec::new();
    let mut rtt_open = None;
    let mut rtt_read = None;
    let mut rtt_write = None;
    let mut geohash = None;

    for parts in tags {
        let Some(name) = parts.first().map(String::as_str) else {
            continue;
        };
        let value = parts.get(1).map(String::as_str);
        match name {
            "d" => {
                if let Some(v) = value.filter(|v| !v.is_empty()) {
                    relay_url = Some(v.to_string());
                }
            }
            "n" => network = value.map(str::to_string),
            "N" => {
                if let Some(v) = value {
                    supported_nips.push(v.to_string());
                }
            }
            "R" => {
                if let Some(v) = value {
                    requirements.push(v.to_string());
                }
            }
            "rtt-open" => rtt_open = value.and_then(|v| v.parse().ok()),
            "rtt-read" => rtt_read = value.and_then(|v| v.parse().ok()),
            "rtt-write" => rtt_write = value.and_then(|v| v.parse().ok()),
            "g" => geohash = value.map(str::to_string),
            _ => {}
        }
    }

    Some(RelayCatalogEntry {
        relay_url: relay_url?,
        monitor_pubkey,
        network,
        supported_nips,
        requirements,
        rtt_open,
        rtt_read,
        rtt_write,
        geohash,
        observed_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn parse(rows: &[&[&str]]) -> Option<RelayCatalogEntry> {
        let t = tags(rows);
        build_entry(
            "monitor_pk".to_string(),
            1_700_000_000,
            t.iter().map(Vec::as_slice),
        )
    }

    #[test]
    fn parses_a_full_discovery_event() {
        let entry = parse(&[
            &["d", "wss://some.relay/"],
            &["n", "clearnet"],
            &["N", "1"],
            &["N", "77"],
            &["R", "!payment"],
            &["R", "auth"],
            &["g", "ww8p1r4t8"],
            &["rtt-open", "234"],
            &["rtt-read", "12"],
        ])
        .expect("entry");

        assert_eq!(entry.relay_url, "wss://some.relay/");
        assert_eq!(entry.monitor_pubkey, "monitor_pk");
        assert_eq!(entry.network.as_deref(), Some("clearnet"));
        assert_eq!(entry.supported_nips, vec!["1", "77"]);
        assert_eq!(entry.requirements, vec!["!payment", "auth"]);
        assert_eq!(entry.geohash.as_deref(), Some("ww8p1r4t8"));
        assert_eq!(entry.rtt_open, Some(234));
        assert_eq!(entry.rtt_read, Some(12));
        assert_eq!(entry.rtt_write, None);
        assert_eq!(entry.observed_at, 1_700_000_000);
    }

    #[test]
    fn supports_nip_checks_the_n_tags() {
        let entry = parse(&[&["d", "wss://r/"], &["N", "77"]]).unwrap();
        assert!(entry.supports_nip("77")); // negentropy
        assert!(!entry.supports_nip("50"));
    }

    #[test]
    fn missing_d_tag_yields_none() {
        assert!(parse(&[&["n", "clearnet"], &["N", "1"]]).is_none());
        assert!(parse(&[&["d"]]).is_none()); // d present but no value
        assert!(parse(&[&["d", ""]]).is_none()); // empty value
    }

    #[test]
    fn bad_rtt_is_ignored() {
        let entry = parse(&[&["d", "wss://r/"], &["rtt-open", "not-a-number"]]).unwrap();
        assert_eq!(entry.rtt_open, None);
    }

    #[test]
    fn alphanumeric_nips_are_preserved() {
        let entry = parse(&[&["d", "wss://r/"], &["N", "C7"]]).unwrap();
        assert!(entry.supports_nip("C7"));
    }
}
