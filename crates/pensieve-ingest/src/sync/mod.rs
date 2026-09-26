//! Negentropy sync module for NIP-77 reconciliation.
//!
//! This module provides periodic sync with trusted relays using the negentropy
//! set reconciliation protocol (NIP-77). It complements the live event stream
//! by discovering and fetching events that may have been missed.
//!
//! # Architecture
//!
//! Both live ingestion and negentropy sync feed into the **same event handler**,
//! which uses the shared `DedupeIndex` to prevent duplicates. The sync module
//! maintains its own RocksDB database (`SyncStateDb`) to track which events have
//! been synced, enabling efficient negentropy reconciliation.
//!
//! ```text
//! ┌──────────────────┐       ┌──────────────────┐
//! │   Live Ingester  │       │  Negentropy Sync │
//! │   (RelaySource)  │       │     (Periodic)   │
//! └────────┬─────────┘       └────────┬─────────┘
//!          │                          │
//!          │ Event                    │ Event
//!          └──────────┬───────────────┘
//!                     │
//!                     ▼
//!          ┌──────────────────────────┐
//!          │   Shared Event Handler   │
//!          │   (dedupe + pack + write)│
//!          └──────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use pensieve_ingest::sync::{NegentropySyncer, NegentropySyncConfig, SyncStateDb};
//!
//! // Create sync state database
//! let sync_state = SyncStateDb::open("./data/sync-state")?;
//!
//! // Configure negentropy sync
//! let config = NegentropySyncConfig {
//!     relays: vec!["wss://relay.damus.io".to_string()],
//!     interval: Duration::from_secs(600), // 10 minutes
//!     lookback: Duration::from_secs(7 * 24 * 3600), // 7 days
//!     ..Default::default()
//! };
//!
//! // Create syncer
//! let syncer = NegentropySyncer::new(config, sync_state);
//!
//! // Run periodic sync
//! syncer.run_periodic(|event| {
//!     handle_event(event)
//! }).await?;
//! ```

#[cfg(unix)]
pub mod binding;
pub mod failure;
pub mod inventory;
pub mod ipc;
pub mod jobs;
mod negentropy;
#[cfg(unix)]
pub mod parent;
#[cfg(unix)]
pub mod runtime;
mod state;
#[cfg(unix)]
pub mod worker;

pub use negentropy::{NegentropySyncConfig, NegentropySyncer, SyncStats, seed_from_clickhouse};
pub use state::{ArchivedWindow, MAX_WINDOW_ITEMS, SyncStateDb};
