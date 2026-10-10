//! Application state shared across all request handlers.

use std::sync::Arc;

use clickhouse::Client;
use moka::future::Cache;

use crate::config::Config;
use crate::lookups::Lookups;

/// Cached HTML response with metadata for TTL decisions.
#[derive(Clone, Debug)]
pub struct CachedHtml {
    /// Rendered HTML string.
    pub html: String,
    /// When this entry was cached.
    pub cached_at: chrono::DateTime<chrono::Utc>,
}

/// Type alias for the HTML response cache.
pub type HtmlCache = Cache<String, CachedHtml>;

/// Type alias for the OG image cache (identifier -> PNG bytes).
pub type OgImageCache = Cache<String, Vec<u8>>;

/// Default cache capacity (number of entries).
/// Each entry is typically 3-10KB of HTML, so 100K entries ~= 300MB-1GB.
const DEFAULT_CACHE_CAPACITY: u64 = 100_000;

/// Default cache TTL (used as the base; actual TTL varies per content type).
const DEFAULT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// OG image cache capacity.
/// Each image is ~20-50KB PNG, so 10K entries ~= 200-500MB.
const OG_CACHE_CAPACITY: u64 = 10_000;

/// OG image cache TTL — images change infrequently (avatar updates).
const OG_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Shared application state available to all request handlers.
#[derive(Clone)]
pub struct AppState {
    /// ClickHouse client for database queries.
    pub clickhouse: Client,

    /// Preview read layer (legacy or compact source per lookup).
    pub lookups: Lookups,

    /// Application configuration.
    pub config: Arc<Config>,

    /// In-memory HTML response cache keyed by NIP-19 identifier.
    pub cache: HtmlCache,

    /// In-memory OG image cache keyed by NIP-19 identifier.
    pub og_cache: OgImageCache,
}

/// ClickHouse settings applied to every preview query when
/// `PREVIEW_DISABLE_PROJECTIONS` is set.
///
/// Besides disabling projections, they bound any query that would now scan
/// `events_local` (a lookup still on its legacy path): it fails after 10
/// seconds, matching the HTTP request timeout, and is cancelled if the request
/// is dropped first, so a rehearsal cannot pile up long-running scans.
pub const PROJECTIONS_OFF_SETTINGS: [(&str, &str); 4] = [
    ("optimize_use_projections", "0"),
    ("max_execution_time", "10"),
    ("timeout_overflow_mode", "throw"),
    ("cancel_http_readonly_queries_on_client_close", "1"),
];

/// Build the preview's ClickHouse client from configuration.
pub fn clickhouse_client(config: &Config) -> Client {
    let mut client = Client::default()
        .with_url(&config.clickhouse_url)
        .with_database(&config.clickhouse_database);
    if config.disable_projections {
        for (name, value) in PROJECTIONS_OFF_SETTINGS {
            client = client.with_option(name, value);
        }
    }
    client
}

impl AppState {
    /// Create a new application state from configuration.
    pub fn new(config: Config) -> Self {
        let clickhouse = clickhouse_client(&config);

        let cache = Cache::builder()
            .max_capacity(DEFAULT_CACHE_CAPACITY)
            .time_to_live(DEFAULT_CACHE_TTL)
            .build();

        let og_cache = Cache::builder()
            .max_capacity(OG_CACHE_CAPACITY)
            .time_to_live(OG_CACHE_TTL)
            .build();

        tracing::info!(
            cache_capacity = DEFAULT_CACHE_CAPACITY,
            cache_ttl_secs = DEFAULT_CACHE_TTL.as_secs(),
            og_cache_capacity = OG_CACHE_CAPACITY,
            og_cache_ttl_secs = OG_CACHE_TTL.as_secs(),
            "application state initialized"
        );

        let lookups = Lookups::new(clickhouse.clone(), config.compact_lookups);

        Self {
            clickhouse,
            lookups,
            config: Arc::new(config),
            cache,
            og_cache,
        }
    }
}
