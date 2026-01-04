//! Live relay event source adapter.
//!
//! Connects to Nostr relays and streams events in real-time using nostr-sdk.
//!
//! # Architecture
//!
//! The relay source uses nostr-sdk's `Client` to manage connections to multiple
//! relays. Events are validated by nostr-sdk before being passed to the handler.
//!
//! # Relay Discovery
//!
//! When `discovery_enabled` is true, the source parses NIP-65 kind:10002 events
//! to discover new relay URLs and automatically connects to them (up to `max_relays`).
//!
//! # Quality-Based Optimization
//!
//! When a `RelayManager` is provided, the source tracks connection events and
//! periodically optimizes which relays are connected based on quality scores.

use super::{EventSource, SourceMetadata, SourceStats};
use crate::pipeline::PackedEvent;
use crate::relay::RelayManager;
use crate::{Error, Result};

use chrono::DateTime;
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Configuration for the relay source.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Initial relay URLs to connect to.
    pub seed_relays: Vec<String>,

    /// Enable relay discovery via NIP-65 (kind:10002 events).
    pub discovery_enabled: bool,

    /// Maximum number of concurrent relay connections.
    pub max_relays: usize,

    /// Relay URLs to never connect to.
    pub blocklist: HashSet<String>,

    /// Connection timeout per relay.
    pub connection_timeout: Duration,

    /// Whether to subscribe to all events or use filters.
    /// For archival purposes, we want all events.
    pub subscribe_all: bool,

    /// How often to run the optimization loop (swap low/high-scoring relays).
    /// Set to Duration::ZERO to disable optimization.
    pub optimization_interval: Duration,

    /// Optional timestamp to request events from (for catch-up after restart).
    ///
    /// When set, subscriptions will use a `since` filter to request events
    /// with `created_at` >= this timestamp. This allows the ingester to
    /// catch up on events missed during downtime.
    ///
    /// The dedupe index will handle any duplicates from the overlap window.
    pub since_timestamp: Option<u64>,

    /// Optional SOCKS5 proxy address for connecting to Tor (.onion) relays.
    ///
    /// When set, .onion relay addresses will be routed through this proxy.
    /// Typically this is the Tor daemon at `127.0.0.1:9050`.
    ///
    /// Clearnet relays (wss://...) will continue to connect directly.
    pub tor_proxy: Option<SocketAddr>,

    /// Size of the notification channel buffer.
    ///
    /// When running negentropy sync alongside live ingestion, the notification
    /// channel can overflow if the receiver can't keep up with all the relay
    /// activity. Increase this value to reduce `Lagged` warnings.
    ///
    /// Default: 16384 (4x the nostr-sdk default of 4096)
    pub notification_channel_size: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            seed_relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.nostr.band".to_string(),
                "wss://nos.lol".to_string(),
                "wss://relay.snort.social".to_string(),
                "wss://purplepag.es".to_string(),
                "wss://relay.primal.net".to_string(),
                "wss://nostr.wine".to_string(),
                "wss://relay.nostr.bg".to_string(),
            ],
            discovery_enabled: true,
            max_relays: 30,
            blocklist: HashSet::new(),
            connection_timeout: Duration::from_secs(30),
            subscribe_all: true,
            optimization_interval: Duration::from_secs(300), // 5 minutes
            since_timestamp: None,
            tor_proxy: None,
            notification_channel_size: 131072, // 32x nostr-sdk default (4096) - generous buffer for heavy sync
        }
    }
}

/// Live relay event source.
///
/// Uses nostr-sdk to manage connections to multiple relays and stream
/// events in real-time.
pub struct RelaySource {
    config: RelayConfig,
    /// Known relay URLs (seed + discovered).
    known_relays: Arc<RwLock<HashSet<String>>>,
    /// Running flag for graceful shutdown.
    running: Arc<AtomicBool>,
    /// Statistics counters.
    stats: Arc<RelayStats>,
    /// Optional relay manager for quality tracking and optimization.
    relay_manager: Option<Arc<RelayManager>>,
    /// Connection start times for tracking uptime (relay_url -> connected_at).
    connection_times: Arc<RwLock<HashMap<String, Instant>>>,
}

