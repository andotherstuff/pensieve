//! Connection guard for relay connections.
//!
//! Provides safety checks before establishing relay connections:
//! - DNS resolution filtering: resolve hostnames and reject private IPs
//! - Per-IP connection deduplication: prevent multiple relays to the same IP
//! - Hard connection cap enforcement: refuse connections beyond `max_relays`
//! - Connection rate limiting: throttle new connections to avoid scan-like behavior
//!
//! # Why This Exists
//!
//! Without these guards, the ingester can trigger automated abuse detection
//! at hosting providers because:
//! - Rapid TCP connections to many IPs looks like port scanning
//! - Hostnames from NIP-65 relay lists can resolve to private/internal IPs (SSRF)
//! - Multiple relay hostnames can point to the same IP, wasting connections
//! - The optimization loop can grow connections beyond `max_relays`

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::net::lookup_host;

use super::url::is_private_ip;

/// Maximum number of connections allowed to a single IP address.
const MAX_CONNECTIONS_PER_IP: usize = 2;

/// Configuration for the connection guard.
#[derive(Debug, Clone)]
pub struct ConnectionGuardConfig {
    /// Maximum number of total concurrent relay connections.
    pub max_relays: usize,

    /// Maximum number of new connections allowed per minute.
    ///
    /// This prevents burst connection patterns that look like port scanning.
    /// A value of 10 means at most 10 new connections per 60-second window.
    pub max_connections_per_minute: usize,

    /// Maximum number of connections to a single IP address.
    ///
    /// Multiple relay hostnames can resolve to the same IP. This prevents
    /// wasting connection slots and reduces the appearance of scanning.
    pub max_per_ip: usize,

    /// Whether to perform DNS resolution checks.
    ///
    /// When enabled, hostnames are resolved before connecting to ensure
    /// they don't point to private/internal IP addresses.
    pub dns_filtering_enabled: bool,
}

impl Default for ConnectionGuardConfig {
    fn default() -> Self {
        Self {
            max_relays: 30,
            max_connections_per_minute: 10,
            max_per_ip: MAX_CONNECTIONS_PER_IP,
            dns_filtering_enabled: true,
        }
    }
}

/// Reason a connection was rejected by the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    /// Connection would exceed the maximum relay count.
    MaxRelaysReached { current: usize, max: usize },
    /// Connection rate limit exceeded.
    RateLimited {
        recent_count: usize,
        max_per_minute: usize,
    },
    /// The relay's hostname resolved to a private/reserved IP.
    PrivateIp { hostname: String, ip: IpAddr },
    /// Too many connections already exist to this IP address.
    TooManyPerIp {
        ip: IpAddr,
        current: usize,
        max: usize,
    },
    /// DNS resolution failed for the hostname.
    DnsResolutionFailed { hostname: String, error: String },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxRelaysReached { current, max } => {
                write!(
                    f,
                    "max relays reached ({}/{}) - not accepting new connections",
                    current, max
                )
            }
            Self::RateLimited {
                recent_count,
                max_per_minute,
            } => {
                write!(
                    f,
                    "rate limited ({} connections in last minute, max {})",
                    recent_count, max_per_minute
                )
            }
            Self::PrivateIp { hostname, ip } => {
                write!(
                    f,
                    "hostname '{}' resolves to private/reserved IP {}",
                    hostname, ip
                )
            }
            Self::TooManyPerIp { ip, current, max } => {
                write!(
                    f,
                    "too many connections to IP {} ({}/{} max)",
                    ip, current, max
                )
            }
            Self::DnsResolutionFailed { hostname, error } => {
                write!(f, "DNS resolution failed for '{}': {}", hostname, error)
            }
        }
    }
}

/// Connection guard that enforces safety checks before relay connections.
///
/// Thread-safe: all internal state is protected by a mutex.
pub struct ConnectionGuard {
    config: ConnectionGuardConfig,
    state: Mutex<GuardState>,
}

