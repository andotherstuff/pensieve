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
//! - .onion addresses (Tor) - unless `allow_onion` is set
//! - .local addresses (mDNS)
//! - "umbrel" (common home server misconfiguration)

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

    // IPv6 loopback and link-local
    if host.starts_with("[::1]") || host.starts_with("[fe80:") {
        return Some("IPv6 loopback/link-local not allowed".to_string());
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

/// Check if a URL is a Tor .onion address.
pub fn is_onion_url(url: &str) -> bool {
    let url = url.trim().to_lowercase();
    url.contains(".onion")
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

    // Check common blocklist patterns
    if url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("192.168.")
        || url.contains("10.0.")
        || url.contains(".local")
        || url.contains("umbrel")
        || url.contains("[::1]")
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
        // Non-default ports should be preserved
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
}
