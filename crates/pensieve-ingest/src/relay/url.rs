//! Relay URL normalization and validation.
//!
//! This module provides utilities for normalizing relay URLs to prevent
//! duplicates caused by trailing slashes, case differences, or other
//! cosmetic variations.
//!
//! # Normalization Rules
//!
//! - Remove trailing slashes
//! - Lowercase the scheme and host
//! - Preserve port numbers and paths
//! - Validate websocket scheme (wss:// or ws://)
//!
//! # Filtering Rules
//!
//! URLs are rejected if they contain:
//! - localhost or 127.0.0.1
//! - Private IP ranges (192.168.x.x, 10.x.x.x, 172.16-31.x.x)
//! - CGNAT/shared address space (100.64-127.x.x)
//! - Link-local IPv4 (169.254.x.x)
//! - 0.0.0.0
//! - IPv6 loopback (::1), link-local (fe80:), unique local (fc00::/fd00:),
//!   IPv4-mapped (::ffff:)
//! - .onion addresses (Tor) - unless `allow_onion` is set
//! - .local addresses (mDNS)
//! - "umbrel" (common home server misconfiguration)
//! - Non-standard ports (only 80, 443, and common relay ports allowed)

use nostr_sdk::RelayUrl;

/// Options for URL normalization.
#[derive(Debug, Clone, Default)]
pub struct NormalizeOptions {
    /// Allow .onion (Tor hidden service) addresses.
    ///
    /// When false (default), .onion addresses are blocked.
    /// Set to true when a Tor proxy is configured.
    pub allow_onion: bool,
}

/// Result of URL normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeResult {
    /// URL is valid and normalized.
    Ok(String),
    /// URL is syntactically invalid.
    Invalid(String),
    /// URL matches a blocklist pattern.
    Blocked(String),
}

impl NormalizeResult {
    /// Returns the normalized URL if valid.
    pub fn ok(self) -> Option<String> {
        match self {
            Self::Ok(url) => Some(url),
            _ => None,
        }
    }

    /// Returns true if the URL is valid.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }
}

/// Normalize a relay URL.
///
/// Applies normalization rules:
/// 1. Parse and validate with nostr-sdk's RelayUrl
/// 2. Remove trailing slashes
/// 3. Lowercase scheme and host
/// 4. Check against blocklist patterns
///
/// This function blocks .onion addresses by default. Use
/// [`normalize_relay_url_with_opts`] to allow them when a Tor proxy is configured.
///
/// # Examples
///
/// ```ignore
/// use pensieve_ingest::relay::url::normalize_relay_url;
///
/// assert_eq!(
///     normalize_relay_url("wss://Relay.Example.COM/").ok(),
///     Some("wss://relay.example.com".to_string())
/// );
///
/// assert!(normalize_relay_url("wss://localhost:8080").ok().is_none());
/// ```
pub fn normalize_relay_url(url: &str) -> NormalizeResult {
    normalize_relay_url_with_opts(url, &NormalizeOptions::default())
}

/// Normalize a relay URL with custom options.
///
/// Use this function when you need to allow .onion addresses (e.g., when
/// a Tor proxy is configured).
///
/// # Examples
///
/// ```ignore
/// use pensieve_ingest::relay::url::{normalize_relay_url_with_opts, NormalizeOptions};
///
/// // Allow .onion addresses
/// let opts = NormalizeOptions { allow_onion: true };
/// assert!(normalize_relay_url_with_opts("ws://relay.onion", &opts).is_ok());
/// ```
pub fn normalize_relay_url_with_opts(url: &str, opts: &NormalizeOptions) -> NormalizeResult {
    let url = url.trim();

    // Quick check for websocket scheme
    if !url.starts_with("wss://") && !url.starts_with("ws://") {
        return NormalizeResult::Invalid("URL must start with wss:// or ws://".to_string());
    }

    // Parse with nostr-sdk to validate structure
    let parsed = match RelayUrl::parse(url) {
        Ok(u) => u,
        Err(e) => return NormalizeResult::Invalid(format!("Invalid relay URL: {}", e)),
    };

    // Convert to string and normalize
    let mut normalized = parsed.to_string();

    // Remove trailing slashes
    while normalized.ends_with('/') {
        normalized.pop();
    }

    // Check blocklist patterns
    if let Some(reason) = check_blocklist(&normalized, opts) {
        return NormalizeResult::Blocked(reason);
    }

    NormalizeResult::Ok(normalized)
}

