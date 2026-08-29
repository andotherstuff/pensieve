//! Application state and configuration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, bail};
use clickhouse::Client;
use parking_lot::Mutex;
use rusqlite::Connection;
use tokio::sync::RwLock;
use tokio_postgres::{Client as PostgresClient, Config as PostgresConfig, NoTls};

use crate::cache::{self, ResponseCache};

/// Independently selectable analytics endpoint families.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AnalyticsFamily {
    /// Overview plus granular headline counters and timestamps.
    Overview,
    /// Event counts and throughput.
    Events,
    /// Active-user summary and time series.
    ActiveUsers,
    /// New-user time series.
    NewUsers,
    /// Cohort retention.
    Retention,
    /// Hour-of-day activity.
    Activity,
    /// Zap totals and histogram.
    Zaps,
    /// Engagement totals.
    Engagement,
    /// Long-form content metrics.
    Longform,
    /// Exact predefined publisher rankings.
    Publishers,
    /// NIP-65 relay distribution.
    RelayDistribution,
    /// Kind list, details, and activity.
    Kinds,
}

impl AnalyticsFamily {
    const ALL: [Self; 12] = [
        Self::Overview,
        Self::Events,
        Self::ActiveUsers,
        Self::NewUsers,
        Self::Retention,
        Self::Activity,
        Self::Zaps,
        Self::Engagement,
        Self::Longform,
        Self::Publishers,
        Self::RelayDistribution,
        Self::Kinds,
    ];

    /// Stable environment/configuration name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Events => "events",
            Self::ActiveUsers => "active_users",
            Self::NewUsers => "new_users",
            Self::Retention => "retention",
            Self::Activity => "activity",
            Self::Zaps => "zaps",
            Self::Engagement => "engagement",
            Self::Longform => "longform",
            Self::Publishers => "publishers",
            Self::RelayDistribution => "relay_distribution",
            Self::Kinds => "kinds",
        }
    }
}

/// Per-family backend selection used for incremental cutover and rollback.
#[derive(Clone, Debug, Default)]
pub struct AnalyticsBackendSelection {
    postgres_families: HashSet<AnalyticsFamily>,
}

impl AnalyticsBackendSelection {
    fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let names = value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if names.contains(&"none") && names.len() != 1 {
            bail!("Postgres API family 'none' cannot be combined with other families");
        }
        let mut postgres_families = HashSet::new();
        for name in names {
            if name == "none" {
                continue;
            }
            if name == "all" {
                postgres_families.extend(AnalyticsFamily::ALL);
                continue;
            }
            let family = AnalyticsFamily::ALL
                .into_iter()
                .find(|family| family.as_str() == name)
                .ok_or_else(|| anyhow::anyhow!("unknown Postgres API family: {name}"))?;
            postgres_families.insert(family);
        }
        Ok(Self { postgres_families })
    }

    /// Whether one family is explicitly selected for Postgres.
    pub fn uses_postgres(&self, family: AnalyticsFamily) -> bool {
        self.postgres_families.contains(&family)
    }

    fn any_postgres(&self) -> bool {
        !self.postgres_families.is_empty()
    }

    fn names(&self) -> Vec<&'static str> {
        AnalyticsFamily::ALL
            .into_iter()
            .filter(|family| self.uses_postgres(*family))
            .map(AnalyticsFamily::as_str)
            .collect()
    }
}

/// Application configuration loaded from environment.
#[derive(Clone)]
pub struct Config {
    /// Server bind address (e.g., "0.0.0.0:3000").
    pub bind_addr: String,

    /// ClickHouse connection URL.
    pub clickhouse_url: String,

    /// ClickHouse database name.
    pub clickhouse_database: String,

    /// Postgres connection string, required when any family selects Postgres.
    pub postgres_url: Option<String>,

    /// Postgres password supplied outside the connection string.
    pub postgres_password: Option<String>,

    /// Maximum persistent Postgres connections.
    pub postgres_pool_size: usize,

    /// Incremental per-family backend selection.
    pub analytics_backends: AnalyticsBackendSelection,

    /// Valid API tokens (loaded from PENSIEVE_API_TOKENS).
    pub api_tokens: HashSet<String>,

