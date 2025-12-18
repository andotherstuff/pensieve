//! Relay quality tracking and management.
//!
//! This module provides persistent tracking of relay quality metrics and
//! intelligent relay slot management.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         RelayManager                            │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  - Persists per-relay stats to SQLite                           │
//! │  - Computes quality scores based on novel rate + uptime         │
//! │  - Manages relay slot allocation                                │
//! │  - Exports aggregate Prometheus metrics                         │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use pensieve_ingest::relay::{RelayManager, RelayManagerConfig};
//!
//! let config = RelayManagerConfig {
//!     db_path: "./data/relay-stats.db".into(),
//!     max_relays: 100,
//!     ..Default::default()
//! };
//!
//! let manager = RelayManager::open(config)?;
//!
//! // Register seed relays
//! manager.register_seed_relays(&["wss://relay.example.com".to_string()])?;
//!
//! // Record events as they arrive
//! manager.record_event("wss://relay.example.com", true); // novel
//! manager.record_event("wss://relay.example.com", false); // duplicate
//!
//! // Periodically recompute scores
//! manager.recompute_scores()?;
//!
//! // Get optimization suggestions
//! let (disconnect, connect) = manager.get_optimization_suggestions(&connected_urls)?;
//! ```

mod manager;
mod schema;
mod scoring;

pub use manager::{AggregateRelayStats, OptimizationSuggestions, RelayManager, RelayManagerConfig};
pub use schema::{RelayStatus, RelayTier};
pub use scoring::{RelayScore, RelayStatsForScoring};

