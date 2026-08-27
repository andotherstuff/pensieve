//! Exact deterministic semantics for current NIP-65 relay distribution.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use pensieve_core::relay_url::normalize_nip65_relay_url;
use serde::{Deserialize, Serialize};

/// Stable current-state product version.
pub const RELAY_DISTRIBUTION_VERSION: &str = "relay-distribution-current-v1";

/// Canonical identity used to choose one latest kind-10002 event per pubkey.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelayListIdentity {
    /// Event creation time in Unix seconds.
    pub created_at: u64,
    /// Canonical event ID used as the deterministic equal-time tie-breaker.
    pub event_id: [u8; 32],
}

impl Ord for RelayListIdentity {
    fn cmp(&self, other: &Self) -> Ordering {
        self.created_at
            .cmp(&other.created_at)
            .then_with(|| self.event_id.cmp(&other.event_id))
    }
}

impl PartialOrd for RelayListIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Deduplicated membership contributed by one winning NIP-65 event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayMembership {
    /// Canonical public secure-websocket URL.
    pub relay_url: String,
    /// The event permits reading from this relay.
    pub read: bool,
    /// The event permits writing to this relay.
    pub write: bool,
}

/// One final relay distribution row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RelayDistributionRow {
    /// Canonical relay URL.
    pub relay_url: String,
    /// Unique winning pubkeys that list the relay in any mode.
    pub user_count: u64,
    /// Unique winning pubkeys that list the relay for reads.
    pub read_count: u64,
    /// Unique winning pubkeys that list the relay for writes.
    pub write_count: u64,
}

/// Return whether `candidate` deterministically replaces `current`.
pub fn relay_list_wins(candidate: RelayListIdentity, current: RelayListIdentity) -> bool {
    candidate > current
}

/// Normalize and deduplicate `r` tags from one winning NIP-65 event.
///
/// Missing/empty markers mean both read and write. `read` and `write` enable
/// their respective mode. Unknown markers retain membership/user-count
/// attribution but enable neither mode, matching the legacy query contract.
pub fn relay_memberships(tags: &[Vec<String>]) -> Vec<RelayMembership> {
    let mut by_url: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for tag in tags {
        if tag.first().map(String::as_str) != Some("r") {
            continue;
        }
        let Some(url) = tag.get(1).and_then(|url| normalize_nip65_relay_url(url)) else {
            continue;
        };
        let marker = tag.get(2).map(String::as_str).unwrap_or("");
        let modes = match marker {
            "" => (true, true),
            "read" => (true, false),
            "write" => (false, true),
            _ => (false, false),
        };
        let combined = by_url.entry(url).or_default();
        combined.0 |= modes.0;
        combined.1 |= modes.1;
    }
    by_url
        .into_iter()
        .map(|(relay_url, (read, write))| RelayMembership {
            relay_url,
            read,
            write,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn latest_identity_uses_event_id_for_equal_timestamps() {
        let lower = RelayListIdentity {
            created_at: 100,
            event_id: [1; 32],
        };
        let higher = RelayListIdentity {
            created_at: 100,
            event_id: [2; 32],
        };
        let later = RelayListIdentity {
            created_at: 101,
            event_id: [0; 32],
        };
        assert!(relay_list_wins(higher, lower));
        assert!(relay_list_wins(later, higher));
        assert!(!relay_list_wins(lower, higher));
    }

    #[test]
    fn markers_expand_and_duplicate_urls_union_without_double_membership() {
        let memberships = relay_memberships(&[
            tag(&["r", "wss://Relay.Example.com/", "read"]),
            tag(&["r", "wss://relay.example.com:443", "write"]),
            tag(&["r", "wss://unknown.example", "other"]),
            tag(&["r", "wss://both.example"]),
            tag(&["p", "wss://ignored.example"]),
            tag(&["r", "ws://insecure.example"]),
        ]);
        assert_eq!(
            memberships,
            vec![
                RelayMembership {
                    relay_url: "wss://both.example".to_owned(),
                    read: true,
                    write: true,
                },
                RelayMembership {
                    relay_url: "wss://relay.example.com".to_owned(),
                    read: true,
                    write: true,
                },
                RelayMembership {
                    relay_url: "wss://unknown.example".to_owned(),
                    read: false,
                    write: false,
                },
            ]
        );
    }

    #[test]
    fn first_two_tag_positions_are_exact_and_case_sensitive() {
        assert!(relay_memberships(&[tag(&["R", "wss://relay.example"])]).is_empty());
        assert_eq!(
            relay_memberships(&[tag(&["r", "wss://relay.example", "READ"])]),
            vec![RelayMembership {
                relay_url: "wss://relay.example".to_owned(),
                read: false,
                write: false,
            }]
        );
    }
}
