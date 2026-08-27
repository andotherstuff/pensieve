//! Canonical public NIP-65 relay URL normalization shared by analytics and serving.

use std::net::IpAddr;

use nostr::RelayUrl;

/// Normalize one public secure-websocket NIP-65 relay URL.
///
/// The contract trims surrounding whitespace, requires `wss://`, delegates
/// syntax and host canonicalization to `nostr::RelayUrl`, removes trailing
/// slashes/default port notation, and rejects local or non-public IP hosts.
/// Paths and non-default ports remain part of the relay identity.
pub fn normalize_nip65_relay_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if !trimmed.starts_with("wss://")
        || trimmed
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b',' || byte.is_ascii_control())
    {
        return None;
    }
    let parsed = RelayUrl::parse(trimmed).ok()?;
    let mut normalized = parsed.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    let authority = normalized.strip_prefix("wss://")?.split('/').next()?;
    let host = authority_host(authority)?;
    if !is_public_host(host) {
        return None;
    }
    Some(normalized)
}

fn authority_host(authority: &str) -> Option<&str> {
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        return Some(&authority[1..end]);
    }
    Some(
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    )
}

fn is_public_host(host: &str) -> bool {
    if host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".local")
        || host.to_ascii_lowercase().ends_with(".onion")
        || host.to_ascii_lowercase().contains("umbrel")
    {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_unspecified()
                || address.is_multicast()
                || is_cgnat(address.octets())
                || is_documentation_v4(address.octets())
                || address.octets()[0] >= 240)
        }
        Ok(IpAddr::V6(address)) => {
            let segments = address.segments();
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || address.to_ipv4_mapped().is_some())
        }
        Err(_) => true,
    }
}

fn is_cgnat(octets: [u8; 4]) -> bool {
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_documentation_v4(octets: [u8; 4]) -> bool {
    (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_secure_public_relays() {
        assert_eq!(
            normalize_nip65_relay_url("  wss://Relay.Example.COM:443///  "),
            Some("wss://relay.example.com".to_owned())
        );
        assert_eq!(
            normalize_nip65_relay_url("wss://relay.example.com:8080/nostr/"),
            Some("wss://relay.example.com:8080/nostr".to_owned())
        );
    }

    #[test]
    fn rejects_insecure_garbage_and_nonpublic_hosts() {
        for value in [
            "ws://relay.example.com",
            "https://relay.example.com",
            "wss://relay.example.com,evil",
            "wss://localhost",
            "wss://127.0.0.1",
            "wss://10.0.0.1",
            "wss://[::1]",
            "wss://relay.local",
            "wss://hidden.onion",
        ] {
            assert_eq!(normalize_nip65_relay_url(value), None, "{value}");
        }
    }
}