/// Internal mutable state for the connection guard.
struct GuardState {
    /// Map of IP address -> set of relay URLs connected to that IP.
    ip_to_relays: HashMap<IpAddr, HashSet<String>>,

    /// Map of relay URL -> resolved IP address.
    relay_to_ip: HashMap<String, IpAddr>,

    /// Timestamps of recent connection attempts (for rate limiting).
    recent_connections: Vec<Instant>,

    /// Current number of active connections.
    active_connections: usize,
}

impl ConnectionGuard {
    /// Create a new connection guard with the given configuration.
    pub fn new(config: ConnectionGuardConfig) -> Self {
        Self {
            config,
            state: Mutex::new(GuardState {
                ip_to_relays: HashMap::new(),
                relay_to_ip: HashMap::new(),
                recent_connections: Vec::new(),
                active_connections: 0,
            }),
        }
    }

    /// Check if a new connection to the given relay URL is allowed.
    ///
    /// This performs synchronous checks (max relays, rate limit, per-IP dedup)
    /// but does NOT perform DNS resolution. Use [`check_and_resolve`] for the
    /// full check including DNS.
    ///
    /// Returns `Ok(())` if the connection is allowed, or `Err(reason)` if rejected.
    pub fn check_sync(&self, relay_url: &str) -> Result<(), RejectionReason> {
        let state = self.state.lock();

        // Check max relays
        if state.active_connections >= self.config.max_relays {
            return Err(RejectionReason::MaxRelaysReached {
                current: state.active_connections,
                max: self.config.max_relays,
            });
        }

        // Check rate limit
        let now = Instant::now();
        let one_minute_ago = now - Duration::from_secs(60);
        let recent_count = state
            .recent_connections
            .iter()
            .filter(|t| **t >= one_minute_ago)
            .count();

        if recent_count >= self.config.max_connections_per_minute {
            return Err(RejectionReason::RateLimited {
                recent_count,
                max_per_minute: self.config.max_connections_per_minute,
            });
        }

        // Check per-IP limit (only if we've already resolved this relay)
        if let Some(ip) = state.relay_to_ip.get(relay_url)
            && let Some(relays) = state.ip_to_relays.get(ip)
            && relays.len() >= self.config.max_per_ip
        {
            return Err(RejectionReason::TooManyPerIp {
                ip: *ip,
                current: relays.len(),
                max: self.config.max_per_ip,
            });
        }

        Ok(())
    }

    /// Check if a connection is allowed, including async DNS resolution.
    ///
    /// This performs all checks:
    /// 1. Max relays cap
    /// 2. Rate limiting
    /// 3. DNS resolution + private IP check
    /// 4. Per-IP connection deduplication
    ///
    /// Returns `Ok(())` if the connection is allowed, or `Err(reason)` if rejected.
    pub async fn check_and_resolve(&self, relay_url: &str) -> Result<(), RejectionReason> {
        // First, do synchronous checks
        self.check_sync(relay_url)?;

        // Skip DNS resolution for .onion addresses (resolved by Tor daemon)
        if relay_url.contains(".onion") {
            return Ok(());
        }

        // Perform DNS resolution if enabled
        if self.config.dns_filtering_enabled {
            let hostname = extract_hostname_for_dns(relay_url);

            // Skip if the hostname is already an IP address literal
            if hostname.parse::<IpAddr>().is_ok() {
                // Already checked by URL blocklist, but double-check
                let ip: IpAddr = hostname.parse().unwrap();
                if is_private_ip(&ip) {
                    return Err(RejectionReason::PrivateIp {
                        hostname: hostname.to_string(),
                        ip,
                    });
                }
                // Check per-IP limit
                return self.check_ip_limit(relay_url, ip);
            }

            // Resolve the hostname
            let resolve_target = format!("{}:443", hostname);
            match lookup_host(&resolve_target).await {
                Ok(addrs) => {
                    let addrs: Vec<_> = addrs.collect();
                    if addrs.is_empty() {
                        return Err(RejectionReason::DnsResolutionFailed {
                            hostname: hostname.to_string(),
                            error: "no addresses returned".to_string(),
                        });
                    }

                    // Check all resolved addresses - reject if ANY is private
                    for addr in &addrs {
                        if is_private_ip(&addr.ip()) {
                            return Err(RejectionReason::PrivateIp {
                                hostname: hostname.to_string(),
                                ip: addr.ip(),
                            });
                        }
                    }

                    // Use the first resolved IP for per-IP dedup
                    let primary_ip = addrs[0].ip();
                    self.check_ip_limit(relay_url, primary_ip)?;

                    // Store the resolved IP mapping
                    {
                        let mut state = self.state.lock();
                        state.relay_to_ip.insert(relay_url.to_string(), primary_ip);
                    }
                }
                Err(e) => {
                    return Err(RejectionReason::DnsResolutionFailed {
                        hostname: hostname.to_string(),
                        error: e.to_string(),
                    });
                }
            }
        }

        Ok(())
    }