    /// Path to the relay stats SQLite database (optional).
    pub relay_db_path: Option<PathBuf>,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// Required environment variables:
    /// - `PENSIEVE_API_TOKENS`: Comma-separated list of valid API tokens
    ///
    /// Optional environment variables:
    /// - `PENSIEVE_BIND_ADDR`: Server bind address (default: "0.0.0.0:3000")
    /// - `CLICKHOUSE_URL`: ClickHouse URL (default: "http://localhost:8123")
    /// - `CLICKHOUSE_DATABASE`: Database name (default: "nostr")
    /// - `DATABASE_URL`: Postgres URL (required for selected Postgres families)
    /// - `POSTGRES_ANALYTICS_PASSWORD`: Optional Postgres password override
    /// - `PENSIEVE_POSTGRES_API_FAMILIES`: Comma-separated cutover families
    /// - `PENSIEVE_POSTGRES_POOL_SIZE`: Persistent connection count (default: 4)
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr =
            std::env::var("PENSIEVE_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

        let clickhouse_url =
            std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".to_string());

        let clickhouse_database =
            std::env::var("CLICKHOUSE_DATABASE").unwrap_or_else(|_| "nostr".to_string());

        let analytics_backends = AnalyticsBackendSelection::parse(
            std::env::var("PENSIEVE_POSTGRES_API_FAMILIES")
                .ok()
                .as_deref(),
        )?;
        let postgres_url = std::env::var("DATABASE_URL").ok();
        if analytics_backends.any_postgres() && postgres_url.is_none() {
            bail!("DATABASE_URL is required when a Postgres API family is selected");
        }
        let postgres_password = std::env::var("POSTGRES_ANALYTICS_PASSWORD").ok();
        let postgres_pool_size = std::env::var("PENSIEVE_POSTGRES_POOL_SIZE")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("PENSIEVE_POSTGRES_POOL_SIZE must be an integer")?
            .unwrap_or(4);
        if !(1..=32).contains(&postgres_pool_size) {
            bail!("PENSIEVE_POSTGRES_POOL_SIZE must be between 1 and 32");
        }

        let tokens_str = std::env::var("PENSIEVE_API_TOKENS")
            .map_err(|_| anyhow::anyhow!("PENSIEVE_API_TOKENS environment variable is required"))?;

        let api_tokens: HashSet<String> = tokens_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if api_tokens.is_empty() {
            anyhow::bail!("PENSIEVE_API_TOKENS must contain at least one token");
        }

        // Optional: path to relay stats SQLite database
        let relay_db_path = std::env::var("RELAY_DB_PATH")
            .ok()
            .map(PathBuf::from)
            .filter(|p| p.exists());

        tracing::info!(
            bind_addr = %bind_addr,
            token_count = api_tokens.len(),
            relay_db = ?relay_db_path,
            postgres_families = ?analytics_backends.names(),
            postgres_pool_size,
            "configuration loaded"
        );

        Ok(Self {
            bind_addr,
            clickhouse_url,
            clickhouse_database,
            postgres_url,
            postgres_password,
            postgres_pool_size,
            analytics_backends,
            api_tokens,
            relay_db_path,
        })
    }
}

struct PostgresAnalyticsInner {
    config: PostgresConfig,
    slots: Vec<RwLock<Option<Arc<PostgresClient>>>>,
    next_slot: AtomicUsize,
}

/// Bounded reconnecting Postgres connection set.
#[derive(Clone)]
pub struct PostgresAnalytics {
    inner: Arc<PostgresAnalyticsInner>,
}

impl PostgresAnalytics {
    async fn connect(url: &str, password: Option<&str>, pool_size: usize) -> anyhow::Result<Self> {
        let mut config: PostgresConfig = url.parse().context("parse Postgres DATABASE_URL")?;
        if let Some(password) = password {
            config.password(password);
        }
        let analytics = Self {
            inner: Arc::new(PostgresAnalyticsInner {
                config,
                slots: (0..pool_size).map(|_| RwLock::new(None)).collect(),
                next_slot: AtomicUsize::new(0),
            }),
        };
        analytics
            .client()
            .await?
            .simple_query("SELECT 1")
            .await
            .context("preflight Postgres analytics query")?;
        Ok(analytics)
    }

    /// Obtain one live client, reconnecting its bounded slot when necessary.
    pub async fn client(&self) -> anyhow::Result<Arc<PostgresClient>> {
        let index = self.inner.next_slot.fetch_add(1, Ordering::Relaxed) % self.inner.slots.len();
        let slot = &self.inner.slots[index];
        if let Some(client) = slot.read().await.as_ref()
            && !client.is_closed()
        {
            return Ok(Arc::clone(client));
        }
        let mut guard = slot.write().await;
        if let Some(client) = guard.as_ref()
            && !client.is_closed()
        {
            return Ok(Arc::clone(client));
        }
        let (client, connection) = self
            .inner
            .config
            .connect(NoTls)
            .await
            .context("connect Postgres analytics client")?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::error!(%error, "Postgres analytics connection ended");
            }
        });
        let client = Arc::new(client);
        *guard = Some(Arc::clone(&client));
        Ok(client)
    }
}