/// Ports that are allowed for relay connections.
///
/// Only standard WebSocket ports and common relay ports are permitted.
/// This prevents the ingester from looking like a port scanner to network
/// monitoring systems.
const ALLOWED_PORTS: &[u16] = &[
    80,   // HTTP (ws://)
    443,  // HTTPS (wss://) - default, usually omitted from URL
    8080, // Common HTTP alt
    8443, // Common HTTPS alt
    8008, // Common relay port
    8880, // Common alt
    3000, // Dev/alt relay port
    4848, // Some relays use this
    7777, // Some relays use this
    9735, // Some relays use this (Lightning-related)
];

/// Check if a URL matches any blocklist pattern.
///
/// Returns `Some(reason)` if blocked, `None` if allowed.
fn check_blocklist(url: &str, opts: &NormalizeOptions) -> Option<String> {
    // Extract host portion for checking
    let host = extract_host(url);

    // Localhost
    if host == "localhost" || host.starts_with("localhost:") {
        return Some("localhost not allowed".to_string());
    }

    // 0.0.0.0 (unspecified address)
    if host.starts_with("0.0.0.0") {
        return Some("unspecified address (0.0.0.0) not allowed".to_string());
    }

    // Loopback IPv4
    if host.starts_with("127.") {
        return Some("loopback address not allowed".to_string());
    }

    // Private IPv4 ranges
    if host.starts_with("192.168.") {
        return Some("private IP (192.168.x.x) not allowed".to_string());
    }
    if host.starts_with("10.") {
        return Some("private IP (10.x.x.x) not allowed".to_string());
    }
    // 172.16.0.0 - 172.31.255.255
    if host.starts_with("172.")
        && let Some(second_octet) = host.split('.').nth(1)
        && let Ok(n) = second_octet.parse::<u8>()
        && (16..=31).contains(&n)
    {
        return Some("private IP (172.16-31.x.x) not allowed".to_string());
    }

    // CGNAT / Shared address space (100.64.0.0 - 100.127.255.255, RFC 6598)
    if host.starts_with("100.")
        && let Some(second_octet) = host.split('.').nth(1)
        && let Ok(n) = second_octet.parse::<u8>()
        && (64..=127).contains(&n)
    {
        return Some("CGNAT/shared address (100.64-127.x.x) not allowed".to_string());
    }

    // Link-local IPv4 (169.254.0.0/16)
    if host.starts_with("169.254.") {
        return Some("link-local address (169.254.x.x) not allowed".to_string());
    }

    // IPv6 loopback
    if host.starts_with("[::1]") {
        return Some("IPv6 loopback (::1) not allowed".to_string());
    }

    // IPv6 link-local (fe80::/10)
    if host.starts_with("[fe80:") {
        return Some("IPv6 link-local (fe80::) not allowed".to_string());
    }

    // IPv6 unique local addresses (fc00::/7 = fc00:: through fdff::)
    if host.starts_with("[fc") || host.starts_with("[fd") {
        return Some("IPv6 unique local (fc00::/7) not allowed".to_string());
    }

    // IPv4-mapped IPv6 (::ffff:x.x.x.x) - could bypass IPv4 blocklist
    if host.starts_with("[::ffff:") {
        return Some("IPv4-mapped IPv6 (::ffff:) not allowed".to_string());
    }

    // .onion (Tor hidden services) - only block if not allowed
    if !opts.allow_onion && (host.ends_with(".onion") || host.contains(".onion:")) {
        return Some(".onion addresses not allowed (enable Tor proxy to allow)".to_string());
    }

    // .local (mDNS/Bonjour)
    if host.ends_with(".local") || host.contains(".local:") {
        return Some(".local addresses not allowed".to_string());
    }

    // Common home server patterns
    if host.contains("umbrel") {
        return Some("umbrel addresses not allowed".to_string());
    }

    // Empty or malformed hosts
    if host.is_empty() || host == ":" {
        return Some("empty host not allowed".to_string());
    }

    // URLs that are too short to be valid
    if host.len() < 3 {
        return Some("host too short".to_string());
    }

    // Port filtering: only allow standard and common relay ports.
    // Non-standard ports make the ingester look like a port scanner.
    // Note: .onion addresses skip port filtering (Tor relays often use odd ports).
    if (!opts.allow_onion || !host.contains(".onion"))
        && let Some(port) = extract_port(url)
        && !ALLOWED_PORTS.contains(&port)
    {
        return Some(format!("non-standard port {} not allowed", port));
    }

    None
}

