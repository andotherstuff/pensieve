//! Pensieve Serve - HTTP API for Nostr analytics
//!
//! This crate provides a REST API for querying aggregate Nostr data from
//! ClickHouse. It is designed for analytics-oriented access patterns rather
//! than real-time relay subscriptions.
//!
//! # Authentication
//!
//! All endpoints require Bearer token authentication. Tokens are configured via
//! environment variables (typically in a `.env` file).
//!
//! # Architecture
//!
//! - **AppState**: Shared ClickHouse client, relay database, and response cache
//! - **Auth**: Bearer token middleware for request authentication
//! - **Routes**: Endpoint handlers grouped by domain
//! - **Rollups**: SQL and operator tooling for the migration 019 rollup tables

mod auth;
pub mod bounded_clickhouse;
pub mod cache;
mod error;
pub mod rollups;
mod routes;
mod state;

pub use self::auth::require_auth;
pub use self::cache::{ResponseCache, get_or_compute, new_cache};
pub use self::error::ApiError;
pub use self::routes::router;
pub use self::state::{AppState, Config};
