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

use super::{EventSource, SourceMetadata, SourceStats};
use crate::pipeline::PackedEvent;
use crate::{Error, Result};
use nostr_sdk::prelude::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

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
            max_relays: 100,
            blocklist: HashSet::new(),
            connection_timeout: Duration::from_secs(30),
            subscribe_all: true,
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

    /// Run the relay source asynchronously.
    ///
    /// This is the main async implementation that the sync `process` method calls.
    pub async fn run_async<F>(&self, mut handler: F) -> Result<SourceStats>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        self.running.store(true, Ordering::SeqCst);

        info!(
            "Starting relay source with {} seed relays, discovery={}",
            self.config.seed_relays.len(),
            self.config.discovery_enabled
        );

        // Generate a random keypair for NIP-42 authentication
        // This allows us to authenticate with relays that require it
        // We don't use this key for signing events, just for relay auth
        let keys = Keys::generate();
        info!(
            "Generated ephemeral keypair for relay auth: {}",
            keys.public_key().to_bech32().unwrap_or_else(|_| "unknown".to_string())
        );

        // Create client with signer for NIP-42 authentication
        let client = Client::builder().signer(keys).build();

        // Enable automatic authentication (NIP-42)
        client.automatic_authentication(true);

        // Add seed relays
        for relay_url in &self.config.seed_relays {
            if let Err(e) = client.add_relay(relay_url).await {
                warn!("Failed to add relay {}: {}", relay_url, e);
            } else {
                debug!("Added relay: {}", relay_url);
            }
        }

        // Connect to relays
        client.connect().await;

        // Wait a bit for connections to establish
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Update connected count
        let connected = client.relays().await.len();
        self.stats
            .relays_connected
            .store(connected, Ordering::Relaxed);
        info!("Connected to {} relays", connected);

        // Subscribe to all events (empty filter = all events)
        // We use an Arc so we can share the filter with the discovery process
        let filter = Arc::new(Filter::new());

        // Subscribe
        let output = client.subscribe((*filter).clone(), None).await?;
        info!("Subscribed with ID: {:?}", output.val);

        // Get notification receiver
        let mut notifications = client.notifications();

        // Process events
        let mut event_count = 0usize;
        let progress_interval = 10_000;

        while self.running.load(Ordering::SeqCst) {
            // Use timeout to periodically check running flag
            let notification = tokio::time::timeout(
                Duration::from_secs(1),
                notifications.recv(),
            )
            .await;

            let notification = match notification {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => {
                    // Channel closed
                    info!("Notification channel closed");
                    break;
                }
                Err(_) => {
                    // Timeout - check running flag and continue
                    continue;
                }
            };

            match notification {
                RelayPoolNotification::Event { event, .. } => {
                    self.stats.total_events.fetch_add(1, Ordering::Relaxed);
                    event_count += 1;

                    // Events from nostr-sdk are already validated
                    // Pack to notepack format
                    match self.pack_event(&event) {
                        Ok(packed) => {
                            self.stats.valid_events.fetch_add(1, Ordering::Relaxed);

                            // Check for NIP-65 relay list events for discovery
                            if self.config.discovery_enabled
                                && event.kind == Kind::RelayList
                                && let Err(e) =
                                    self.process_relay_list(&event, &client, &filter).await
                            {
                                debug!("Failed to process relay list: {}", e);
                            }

                            // Call the handler
                            match handler(packed) {
                                Ok(true) => {} // Continue
                                Ok(false) => {
                                    info!("Handler signaled stop");
                                    break;
                                }
                                Err(e) => {
                                    error!("Handler error: {}", e);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            self.stats.invalid_events.fetch_add(1, Ordering::Relaxed);
                            debug!("Failed to pack event: {}", e);
                        }
                    }

                    // Progress logging
                    if event_count.is_multiple_of(progress_interval) {
                        info!(
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
                                warn!(
                                    "Relay {} rejected access: {}. Disconnecting.",
                                    relay_url, notice_msg
                                );
                                // Disconnect from this relay
                                if let Err(e) = client.disconnect_relay(relay_url.clone()).await {
                                    debug!("Failed to disconnect from {}: {}", relay_url, e);
                                }
                                self.stats.relays_connected.fetch_sub(1, Ordering::Relaxed);
                            } else {
                                debug!("Relay {} notice: {}", relay_url, notice_msg);
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
                                warn!(
                                    "Relay {} closed subscription {}: {}. Disconnecting.",
                                    relay_url, subscription_id, closed_msg
                                );
                                if let Err(e) = client.disconnect_relay(relay_url.clone()).await {
                                    debug!("Failed to disconnect from {}: {}", relay_url, e);
                                }
                                self.stats.relays_connected.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    }
                }

                RelayPoolNotification::Shutdown => {
                    info!("Relay pool shutdown notification received");
                    break;
                }
            }
        }

        // Disconnect
        client.disconnect().await;

        self.running.store(false, Ordering::SeqCst);

        Ok(self.build_stats())
    }

    /// Pack a nostr Event into notepack format.
    fn pack_event(&self, event: &Event) -> Result<PackedEvent> {
        use notepack::{pack_note_into, NoteBuf};

        let mut buf = Vec::with_capacity(512);

        // Convert tags to the format notepack expects
        let tags: Vec<Vec<String>> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect())
            .collect();

        // Format signature as hex
        let sig_bytes = event.sig.serialize();
        let sig_hex = hex::encode(sig_bytes);

        let note = NoteBuf {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16() as u64,
            tags,
            content: event.content.clone(),
            sig: sig_hex,
        };

        pack_note_into(&note, &mut buf).map_err(|e| Error::Serialization(e.to_string()))?;

        Ok(PackedEvent {
            event_id: *event.id.as_bytes(),
            data: buf,
        })
    }

    /// Process a NIP-65 relay list event to discover new relays.
    ///
    /// When a new relay is discovered and connected, we also subscribe it to
    /// the same filter so it starts sending us events immediately.
    async fn process_relay_list(
        &self,
        event: &Event,
        client: &Client,
        filter: &Arc<Filter>,
    ) -> Result<()> {
        let current_count = self.known_relays.read().await.len();

        if current_count >= self.config.max_relays {
            return Ok(()); // Already at max
        }

        // Parse relay URLs from 'r' tags
        for tag in event.tags.iter() {
            let tag_vec: Vec<&str> = tag.as_slice().iter().map(|s| s.as_str()).collect();

            if tag_vec.first() == Some(&"r") && tag_vec.len() >= 2 {
                let relay_url = tag_vec[1];

                // Normalize and validate
                if let Ok(url) = RelayUrl::parse(relay_url) {
                    let url_str = url.to_string();

                    // Check blocklist
                    if self.config.blocklist.contains(&url_str) {
                        continue;
                    }

                    // Check if we already know this relay
                    let is_new = {
                        let known = self.known_relays.read().await;
                        !known.contains(&url_str) && known.len() < self.config.max_relays
                    };

                    if is_new {
                        // Add to known set
                        {
                            let mut known = self.known_relays.write().await;
                            if known.len() < self.config.max_relays {
                                known.insert(url_str.clone());
                            } else {
                                continue;
                            }
                        }

                        // Try to add and connect
                        match client.add_relay(&url_str).await {
                            Ok(_) => {
                                self.stats.relays_discovered.fetch_add(1, Ordering::Relaxed);
                                debug!("Discovered and added relay: {}", url_str);

                                // Try to connect the new relay
                                match client.connect_relay(&url_str).await {
                                    Ok(_) => {
                                        self.stats.relays_connected.fetch_add(1, Ordering::Relaxed);
                                        info!("Connected to discovered relay: {}", url_str);

                                        // Subscribe the new relay to the same filter
                                        // so it starts sending us events immediately
                                        if let Ok(relay_url) = RelayUrl::parse(&url_str) {
                                            match client
                                                .subscribe_to(vec![relay_url], (**filter).clone(), None)
                                                .await
                                            {
                                                Ok(_) => {
                                                    debug!(
                                                        "Subscribed discovered relay to event stream: {}",
                                                        url_str
                                                    );
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to subscribe discovered relay {}: {}",
                                                        url_str, e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        debug!(
                                            "Failed to connect to discovered relay {}: {}",
                                            url_str, e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("Failed to add discovered relay {}: {}", url_str, e);
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

    fn process<F>(&mut self, handler: F) -> Result<SourceStats>
    where
        F: FnMut(PackedEvent) -> Result<bool>,
    {
        // Create a tokio runtime for the async code
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(Error::Io)?;

        rt.block_on(self.run_async(handler))
    }
}
