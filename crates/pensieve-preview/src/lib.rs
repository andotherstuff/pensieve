//! Pensieve Preview - Static HTML preview pages for Nostr events.
//!
//! This crate provides a lightweight HTTP server that renders static HTML
//! preview pages for Nostr events, profiles, and other entities. It is
//! designed to be placed behind a CDN (e.g., Cloudflare) for edge caching.
//!
//! # Architecture
//!
//! - **Resolve**: Decodes NIP-19 bech32 identifiers and fetches data from ClickHouse
//! - **Render**: Generates HTML with Open Graph tags using maud (compile-time templates)
//! - **Cache**: In-process moka cache + Cache-Control headers for CDN caching
//!
//! # URL Pattern
//!
//! ```text
//! GET /{nip19_identifier}
//! ```
//!
//! Supported identifiers:
//! - `npub1...` - Public key → profile page
//! - `nprofile1...` - Profile with relay hints → profile page
//! - `note1...` - Event ID → event page
//! - `nevent1...` - Event with metadata → event page
//! - `naddr1...` - Replaceable event coordinate → event page
//! - 64-char hex - Event ID or pubkey (auto-detected)
//!
//! # Security
//!
//! - All dynamic content is HTML-escaped by maud
//! - URLs are validated (HTTPS/HTTP only) before use in attributes
//! - Strict Content-Security-Policy: no JavaScript execution
//! - X-Frame-Options: DENY prevents clickjacking

pub mod config;
pub mod error;
pub mod query;
pub mod render;
pub mod resolve;
pub mod routes;
pub mod state;

pub use config::Config;
pub use routes::router;
pub use state::AppState;