/// Extract the host portion from a websocket URL.
fn extract_host(url: &str) -> &str {
    // Skip scheme
    let without_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);

    // Take up to path or end
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

/// Extract the port number from a websocket URL, if explicitly specified.
///
/// Returns `None` if no port is specified (uses default: 80 for ws, 443 for wss).
fn extract_port(url: &str) -> Option<u16> {
    let host = extract_host(url);

    // Handle IPv6 addresses like [::1]:8080
    if let Some(bracket_end) = host.rfind(']') {
        // IPv6: look for port after the closing bracket
        let after_bracket = &host[bracket_end + 1..];
        if let Some(port_str) = after_bracket.strip_prefix(':') {
            return port_str.parse().ok();
        }
        return None;
    }

    // IPv4 or hostname: look for last colon
    if let Some(colon_pos) = host.rfind(':') {
        let port_str = &host[colon_pos + 1..];
        return port_str.parse().ok();
    }

    None
}

/// Check if a URL is a Tor .onion address.
pub fn is_onion_url(url: &str) -> bool {
    let url = url.trim().to_lowercase();
    url.contains(".onion")
}

/// Check if an IP address is private, reserved, or otherwise not suitable
/// for public relay connections.
///
/// This is used for post-DNS-resolution filtering to catch hostnames that
/// resolve to private/internal addresses (SSRF protection).
pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_loopback()                      // 127.0.0.0/8
            || ipv4.is_unspecified()                 // 0.0.0.0
            || ipv4.is_broadcast()                   // 255.255.255.255
            || ipv4.is_link_local()                  // 169.254.0.0/16
            || is_ipv4_private(*ipv4)                // 10/8, 172.16/12, 192.168/16
            || is_ipv4_cgnat(*ipv4)                  // 100.64.0.0/10
            || is_ipv4_documentation(*ipv4)          // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
            || is_ipv4_benchmarking(*ipv4)           // 198.18.0.0/15
            || ipv4.is_multicast()                   // 224.0.0.0/4
            || is_ipv4_reserved(*ipv4) // 240.0.0.0/4 (except broadcast)
        }
        IpAddr::V6(ipv6) => {
            ipv6.is_loopback()                       // ::1
            || ipv6.is_unspecified()                  // ::
            || ipv6.is_multicast()                    // ff00::/8
            || is_ipv6_link_local(*ipv6)              // fe80::/10
            || is_ipv6_unique_local(*ipv6)            // fc00::/7
            || is_ipv6_ipv4_mapped(*ipv6)             // ::ffff:0:0/96
            || is_ipv6_documentation(*ipv6) // 2001:db8::/32
        }
    }
}

// IPv4 helper functions

fn is_ipv4_private(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 10.0.0.0/8
    octets[0] == 10
    // 172.16.0.0/12
    || (octets[0] == 172 && (16..=31).contains(&octets[1]))
    // 192.168.0.0/16
    || (octets[0] == 192 && octets[1] == 168)
}