/// Shared application state available to all request handlers.
#[derive(Clone)]
pub struct AppState {
    /// ClickHouse client for database queries.
    pub clickhouse: Client,

    /// Optional Postgres analytics connections for explicitly selected routes.
    postgres: Option<PostgresAnalytics>,

    /// Application configuration.
    pub config: Arc<Config>,

    /// SQLite connection for relay stats (optional, shared via Arc<Mutex>).
    pub relay_db: Option<Arc<Mutex<Connection>>>,

    /// In-memory response cache for expensive queries.
    pub cache: ResponseCache,
}

impl AppState {
    /// Create a new application state from configuration.
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let clickhouse = Client::default()
            .with_url(&config.clickhouse_url)
            .with_database(&config.clickhouse_database);

        let postgres = if config.analytics_backends.any_postgres() {
            Some(
                PostgresAnalytics::connect(
                    config
                        .postgres_url
                        .as_deref()
                        .expect("validated Postgres URL"),
                    config.postgres_password.as_deref(),
                    config.postgres_pool_size,
                )
                .await?,
            )
        } else {
            None
        };

        // Open SQLite connection if path is configured
        // Use immutable=1 to safely read WAL-mode databases without write access
        // to -wal/-shm auxiliary files
        let relay_db = config.relay_db_path.as_ref().and_then(|path| {
            // Convert path to file URI with immutable flag for WAL-mode safety
            // SQLite URI format requires file:///path for absolute POSIX paths
            // Using file:// so that absolute paths (starting with /) produce file:///
            let uri = format!(
                "file://{}?immutable=1",
                path.to_string_lossy().replace('?', "%3F")
            );
            match Connection::open_with_flags(
                &uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            ) {
                Ok(conn) => {
                    tracing::info!(path = %path.display(), "relay database connected (immutable)");
                    Some(Arc::new(Mutex::new(conn)))
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to open relay database");
                    None
                }
            }
        });

        // Create response cache
        let cache = cache::new_cache();
        tracing::info!(
            capacity = cache::DEFAULT_CACHE_CAPACITY,
            ttl_secs = cache::DEFAULT_TTL.as_secs(),
            "response cache initialized"
        );

        Ok(Self {
            clickhouse,
            postgres,
            config: Arc::new(config),
            relay_db,
            cache,
        })
    }

    /// Whether this route family has been explicitly cut over to Postgres.
    pub fn uses_postgres(&self, family: AnalyticsFamily) -> bool {
        self.config.analytics_backends.uses_postgres(family)
    }

    /// Return a live Postgres client for a selected family.
    pub async fn postgres_client(
        &self,
        family: AnalyticsFamily,
    ) -> anyhow::Result<Arc<PostgresClient>> {
        if !self.uses_postgres(family) {
            bail!("Postgres analytics family is not selected");
        }
        self.postgres
            .as_ref()
            .context("Postgres analytics backend is not configured")?
            .client()
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{AnalyticsBackendSelection, AnalyticsFamily};

    #[test]
    fn backend_selection_defaults_to_clickhouse() {
        let selection = AnalyticsBackendSelection::parse(None).unwrap();
        for family in AnalyticsFamily::ALL {
            assert!(!selection.uses_postgres(family));
        }
    }

    #[test]
    fn backend_selection_accepts_independent_families() {
        let selection = AnalyticsBackendSelection::parse(Some("overview,kinds")).unwrap();
        assert!(selection.uses_postgres(AnalyticsFamily::Overview));
        assert!(selection.uses_postgres(AnalyticsFamily::Kinds));
        assert!(!selection.uses_postgres(AnalyticsFamily::Events));
    }

    #[test]
    fn backend_selection_supports_all_and_rejects_unknown_names() {
        let selection = AnalyticsBackendSelection::parse(Some("all")).unwrap();
        for family in AnalyticsFamily::ALL {
            assert!(selection.uses_postgres(family));
        }
        assert!(AnalyticsBackendSelection::parse(Some("publisher")).is_err());
        assert!(AnalyticsBackendSelection::parse(Some("none,overview")).is_err());
    }
}
