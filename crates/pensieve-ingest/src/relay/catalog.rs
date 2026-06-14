//! NIP-66 relay catalog consumer.
//!
//! Monitors publish kind-`30166` "relay discovery" events describing relays they
//! probe: supported NIPs, network type, round-trip times, requirements, geohash.
//! These events arrive through the normal firehose (the relay source subscribes
//! to all kinds), and this module parses them into [`RelayCatalogEntry`] rows that
//! the relay manager persists — keyed by `(relay_id, monitor_pubkey)` so multiple
//! monitors are aggregated rather than trusted individually (NIP-66 advises against
//! trusting a single monitor).
//!
//! The `d` tag is validated, not trusted: URL-shaped values are normalized and
//! filtered through the shared relay URL normalizer (so trailing-slash/case
//! variants collapse and localhost/private/blocked URLs are rejected), and the
//! NIP-66 alternative of a bare hex pubkey (for relays with no URL) is modeled
//! explicitly via [`RelayId`]. Anything else is dropped.
//!
//! The catalog is the foundation for non-graph relay discovery and dynamic
//! negentropy targeting (relays advertising NIP-77 via [`RelayCatalogEntry::supports_nip`]).
//!
//! See <https://github.com/nostr-protocol/nips/blob/master/66.md>.

/// Kind for NIP-66 relay discovery events.
pub const KIND_RELAY_DISCOVERY: u16 = 30166;
/// Kind for NIP-66 relay monitor announcements.
pub const KIND_RELAY_MONITOR: u16 = 10166;

/// Identifier for a relay from a NIP-66 `d` tag.
///
/// NIP-66 says the `d` tag is the relay's normalized URL, or a hex pubkey for
/// relays not addressable by URL. We model both explicitly so callers can tell a
/// connectable URL apart from an opaque pubkey id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayId {
    /// A normalized relay websocket URL (validated via the shared normalizer).
    Url(String),
    /// A 64-char lowercase hex pubkey, for relays without a URL.
    Pubkey(String),
}

impl RelayId {
    /// The underlying string value (normalized URL or hex pubkey).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Url(s) | Self::Pubkey(s) => s,
        }
    }

    /// `"url"` or `"pubkey"` — stored alongside the id so queries can filter to
    /// connectable URL relays.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Url(_) => "url",
            Self::Pubkey(_) => "pubkey",
        }
    }
}

/// A relay's characteristics as reported by one monitor's kind-`30166` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCatalogEntry {
    /// Relay identifier (normalized URL or hex pubkey), from the `d` tag.
    pub relay_id: RelayId,
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
/// Returns `None` if the event is not a relay discovery event, lacks a `d` tag,
/// or the `d` tag is neither a valid relay URL nor a hex pubkey.
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

/// Classify and validate a `d`-tag value into a [`RelayId`].
///
/// URL-shaped values go through the shared relay URL normalizer (rejecting
/// invalid/blocked/private URLs and collapsing cosmetic variants); otherwise a
/// 64-char hex string is accepted as a pubkey id. Anything else returns `None`.
fn classify_relay_id(d: &str) -> Option<RelayId> {
    if d.starts_with("ws://") || d.starts_with("wss://") {
        return super::url::normalize_relay_url(d).ok().map(RelayId::Url);
    }
    if is_hex64(d) {
        return Some(RelayId::Pubkey(d.to_ascii_lowercase()));
    }
    None
}

/// Whether `s` is exactly 64 hexadecimal characters (a Nostr pubkey).
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
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
    let mut raw_d: Option<String> = None;
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
                    raw_d = Some(v.to_string());
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

    let relay_id = classify_relay_id(&raw_d?)?;

    Some(RelayCatalogEntry {
        relay_id,
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
    fn parses_a_full_discovery_event_and_normalizes_url() {
        // Mixed-case host + trailing slash exercises URL normalization.
        let entry = parse(&[
            &["d", "wss://Some.Relay/"],
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

        assert_eq!(entry.relay_id, RelayId::Url("wss://some.relay".to_string()));
        assert_eq!(entry.relay_id.kind(), "url");
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
    fn url_variants_normalize_to_the_same_id() {
        let a = parse(&[&["d", "wss://Relay.Example.COM/"]]).unwrap();
        let b = parse(&[&["d", "wss://relay.example.com"]]).unwrap();
        assert_eq!(a.relay_id, b.relay_id);
        assert_eq!(
            a.relay_id,
            RelayId::Url("wss://relay.example.com".to_string())
        );
    }

    #[test]
    fn supports_nip_checks_the_n_tags() {
        let entry = parse(&[&["d", "wss://relay.example.com"], &["N", "77"]]).unwrap();
        assert!(entry.supports_nip("77")); // negentropy
        assert!(!entry.supports_nip("50"));
    }

    #[test]
    fn accepts_pubkey_d_tag_for_url_less_relays() {
        let pk = "a".repeat(64);
        let entry = parse(&[&["d", &pk.to_uppercase()]]).unwrap();
        assert_eq!(entry.relay_id, RelayId::Pubkey(pk)); // stored lowercase
        assert_eq!(entry.relay_id.kind(), "pubkey");
    }

    #[test]
    fn rejects_missing_or_empty_d() {
        assert!(parse(&[&["n", "clearnet"], &["N", "1"]]).is_none());
        assert!(parse(&[&["d"]]).is_none());
        assert!(parse(&[&["d", ""]]).is_none());
    }

    #[test]
    fn rejects_invalid_blocked_and_non_url_ids() {
        assert!(parse(&[&["d", "wss://localhost:8080"]]).is_none()); // blocked
        assert!(parse(&[&["d", "http://relay.example.com"]]).is_none()); // not ws
        assert!(parse(&[&["d", "relay.example.com"]]).is_none()); // no scheme, not hex
        assert!(parse(&[&["d", "not a url"]]).is_none());
    }

    #[test]
    fn bad_rtt_is_ignored() {
        let entry = parse(&[&["d", "wss://relay.example.com"], &["rtt-open", "nope"]]).unwrap();
        assert_eq!(entry.rtt_open, None);
    }

    #[test]
    fn alphanumeric_nips_are_preserved() {
        let entry = parse(&[&["d", "wss://relay.example.com"], &["N", "C7"]]).unwrap();
        assert!(entry.supports_nip("C7"));
    }
}