    /// Record a successful connection establishment.
    ///
    /// Must be called after a connection is successfully established
    /// to update internal tracking state.
    pub fn record_connect(&self, relay_url: &str) {
        let mut state = self.state.lock();

        state.active_connections += 1;

        // Record timestamp for rate limiting
        state.recent_connections.push(Instant::now());

        // Clean up old timestamps (older than 2 minutes)
        let cutoff = Instant::now() - Duration::from_secs(120);
        state.recent_connections.retain(|t| *t >= cutoff);

        // Update per-IP tracking
        if let Some(ip) = state.relay_to_ip.get(relay_url).copied() {
            state
                .ip_to_relays
                .entry(ip)
                .or_default()
                .insert(relay_url.to_string());
        }
    }

    /// Record a disconnection from a relay.
    ///
    /// Must be called when a relay connection is terminated to update
    /// internal tracking state.
    pub fn record_disconnect(&self, relay_url: &str) {
        let mut state = self.state.lock();

        state.active_connections = state.active_connections.saturating_sub(1);

        // Update per-IP tracking
        if let Some(ip) = state.relay_to_ip.remove(relay_url)
            && let Some(relays) = state.ip_to_relays.get_mut(&ip)
        {
            relays.remove(relay_url);
            if relays.is_empty() {
                state.ip_to_relays.remove(&ip);
            }
        }
    }

    /// Set the active connection count (used for reconciliation).
    pub fn set_active_connections(&self, count: usize) {
        self.state.lock().active_connections = count;
    }

    /// Get the current number of tracked active connections.
    pub fn active_connections(&self) -> usize {
        self.state.lock().active_connections
    }

    /// Get the number of unique IPs currently connected.
    pub fn unique_ips(&self) -> usize {
        self.state.lock().ip_to_relays.len()
    }

    /// Check per-IP connection limit.
    fn check_ip_limit(&self, relay_url: &str, ip: IpAddr) -> Result<(), RejectionReason> {
        let state = self.state.lock();

        if let Some(relays) = state.ip_to_relays.get(&ip) {
            let count = relays.len();

            // Only reject if the relay isn't already tracked AND we're at the limit
            if count >= self.config.max_per_ip && !relays.contains(relay_url) {
                return Err(RejectionReason::TooManyPerIp {
                    ip,
                    current: count,
                    max: self.config.max_per_ip,
                });
            }
        }

        Ok(())
    }
}

