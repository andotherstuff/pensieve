//! Application configuration loaded from environment variables.

use std::collections::HashSet;
use std::sync::Arc;

/// Application configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Server bind address (e.g., "0.0.0.0:8081").
    pub bind_addr: String,

    /// ClickHouse connection URL.
    pub clickhouse_url: String,

    /// ClickHouse database name.
    pub clickhouse_database: String,

    /// Base URL for this preview service (used in OG tags and canonical URLs).
    /// e.g., "https://nostr.at" or "https://preview.pensieve.dev"
    pub base_url: String,

    /// Site name shown in OG tags and page titles.
    pub site_name: String,

    /// Set of "VIP" pubkeys (hex) that get longer cache TTLs.
    /// These are high-profile accounts whose pages are accessed frequently.
    pub vip_pubkeys: Arc<HashSet<String>>,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// Required:
    /// - None (all have defaults for local development)
    ///
    /// Optional:
    /// - `PREVIEW_BIND_ADDR`: Server bind address (default: "0.0.0.0:8081")
    /// - `CLICKHOUSE_URL`: ClickHouse URL (default: "http://localhost:8123")
    /// - `CLICKHOUSE_DATABASE`: Database name (default: "nostr")
    /// - `PREVIEW_BASE_URL`: Base URL for links/OG tags (default: "http://localhost:8081")
    /// - `PREVIEW_SITE_NAME`: Site name (default: "Nostr Preview")
    /// - `PREVIEW_VIP_PUBKEYS`: Comma-separated hex pubkeys for extended cache TTL
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr =
            std::env::var("PREVIEW_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());

        let clickhouse_url =
            std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".to_string());

        let clickhouse_database =
            std::env::var("CLICKHOUSE_DATABASE").unwrap_or_else(|_| "nostr".to_string());

        let base_url = std::env::var("PREVIEW_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8081".to_string())
            .trim_end_matches('/')
            .to_string();

        let site_name =
            std::env::var("PREVIEW_SITE_NAME").unwrap_or_else(|_| "Nostr Preview".to_string());

        let vip_pubkeys: HashSet<String> = std::env::var("PREVIEW_VIP_PUBKEYS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty() && s.len() == 64)
            .collect();

        tracing::info!(
            bind_addr = %bind_addr,
            clickhouse_url = %clickhouse_url,
            base_url = %base_url,
            site_name = %site_name,
            vip_count = vip_pubkeys.len(),
            "preview configuration loaded"
        );

        Ok(Self {
            bind_addr,
            clickhouse_url,
            clickhouse_database,
            base_url,
            site_name,
            vip_pubkeys: Arc::new(vip_pubkeys),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize config tests that manipulate env vars.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    const ENV_KEYS: &[&str] = &[
        "PREVIEW_BIND_ADDR",
        "CLICKHOUSE_URL",
        "CLICKHOUSE_DATABASE",
        "PREVIEW_BASE_URL",
        "PREVIEW_SITE_NAME",
        "PREVIEW_VIP_PUBKEYS",
    ];

    /// Helper to run config tests with isolated env vars.
    /// Uses a mutex to prevent concurrent env var races.
    fn with_env_vars<F: FnOnce()>(vars: &[(&str, &str)], f: F) {
        let _guard = ENV_MUTEX.lock().unwrap();

        let saved: Vec<_> = ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();

        // SAFETY: Serialized by mutex; only test code touches these vars.
        unsafe {
            for k in ENV_KEYS {
                std::env::remove_var(k);
            }
            for (k, v) in vars {
                std::env::set_var(k, v);
            }
        }

        f();

        // SAFETY: Restoring original env state.
        unsafe {
            for (k, v) in &saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn config_defaults() {
        with_env_vars(&[], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.bind_addr, "0.0.0.0:8081");
            assert_eq!(config.clickhouse_url, "http://localhost:8123");
            assert_eq!(config.clickhouse_database, "nostr");
            assert_eq!(config.base_url, "http://localhost:8081");
            assert_eq!(config.site_name, "Nostr Preview");
            assert!(config.vip_pubkeys.is_empty());
        });
    }

    #[test]
    fn config_custom_values() {
        with_env_vars(
            &[
                ("PREVIEW_BIND_ADDR", "127.0.0.1:9090"),
                ("CLICKHOUSE_URL", "http://ch:8123"),
                ("CLICKHOUSE_DATABASE", "mydb"),
                ("PREVIEW_BASE_URL", "https://nostr.at"),
                ("PREVIEW_SITE_NAME", "My Nostr"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.bind_addr, "127.0.0.1:9090");
                assert_eq!(config.clickhouse_url, "http://ch:8123");
                assert_eq!(config.clickhouse_database, "mydb");
                assert_eq!(config.base_url, "https://nostr.at");
                assert_eq!(config.site_name, "My Nostr");
            },
        );
    }

    #[test]
    fn config_base_url_trailing_slash_stripped() {
        with_env_vars(&[("PREVIEW_BASE_URL", "https://nostr.at/")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.base_url, "https://nostr.at");
        });
    }

    #[test]
    fn config_vip_pubkeys_parsing() {
        let pk1 = "a".repeat(64);
        let pk2 = "b".repeat(64);
        let vip_str = format!("{pk1}, {pk2}");
        with_env_vars(&[("PREVIEW_VIP_PUBKEYS", &vip_str)], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.vip_pubkeys.len(), 2);
            assert!(config.vip_pubkeys.contains(&pk1));
            assert!(config.vip_pubkeys.contains(&pk2));
        });
    }

    #[test]
    fn config_vip_pubkeys_filters_invalid() {
        with_env_vars(&[("PREVIEW_VIP_PUBKEYS", "short,,,tooshort")], || {
            let config = Config::from_env().unwrap();
            assert!(config.vip_pubkeys.is_empty());
        });
    }

    #[test]
    fn config_vip_pubkeys_empty() {
        with_env_vars(&[("PREVIEW_VIP_PUBKEYS", "")], || {
            let config = Config::from_env().unwrap();
            assert!(config.vip_pubkeys.is_empty());
        });
    }

    #[test]
    fn config_vip_pubkeys_lowercased() {
        let pk_upper = "A".repeat(64);
        with_env_vars(&[("PREVIEW_VIP_PUBKEYS", &pk_upper)], || {
            let config = Config::from_env().unwrap();
            assert!(config.vip_pubkeys.contains(&"a".repeat(64)));
            assert!(!config.vip_pubkeys.contains(&pk_upper));
        });
    }
}
