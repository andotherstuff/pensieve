//! Pensieve Serve - HTTP API for Nostr analytics
//!
//! This crate provides a REST API for querying aggregate Nostr data stored in ClickHouse.
//! It is designed for analytics-oriented access patterns (counts, trends, aggregations)
//! rather than real-time relay subscriptions.
//!
//! # Authentication
//!
//! All endpoints require Bearer token authentication. Tokens are configured via
//! environment variables (typically in a `.env` file).
//!
//! # Architecture
//!
//! - **AppState**: Shared application state (ClickHouse client, configuration)
//! - **Auth**: Bearer token middleware for request authentication
//! - **Routes**: Endpoint handlers grouped by domain

mod auth;
pub mod cache;
mod error;
mod routes;
mod state;

pub use self::auth::require_auth;
pub use self::cache::{get_or_compute, new_cache, ResponseCache};
pub use self::error::ApiError;
pub use self::routes::router;
pub use self::state::{AppState, Config};