/// Extract the hostname from a relay URL for DNS resolution.
///
/// Strips the scheme, port, and path. For IPv6, strips the brackets.
fn extract_hostname_for_dns(url: &str) -> &str {
    // Skip scheme
    let without_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);

    // Take up to path
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);

    // Handle IPv6 [::1]:port
    if host_port.starts_with('[')
        && let Some(bracket_end) = host_port.find(']')
    {
        return &host_port[1..bracket_end];
    }

    // Strip port for IPv4/hostname
    if let Some(colon_pos) = host_port.rfind(':') {
        // Make sure we're not stripping part of an IPv6 address
        let before_colon = &host_port[..colon_pos];
        let after_colon = &host_port[colon_pos + 1..];
        if after_colon.parse::<u16>().is_ok() {
            return before_colon;
        }
    }

    host_port
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_hostname_for_dns() {
        assert_eq!(
            extract_hostname_for_dns("wss://relay.example.com"),
            "relay.example.com"
        );
        assert_eq!(
            extract_hostname_for_dns("wss://relay.example.com:8080"),
            "relay.example.com"
        );
        assert_eq!(
            extract_hostname_for_dns("wss://relay.example.com:8080/path"),
            "relay.example.com"
        );
        assert_eq!(
            extract_hostname_for_dns("ws://relay.example.com"),
            "relay.example.com"
        );
    }

    #[test]
    fn test_max_relays_enforcement() {
        let config = ConnectionGuardConfig {
            max_relays: 2,
            max_connections_per_minute: 100,
            ..Default::default()
        };
        let guard = ConnectionGuard::new(config);

        // First two should be allowed
        assert!(guard.check_sync("wss://relay1.example.com").is_ok());
        guard.record_connect("wss://relay1.example.com");

        assert!(guard.check_sync("wss://relay2.example.com").is_ok());
        guard.record_connect("wss://relay2.example.com");

        // Third should be rejected
        let result = guard.check_sync("wss://relay3.example.com");
        assert!(matches!(
            result,
            Err(RejectionReason::MaxRelaysReached { current: 2, max: 2 })
        ));

        // After disconnect, should be allowed again
        guard.record_disconnect("wss://relay1.example.com");
        assert!(guard.check_sync("wss://relay3.example.com").is_ok());
    }

    #[test]
    fn test_rate_limiting() {
        let config = ConnectionGuardConfig {
            max_relays: 100,
            max_connections_per_minute: 2,
            ..Default::default()
        };
        let guard = ConnectionGuard::new(config);

        // Record two connections
        guard.record_connect("wss://relay1.example.com");
        guard.record_connect("wss://relay2.example.com");

        // Third should be rate limited
        let result = guard.check_sync("wss://relay3.example.com");
        assert!(matches!(result, Err(RejectionReason::RateLimited { .. })));
    }

    #[test]
    fn test_per_ip_dedup() {
        let config = ConnectionGuardConfig {
            max_relays: 100,
            max_connections_per_minute: 100,
            max_per_ip: 2,
            dns_filtering_enabled: false,
        };
        let guard = ConnectionGuard::new(config);

        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // Manually set up IP mappings
        {
            let mut state = guard.state.lock();
            state
                .relay_to_ip
                .insert("wss://relay1.example.com".to_string(), ip);
            state
                .relay_to_ip
                .insert("wss://relay2.example.com".to_string(), ip);
        }

        // Record two connections to same IP
        guard.record_connect("wss://relay1.example.com");
        guard.record_connect("wss://relay2.example.com");

        // Set up a third relay pointing to the same IP
        {
            let mut state = guard.state.lock();
            state
                .relay_to_ip
                .insert("wss://relay3.example.com".to_string(), ip);
        }

        // Third should be rejected (too many per IP)
        let result = guard.check_sync("wss://relay3.example.com");
        assert!(matches!(result, Err(RejectionReason::TooManyPerIp { .. })));
    }

    #[test]
    fn test_disconnect_tracking() {
        let config = ConnectionGuardConfig {
            max_relays: 2,
            max_connections_per_minute: 100,
            ..Default::default()
        };
        let guard = ConnectionGuard::new(config);

        guard.record_connect("wss://relay1.example.com");
        guard.record_connect("wss://relay2.example.com");
        assert_eq!(guard.active_connections(), 2);

        guard.record_disconnect("wss://relay1.example.com");
        assert_eq!(guard.active_connections(), 1);

        guard.record_disconnect("wss://relay2.example.com");
        assert_eq!(guard.active_connections(), 0);

        // Underflow protection
        guard.record_disconnect("wss://nonexistent.example.com");
        assert_eq!(guard.active_connections(), 0);
    }
}