/// Internal statistics for the relay source.
struct RelayStats {
    total_events: AtomicUsize,
    valid_events: AtomicUsize,
    invalid_events: AtomicUsize,
    relays_connected: AtomicUsize,
    relays_discovered: AtomicUsize,
}

impl Default for RelayStats {
    fn default() -> Self {
        Self {
            total_events: AtomicUsize::new(0),
            valid_events: AtomicUsize::new(0),
            invalid_events: AtomicUsize::new(0),
            relays_connected: AtomicUsize::new(0),
            relays_discovered: AtomicUsize::new(0),
        }
    }
}

impl RelaySource {
    /// Create a new relay source with the given configuration.
    pub fn new(config: RelayConfig) -> Self {
        let known_relays: HashSet<String> = config.seed_relays.iter().cloned().collect();

        Self {
            config,
            known_relays: Arc::new(RwLock::new(known_relays)),
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(RelayStats::default()),
            relay_manager: None,
            connection_times: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new relay source with quality tracking via a `RelayManager`.
    ///
    /// When a manager is provided:
    /// - Connection attempts and disconnections are recorded
    /// - The optimization loop periodically swaps low-scoring relays for higher-scoring ones
    pub fn with_manager(config: RelayConfig, relay_manager: Arc<RelayManager>) -> Self {
        let known_relays: HashSet<String> = config.seed_relays.iter().cloned().collect();

        Self {
            config,
            known_relays: Arc::new(RwLock::new(known_relays)),
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(RelayStats::default()),
            relay_manager: Some(relay_manager),
            connection_times: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &RelayConfig {
        &self.config
    }

    /// Signal the source to stop processing.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Check if the source is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Record a successful connection to a relay.
    async fn record_connection(&self, relay_url: &str) {
        // Track connection time for uptime calculation
        self.connection_times
            .write()
            .await
            .insert(relay_url.to_string(), Instant::now());

        // Record in relay manager
        if let Some(ref manager) = self.relay_manager {
            manager.record_connection_attempt(relay_url, true);
        }
    }

    /// Record a failed connection attempt.
    fn record_connection_failure(&self, relay_url: &str) {
        if let Some(ref manager) = self.relay_manager {
            manager.record_connection_attempt(relay_url, false);
        }
    }

    /// Record a disconnection from a relay.
    async fn record_disconnection(&self, relay_url: &str) {
        // Calculate connected duration
        let connected_seconds = {
            let mut times = self.connection_times.write().await;
            times
                .remove(relay_url)
                .map(|start| start.elapsed().as_secs())
                .unwrap_or(0)
        };

        // Record in relay manager
        if let Some(ref manager) = self.relay_manager {
            manager.record_disconnect(relay_url, connected_seconds);
        }
    }

    /// Reconcile relay status by checking actual connection state from nostr-sdk.
    ///
    /// This is necessary because `connect_relay()` returns Ok immediately (async),
    /// but the actual WebSocket connection may fail later. This function checks
    /// the real status of each relay and updates the database accordingly.
    ///
    /// Uses `reconcile_status` which only updates when there's a mismatch and
    /// doesn't double-count failures for already-idle relays.
    async fn reconcile_relay_status(&self, client: &Client) {
        use nostr_sdk::RelayStatus;

        let manager = match &self.relay_manager {
            Some(m) => m,
            None => return,
        };

        let relays = client.relays().await;
        let mut connected_count: u64 = 0;
        let mut updates: u64 = 0;

        for (url, relay) in relays {
            let url_str = url.to_string();
            let status = relay.status();

            let is_connected = matches!(status, RelayStatus::Connected);

            if is_connected {
                connected_count += 1;
            }

            // Only record for connected/disconnected states, not transitional ones
            if matches!(
                status,
                RelayStatus::Connected | RelayStatus::Disconnected | RelayStatus::Terminated
            ) {
                match manager.reconcile_status(&url_str, is_connected) {
                    Ok(true) => updates += 1,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::debug!("Failed to reconcile status for {}: {}", url_str, e);
                    }
                }
            }
        }

        // Update the connected count gauge
        self.stats.relays_connected.store(
            connected_count as usize,
            std::sync::atomic::Ordering::Relaxed,
        );

        if updates > 0 {
            tracing::debug!(
                "Status reconciliation: {} connected, {} status updates",
                connected_count,
                updates
            );
        }
    }

    /// Run the relay source asynchronously.
    ///
    /// This is the main async implementation that the sync `process` method calls.
    /// The handler receives `(relay_url, &Event)` so the caller can do dedupe checks
    /// **before** expensive packing, and attribute events to their source relay.
    ///
    /// # Performance Note
    ///
    /// The handler receives a reference to the raw `Event` rather than a `PackedEvent`.
    /// This allows deduplication checks to happen before the expensive serialization
    /// step, avoiding wasted work on duplicate events (~90% of incoming traffic).
    pub async fn run_async<F>(&self, mut handler: F) -> Result<SourceStats>
    where
        F: FnMut(String, &Event) -> Result<bool>,
    {
        self.running.store(true, Ordering::SeqCst);

        tracing::info!(
            "Starting relay source with {} seed relays, discovery={}",
            self.config.seed_relays.len(),
            self.config.discovery_enabled
        );

        // Generate a random keypair for NIP-42 authentication
        // This allows us to authenticate with relays that require it
        // We don't use this key for signing events, just for relay auth
        let keys = Keys::generate();
        tracing::info!(
            "Generated ephemeral keypair for relay auth: {}",
            keys.public_key()
                .to_bech32()
                .unwrap_or_else(|_| "unknown".to_string())
        );

        // Build client options with larger notification buffer and optional Tor proxy
        let pool_opts = RelayPoolOptions::default()
            .notification_channel_size(self.config.notification_channel_size);

        let mut client_opts = ClientOptions::new().pool(pool_opts);

        if let Some(proxy_addr) = self.config.tor_proxy {
            tracing::info!(
                "Tor proxy enabled: {} (routing .onion addresses only)",
                proxy_addr
            );
            // Configure proxy for .onion addresses only
            // Clearnet relays will connect directly
            let connection = Connection::new()
                .proxy(proxy_addr)
                .target(ConnectionTarget::Onion);
            client_opts = client_opts.connection(connection);
        }

        // Create client with signer for NIP-42 authentication
        let client = Client::builder()
            .signer(keys)
            .opts(client_opts)
            .build();

        // Enable automatic authentication (NIP-42)
        client.automatic_authentication(true);

        // Add seed relays
        for relay_url in &self.config.seed_relays {
            // Register with manager if available
            if let Some(ref manager) = self.relay_manager
                && let Err(e) = manager.register_relay(relay_url, crate::relay::RelayTier::Seed)
            {
                tracing::warn!("Failed to register seed relay {}: {}", relay_url, e);
            }

            if let Err(e) = client.add_relay(relay_url).await {
                tracing::warn!("Failed to add relay {}: {}", relay_url, e);
                self.record_connection_failure(relay_url);
            } else {
                tracing::debug!("Added relay: {}", relay_url);
            }
        }

        // Connect to relays
        client.connect().await;

        // Wait a bit for connections to establish
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Record successful connections
        let relays = client.relays().await;
        let mut connected_count = 0usize;
        for (url, relay) in &relays {
            // Check if relay is actually connected
            if relay.status() == nostr_sdk::RelayStatus::Connected {
                self.record_connection(&url.to_string()).await;
                metrics::counter!("relay_connects_total", "reason" => "seed").increment(1);
                connected_count += 1;
            }
        }

        // Update connected count
        self.stats
            .relays_connected
            .store(connected_count, Ordering::Relaxed);
        tracing::info!("Connected to {} relays", connected_count);

        // Build subscription filter
        // If since_timestamp is set, we're catching up from a previous run
        let filter = if let Some(since_ts) = self.config.since_timestamp {
            let since = Timestamp::from(since_ts);
            let formatted = DateTime::from_timestamp(since_ts as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "invalid".to_string());
            tracing::info!(
                "Catch-up mode: requesting events since {} ({})",
                since_ts,
                formatted
            );
            Arc::new(Filter::new().since(since))
        } else {
            tracing::info!("Live mode: subscribing to new events only (no catch-up)");
            Arc::new(Filter::new())
        };

        // Subscribe
        let output = client.subscribe((*filter).clone(), None).await?;
        tracing::info!("Subscribed with ID: {:?}", output.val);

        // Get notification receiver
        let mut notifications = client.notifications();

        // Process events
        let mut event_count = 0usize;
        let progress_interval = 10_000;

        // Optimization timer
        // Run first optimization quickly (30s) to bootstrap from imported/discovered relays
        // Subsequent optimizations use the configured interval
        let optimization_interval = self.config.optimization_interval;
        let bootstrap_interval = Duration::from_secs(30);
        let mut last_optimization = Instant::now();
        let mut optimization_count = 0usize;

        // Rate-limited lag warning (avoid log spam during bursts)
        let mut last_lag_warning = Instant::now();
        let mut lag_count_since_warning = 0u64;
        let mut lag_messages_since_warning = 0u64;
        let lag_warning_interval = Duration::from_secs(10);

        while self.running.load(Ordering::SeqCst) {
            // Check if it's time to run optimization
            // First 3 optimizations run at bootstrap_interval (30s) for faster relay discovery
            // After that, use the configured optimization_interval
            let current_interval = if optimization_count < 3 {
                bootstrap_interval
            } else {
                optimization_interval
            };

            if self.relay_manager.is_some()
                && !optimization_interval.is_zero()
                && last_optimization.elapsed() >= current_interval
            {
                // Reconcile actual connection status before making optimization decisions
                self.reconcile_relay_status(&client).await;
                self.run_optimization(&client, &filter).await;
                last_optimization = Instant::now();
                optimization_count += 1;
            }

            // Use timeout to periodically check running flag and optimization timer
            let notification =
                tokio::time::timeout(Duration::from_secs(1), notifications.recv()).await;

            let notification = match notification {
                Ok(Ok(n)) => n,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    // All senders dropped - channel is truly closed
                    tracing::info!("Notification channel closed");
                    break;
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(count))) => {
                    // Receiver fell behind - some notifications were dropped
                    // This can happen during heavy relay traffic
                    // Track metrics and rate-limit warnings to avoid log spam
                    metrics::counter!("relay_notifications_lagged_total").increment(count);
                    lag_count_since_warning += 1;
                    lag_messages_since_warning += count;

                    // Only log every lag_warning_interval to avoid spam
                    if last_lag_warning.elapsed() >= lag_warning_interval {
                        tracing::warn!(
                            "Notification receiver lagged {} times, dropped {} messages in last {:?}",
                            lag_count_since_warning,
                            lag_messages_since_warning,
                            last_lag_warning.elapsed()
                        );
                        last_lag_warning = Instant::now();
                        lag_count_since_warning = 0;
                        lag_messages_since_warning = 0;
                    }
                    continue;
                }
                Err(_) => {
                    // Timeout - check running flag and continue
                    continue;
                }
            };

            match notification {
                RelayPoolNotification::Event {
                    relay_url, event, ..
                } => {
                    self.stats.total_events.fetch_add(1, Ordering::Relaxed);
                    self.stats.valid_events.fetch_add(1, Ordering::Relaxed);
                    event_count += 1;

                    // Extract relay URL as string for attribution
                    let relay_url_str = relay_url.to_string();

                    // Check for NIP-65 relay list events for discovery
                    // Discovery only registers relays - the optimization loop connects to them
                    if self.config.discovery_enabled
                        && event.kind == Kind::RelayList
                        && let Err(e) = self.process_relay_list(&event).await
                    {
                        tracing::debug!("Failed to process relay list: {}", e);
                    }

                    // Pass raw event to handler - let it decide whether to pack
                    // This allows dedupe checks BEFORE expensive serialization
                    match handler(relay_url_str, &event) {
                        Ok(true) => {} // Continue
                        Ok(false) => {
                            tracing::info!("Handler signaled stop");
                            break;
                        }
                        Err(e) => {
                            tracing::error!("Handler error: {}", e);
                            break;
                        }
                    }

                    // Progress logging
                    if event_count.is_multiple_of(progress_interval) {
                        tracing::info!(
                            "Received {} events, {} relays connected",
                            event_count,
                            self.stats.relays_connected.load(Ordering::Relaxed)
                        );
                    }
                }

                RelayPoolNotification::Message { relay_url, message } => {
                    // Handle relay messages (NOTICE, AUTH responses, etc.)
                    match message {
                        RelayMessage::Notice(notice_msg) => {
                            // Some relays send notices when rejecting connections
                            let msg_lower = notice_msg.to_lowercase();
                            if msg_lower.contains("auth")
                                || msg_lower.contains("whitelist")
                                || msg_lower.contains("paid")
                                || msg_lower.contains("denied")
                                || msg_lower.contains("not allowed")
                            {
                                tracing::warn!(
                                    "Relay {} rejected access: {}. Disconnecting.",
                                    relay_url,
                                    notice_msg
                                );
                                let relay_url_str = relay_url.to_string();
                                // Record disconnection before disconnecting
                                self.record_disconnection(&relay_url_str).await;
                                metrics::counter!("relay_disconnects_total", "reason" => "rejection").increment(1);
                                // Disconnect from this relay
                                if let Err(e) = client.disconnect_relay(relay_url.clone()).await {
                                    tracing::debug!(
                                        "Failed to disconnect from {}: {}",
                                        relay_url,
                                        e
                                    );
                                }
                                self.stats.relays_connected.fetch_sub(1, Ordering::Relaxed);
                            } else {
                                tracing::debug!("Relay {} notice: {}", relay_url, notice_msg);
                            }
                        }
                        RelayMessage::Closed {
                            subscription_id,
                            message: closed_msg,
                        } => {
                            // Relay closed our subscription, possibly due to auth failure
                            let msg_lower = closed_msg.to_lowercase();
                            if msg_lower.contains("auth")
                                || msg_lower.contains("restricted")
                                || msg_lower.contains("not allowed")
                            {
                                tracing::warn!(
                                    "Relay {} closed subscription {}: {}. Disconnecting.",
                                    relay_url,
                                    subscription_id,
                                    closed_msg
                                );
                                let relay_url_str = relay_url.to_string();
                                // Record disconnection before disconnecting
                                self.record_disconnection(&relay_url_str).await;
                                metrics::counter!("relay_disconnects_total", "reason" => "rejection").increment(1);
                                if let Err(e) = client.disconnect_relay(relay_url.clone()).await {
                                    tracing::debug!(
                                        "Failed to disconnect from {}: {}",
                                        relay_url,
                                        e
                                    );
                                }
                                self.stats.relays_connected.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    }
                }

                RelayPoolNotification::Shutdown => {
                    tracing::info!("Relay pool shutdown notification received");
                    break;
                }
            }
        }

        // Disconnect
        client.disconnect().await;

        self.running.store(false, Ordering::SeqCst);

        Ok(self.build_stats())
    }

    /// Run the optimization loop: swap low-scoring relays for higher-scoring ones.
    async fn run_optimization(&self, client: &Client, filter: &Arc<Filter>) {
        use metrics::{counter, gauge};

        let manager = match &self.relay_manager {
            Some(m) => m,
            None => return,
        };

        // First, recompute scores
        if let Err(e) = manager.recompute_scores() {
            tracing::warn!("Failed to recompute relay scores: {}", e);
            return;
        }

        // Get list of currently connected relay URLs
        let connected_relays: Vec<String> = {
            let relays = client.relays().await;
            let mut urls = Vec::with_capacity(relays.len());
            for (url, relay) in relays {
                if relay.status() == nostr_sdk::RelayStatus::Connected {
                    urls.push(url.to_string());
                }
            }
            urls
        };

        // Get optimization suggestions
        let suggestions = match manager.get_optimization_suggestions(&connected_relays) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to get optimization suggestions: {}", e);
                return;
            }
        };

        // Update metrics
        counter!("relay_optimization_cycles_total").increment(1);
        gauge!("relay_optimization_swaps").set(suggestions.to_disconnect.len() as f64);
        gauge!("relay_optimization_explorations").set(suggestions.exploration_relays.len() as f64);

        if suggestions.is_empty() {
            tracing::debug!("Optimization: no changes needed");
            return;
        }

        tracing::info!(
            "Optimization: disconnecting {} relays, connecting {} (swaps) + {} (exploration)",
            suggestions.to_disconnect.len(),
            suggestions.to_connect.len(),
            suggestions.exploration_relays.len()
        );

        // Disconnect low-scoring relays
        for url in &suggestions.to_disconnect {
            tracing::info!("Disconnecting low-scoring relay: {}", url);
            self.record_disconnection(url).await;
            counter!("relay_disconnects_total", "reason" => "optimization").increment(1);

            if let Ok(relay_url) = RelayUrl::parse(url) {
                if let Err(e) = client.disconnect_relay(relay_url).await {
                    tracing::warn!("Failed to disconnect {}: {}", url, e);
                } else {
                    self.stats.relays_connected.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }

        // Connect higher-scoring relays (swaps)
        for url in &suggestions.to_connect {
            tracing::info!("Connecting higher-scoring relay: {}", url);
            self.try_connect_relay(client, filter, url, "swap").await;
        }

        // Connect exploration relays (untested/stale relays)
        for url in &suggestions.exploration_relays {
            tracing::info!("Exploring untested relay: {}", url);
            counter!("relay_exploration_attempts_total").increment(1);
            self.try_connect_relay(client, filter, url, "exploration")
                .await;
        }

        // Log aggregate stats
        if let Ok(stats) = manager.get_aggregate_stats() {
            tracing::info!(
                "Relay stats: {} total, {} active, {} blocked, avg score {:.3}",
                stats.total_relays,
                stats.active_relays,
                stats.blocked_relays,
                stats.avg_score
            );
        }
    }

    /// Try to connect to a relay and subscribe it to the filter.
    async fn try_connect_relay(
        &self,
        client: &Client,
        filter: &Arc<Filter>,
        url: &str,
        reason: &'static str,
    ) {
        use metrics::counter;

        // Add to known set
        {
            let mut known = self.known_relays.write().await;
            known.insert(url.to_string());
        }

        // Add and connect
        match client.add_relay(url).await {
            Ok(_) => match client.connect_relay(url).await {
                Ok(_) => {
                    self.record_connection(url).await;
                    self.stats.relays_connected.fetch_add(1, Ordering::Relaxed);
                    counter!("relay_connects_total", "reason" => reason).increment(1);

                    // Subscribe the new relay
                    if let Ok(relay_url) = RelayUrl::parse(url)
                        && let Err(e) = client
                            .subscribe_to(vec![relay_url], (**filter).clone(), None)
                            .await
                    {
                        tracing::warn!("Failed to subscribe new relay {}: {}", url, e);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to connect to {}: {}", url, e);
                    self.record_connection_failure(url);
                    counter!("relay_connect_failures_total", "reason" => reason).increment(1);
                }
            },
            Err(e) => {
                tracing::debug!("Failed to add relay {}: {}", url, e);
                self.record_connection_failure(url);
                counter!("relay_connect_failures_total", "reason" => reason).increment(1);
            }
        }
    }

    /// Process a NIP-65 relay list event to discover new relays.
    ///
    /// Discovery only REGISTERS relays with the manager - it does NOT connect
    /// immediately. This prevents a flood of connection attempts when many relay
    /// list events arrive in a short time. The optimization loop handles actually
    /// connecting to the best relays later.
    async fn process_relay_list(&self, event: &Event) -> Result<()> {
        // Parse relay URLs from 'r' tags
        for tag in event.tags.iter() {
            let tag_vec: Vec<&str> = tag.as_slice().iter().map(|s| s.as_str()).collect();

            if tag_vec.first() == Some(&"r") && tag_vec.len() >= 2 {
                let relay_url = tag_vec[1];

                // Normalize and validate URL
                let normalized = match crate::relay::normalize_relay_url(relay_url) {
                    crate::relay::NormalizeResult::Ok(u) => u,
                    _ => continue, // Skip invalid or blocked URLs
                };

                // Check blocklist (using normalized URL)
                if self.config.blocklist.contains(&normalized) {
                    continue;
                }

                // Check if we already know this relay
                let is_new = {
                    let known = self.known_relays.read().await;
                    !known.contains(&normalized)
                };

                if is_new {
                    // Add to known set
                    {
                        let mut known = self.known_relays.write().await;
                        known.insert(normalized.clone());
                    }

                    // Register with relay manager if available
                    // The optimization loop will decide when to connect
                    if let Some(ref manager) = self.relay_manager {
                        match manager
                            .register_relay(&normalized, crate::relay::RelayTier::Discovered)
                        {
                            Ok(_) => {
                                self.stats.relays_discovered.fetch_add(1, Ordering::Relaxed);
                                metrics::counter!("relay_discovered_total").increment(1);
                                tracing::debug!("Discovered relay: {}", normalized);
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Failed to register discovered relay {}: {}",
                                    normalized,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Build statistics from the current state.
    fn build_stats(&self) -> SourceStats {
        SourceStats {
            total_events: self.stats.total_events.load(Ordering::Relaxed),
            valid_events: self.stats.valid_events.load(Ordering::Relaxed),
            invalid_events: self.stats.invalid_events.load(Ordering::Relaxed),
            validation_errors: 0,
            parse_errors: self.stats.invalid_events.load(Ordering::Relaxed),
            source_metadata: SourceMetadata {
                relays_connected: Some(self.stats.relays_connected.load(Ordering::Relaxed)),
                relays_discovered: Some(self.stats.relays_discovered.load(Ordering::Relaxed)),
                ..Default::default()
            },
        }
    }
}

impl EventSource for RelaySource {
    fn name(&self) -> &'static str {
        "relay"
    }

    fn process<F>(&mut self, mut handler: F) -> Result<SourceStats>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        use crate::pipeline::pack_nostr_event;

        // Create a tokio runtime for the async code
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(Error::Io)?;

        // Wrap the handler to adapt from (String, &Event) to PackedEvent
        // This provides backward compatibility with the EventSource trait
        // Note: This path does NOT benefit from the dedupe-before-pack optimization
        // For optimized handling, use run_async directly with a custom handler
        rt.block_on(self.run_async(|_relay_url, event| {
            match pack_nostr_event(event) {
                Ok(packed) => handler(packed),
                Err(e) => {
                    tracing::debug!("Failed to pack event: {}", e);
                    Ok(true) // Continue on pack errors
                }
            }
        }))
    }
}