fn is_ipv4_cgnat(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 100.64.0.0/10 (100.64.0.0 - 100.127.255.255)
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_ipv4_documentation(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 192.0.2.0/24 (TEST-NET-1)
    (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
    // 198.51.100.0/24 (TEST-NET-2)
    || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
    // 203.0.113.0/24 (TEST-NET-3)
    || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

fn is_ipv4_benchmarking(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 198.18.0.0/15 (198.18.0.0 - 198.19.255.255)
    octets[0] == 198 && (18..=19).contains(&octets[1])
}

fn is_ipv4_reserved(ip: std::net::Ipv4Addr) -> bool {
    // 240.0.0.0/4 (240.0.0.0 - 255.255.255.255, but not broadcast 255.255.255.255)
    ip.octets()[0] >= 240
}

// IPv6 helper functions

fn is_ipv6_link_local(ip: std::net::Ipv6Addr) -> bool {
    // fe80::/10
    let segments = ip.segments();
    (segments[0] & 0xffc0) == 0xfe80
}

fn is_ipv6_unique_local(ip: std::net::Ipv6Addr) -> bool {
    // fc00::/7 (fc00:: through fdff::)
    let segments = ip.segments();
    (segments[0] & 0xfe00) == 0xfc00
}

fn is_ipv6_ipv4_mapped(ip: std::net::Ipv6Addr) -> bool {
    // ::ffff:0:0/96
    let segments = ip.segments();
    segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0xffff
}

fn is_ipv6_documentation(ip: std::net::Ipv6Addr) -> bool {
    // 2001:db8::/32
    let segments = ip.segments();
    segments[0] == 0x2001 && segments[1] == 0x0db8
}

/// Check if a URL is obviously invalid without full normalization.
///
/// This is a fast path for rejecting clearly bad URLs.
/// Note: This blocks .onion by default. Use `is_obviously_invalid_with_opts`
/// if you need to allow .onion addresses.
pub fn is_obviously_invalid(url: &str) -> bool {
    is_obviously_invalid_with_opts(url, &NormalizeOptions::default())
}

/// Check if a URL is obviously invalid, with options.
pub fn is_obviously_invalid_with_opts(url: &str, opts: &NormalizeOptions) -> bool {
    let url = url.trim().to_lowercase();

    // Must be websocket
    if !url.starts_with("wss://") && !url.starts_with("ws://") {
        return true;
    }

    // Check common blocklist patterns (fast string checks)
    if url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("192.168.")
        || url.contains("10.0.")
        || url.contains("0.0.0.0")
        || url.contains("169.254.")
        || url.contains(".local")
        || url.contains("umbrel")
        || url.contains("[::1]")
        || url.contains("[fe80:")
        || url.contains("[fc")
        || url.contains("[fd")
        || url.contains("[::ffff:")
    {
        return true;
    }

    // .onion is only invalid if not allowed
    if !opts.allow_onion && url.contains(".onion") {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_trailing_slash() {
        assert_eq!(
            normalize_relay_url("wss://relay.example.com/").ok(),
            Some("wss://relay.example.com".to_string())
        );
        assert_eq!(
            normalize_relay_url("wss://relay.example.com///").ok(),
            Some("wss://relay.example.com".to_string())
        );
    }

    #[test]
    fn test_normalize_preserves_path() {
        // Paths should be preserved (minus trailing slash)
        let result = normalize_relay_url("wss://relay.example.com/nostr");
        assert_eq!(
            result.ok(),
            Some("wss://relay.example.com/nostr".to_string())
        );
    }

    #[test]
    fn test_normalize_preserves_port() {
        // Note: Port 443 is the default for wss:// so it's normalized away by nostr_sdk
        assert_eq!(
            normalize_relay_url("wss://relay.example.com:443/").ok(),
            Some("wss://relay.example.com".to_string())
        );
        // Allowed non-default ports should be preserved
        assert_eq!(
            normalize_relay_url("wss://relay.example.com:8080/").ok(),
            Some("wss://relay.example.com:8080".to_string())
        );
    }

    #[test]
    fn test_block_localhost() {
        assert!(matches!(
            normalize_relay_url("wss://localhost:8080"),
            NormalizeResult::Blocked(_)
        ));
        assert!(matches!(
            normalize_relay_url("wss://127.0.0.1:8080"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_block_private_ips() {
        assert!(matches!(
            normalize_relay_url("wss://192.168.1.1:8080"),
            NormalizeResult::Blocked(_)
        ));
        assert!(matches!(
            normalize_relay_url("wss://10.0.0.1:8080"),
            NormalizeResult::Blocked(_)
        ));
        assert!(matches!(
            normalize_relay_url("wss://172.16.0.1:8080"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_block_cgnat() {
        assert!(matches!(
            normalize_relay_url("wss://100.64.0.1:443"),
            NormalizeResult::Blocked(_)
        ));
        assert!(matches!(
            normalize_relay_url("wss://100.127.255.255:443"),
            NormalizeResult::Blocked(_)
        ));
        // 100.63.x.x should NOT be blocked (not CGNAT)
        assert!(normalize_relay_url("wss://100.63.0.1").is_ok());
    }

    #[test]
    fn test_block_link_local_ipv4() {
        assert!(matches!(
            normalize_relay_url("wss://169.254.1.1:443"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_block_zero_address() {
        assert!(matches!(
            normalize_relay_url("wss://0.0.0.0:443"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_block_ipv6_unique_local() {
        assert!(is_obviously_invalid("wss://[fc00::1]:443"));
        assert!(is_obviously_invalid("wss://[fd12:3456::1]:443"));
    }

    #[test]
    fn test_block_ipv4_mapped_ipv6() {
        assert!(is_obviously_invalid("wss://[::ffff:192.168.1.1]:443"));
    }

    #[test]
    fn test_block_onion() {
        assert!(matches!(
            normalize_relay_url("wss://something.onion"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_block_local() {
        assert!(matches!(
            normalize_relay_url("wss://myserver.local"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_block_non_standard_ports() {
        // Non-standard port should be blocked
        assert!(matches!(
            normalize_relay_url("wss://relay.example.com:31337"),
            NormalizeResult::Blocked(_)
        ));
        assert!(matches!(
            normalize_relay_url("wss://relay.example.com:12345"),
            NormalizeResult::Blocked(_)
        ));
        assert!(matches!(
            normalize_relay_url("ws://relay.example.com:1234"),
            NormalizeResult::Blocked(_)
        ));
    }

    #[test]
    fn test_allow_standard_ports() {
        // Standard and common relay ports should be allowed
        assert!(normalize_relay_url("wss://relay.example.com").is_ok()); // default 443
        assert!(normalize_relay_url("wss://relay.example.com:8080").is_ok());
        assert!(normalize_relay_url("wss://relay.example.com:8443").is_ok());
        assert!(normalize_relay_url("ws://relay.example.com:80").is_ok());
        assert!(normalize_relay_url("ws://relay.example.com:3000").is_ok());
    }

    #[test]
    fn test_invalid_scheme() {
        assert!(matches!(
            normalize_relay_url("https://relay.example.com"),
            NormalizeResult::Invalid(_)
        ));
        assert!(matches!(
            normalize_relay_url("relay.example.com"),
            NormalizeResult::Invalid(_)
        ));
    }

    #[test]
    fn test_valid_relays() {
        // Common real relays should work
        assert!(normalize_relay_url("wss://relay.damus.io").is_ok());
        assert!(normalize_relay_url("wss://nos.lol").is_ok());
        assert!(normalize_relay_url("wss://relay.primal.net").is_ok());
        assert!(normalize_relay_url("wss://purplepag.es").is_ok());
    }

    #[test]
    fn test_is_obviously_invalid() {
        assert!(is_obviously_invalid("https://example.com"));
        assert!(is_obviously_invalid("wss://localhost:8080"));
        assert!(is_obviously_invalid("wss://192.168.1.1"));
        assert!(is_obviously_invalid("wss://0.0.0.0"));
        assert!(is_obviously_invalid("wss://169.254.1.1"));
        assert!(!is_obviously_invalid("wss://relay.damus.io"));
    }

    #[test]
    fn test_allow_onion_with_opts() {
        let opts = NormalizeOptions { allow_onion: true };

        // With allow_onion = true, .onion addresses should be allowed
        let result = normalize_relay_url_with_opts(
            "ws://nostrnetl6yd5whkldj3vqsxyyaq3tkuspy23a3qgx7cdepb4564qgqd.onion",
            &opts,
        );
        assert!(result.is_ok());

        // But without, they should be blocked
        let result_blocked = normalize_relay_url(
            "ws://nostrnetl6yd5whkldj3vqsxyyaq3tkuspy23a3qgx7cdepb4564qgqd.onion",
        );
        assert!(matches!(result_blocked, NormalizeResult::Blocked(_)));
    }

    #[test]
    fn test_is_onion_url() {
        assert!(is_onion_url("ws://something.onion"));
        assert!(is_onion_url("wss://relay.onion/path"));
        assert!(!is_onion_url("wss://relay.damus.io"));
    }

    #[test]
    fn test_is_obviously_invalid_with_opts() {
        let opts = NormalizeOptions { allow_onion: true };

        // With allow_onion, .onion should NOT be invalid
        assert!(!is_obviously_invalid_with_opts("ws://relay.onion", &opts));

        // Without, it should be invalid
        assert!(is_obviously_invalid("ws://relay.onion"));
    }

    #[test]
    fn test_extract_port() {
        assert_eq!(extract_port("wss://relay.example.com"), None);
        assert_eq!(extract_port("wss://relay.example.com:8080"), Some(8080));
        assert_eq!(extract_port("wss://relay.example.com:443"), Some(443));
        assert_eq!(extract_port("ws://relay.example.com:80"), Some(80));
        assert_eq!(
            extract_port("wss://relay.example.com:8080/path"),
            Some(8080)
        );
    }

    #[test]
    fn test_is_private_ip() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        // Private IPv4
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::BROADCAST)));

        // CGNAT
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 255
        ))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(100, 63, 0, 1))));

        // Link-local
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));

        // Public IPv4 should NOT be private
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));

        // IPv6 private ranges
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));

        // Public IPv6 should NOT be private
        assert!(!is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2607, 0xf8b0, 0x4004, 0x800, 0, 0, 0, 0x200e
        ))));
    }
}
