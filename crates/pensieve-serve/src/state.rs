//! Application state and configuration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use clickhouse::Client;
use parking_lot::Mutex;
use rusqlite::Connection;

/// Application configuration loaded from environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Server bind address (e.g., "0.0.0.0:3000").
    pub bind_addr: String,

    /// ClickHouse connection URL.
    pub clickhouse_url: String,

    /// ClickHouse database name.
    pub clickhouse_database: String,

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
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr =
            std::env::var("PENSIEVE_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

        let clickhouse_url =
            std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".to_string());

        let clickhouse_database =
            std::env::var("CLICKHOUSE_DATABASE").unwrap_or_else(|_| "nostr".to_string());

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
            clickhouse_url = %clickhouse_url,
            token_count = api_tokens.len(),
            relay_db = ?relay_db_path,
            "configuration loaded"
        );

        Ok(Self {
            bind_addr,
            clickhouse_url,
            clickhouse_database,
            api_tokens,
            relay_db_path,
        })
    }
}

/// Shared application state available to all request handlers.
#[derive(Clone)]
pub struct AppState {
    /// ClickHouse client for database queries.
    pub clickhouse: Client,

    /// Application configuration.
    pub config: Arc<Config>,

    /// SQLite connection for relay stats (optional, shared via Arc<Mutex>).
    pub relay_db: Option<Arc<Mutex<Connection>>>,
}

impl AppState {
    /// Create a new application state from configuration.
    pub fn new(config: Config) -> Self {
        let clickhouse = Client::default()
            .with_url(&config.clickhouse_url)
            .with_database(&config.clickhouse_database);

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
                    tracing::info!("Relay database connected (immutable): {:?}", path);
                    Some(Arc::new(Mutex::new(conn)))
                }
                Err(e) => {
                    tracing::warn!("Failed to open relay database {:?}: {}", path, e);
                    None
                }
            }
        });

        Self {
            clickhouse,
            config: Arc::new(config),
            relay_db,
        }
    }
}

