//! Application configuration loaded from environment variables and files.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use nostr::nips::nip19::{FromBech32, Nip19};

use crate::lookups::CompactLookups;

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
    /// Loaded from a `.vip` file — one pubkey per line (hex or npub).
    pub vip_pubkeys: Arc<HashSet<String>>,

    /// Which lookups read the compact ClickHouse tables (migration 017)
    /// instead of `events_local`. Defaults to none (legacy reads).
    pub compact_lookups: CompactLookups,

    /// Run every preview query with `optimize_use_projections = 0`, a
    /// rehearsal of production without the `events_by_kind` projection.
    /// Defaults to false.
    pub disable_projections: bool,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// VIP pubkeys are loaded from a `.vip` file (one per line, hex or npub format).
    /// The file path defaults to `.vip` in the working directory, or can be set
    /// via the `PREVIEW_VIP_FILE` environment variable.
    ///
    /// Optional env vars:
    /// - `PREVIEW_BIND_ADDR`: Server bind address (default: "0.0.0.0:8081")
    /// - `CLICKHOUSE_URL`: ClickHouse URL (default: "http://localhost:8123")
    /// - `CLICKHOUSE_DATABASE`: Database name (default: "nostr")
    /// - `PREVIEW_BASE_URL`: Base URL for links/OG tags (default: "http://localhost:8081")
    /// - `PREVIEW_SITE_NAME`: Site name (default: "Nostr Preview")
    /// - `PREVIEW_VIP_FILE`: Path to VIP pubkeys file (default: ".vip")
    /// - `PREVIEW_COMPACT_LOOKUPS`: `none` (default), `all`, or a comma list of
    ///   `profiles`, `zaps`, `addressable`. An unrecognised value is an error.
    /// - `PREVIEW_DISABLE_PROJECTIONS`: `true`/`false` (default `false`). Runs
    ///   every preview query with projections disabled and bounded; see
    ///   [`crate::state::PROJECTIONS_OFF_SETTINGS`].
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

        let vip_file = std::env::var("PREVIEW_VIP_FILE").unwrap_or_else(|_| ".vip".to_string());
        let vip_pubkeys = load_vip_file(&vip_file);

        let compact_lookups = std::env::var("PREVIEW_COMPACT_LOOKUPS")
            .unwrap_or_default()
            .parse::<CompactLookups>()
            .context("PREVIEW_COMPACT_LOOKUPS")?;

        let disable_projections = parse_bool(
            "PREVIEW_DISABLE_PROJECTIONS",
            &std::env::var("PREVIEW_DISABLE_PROJECTIONS").unwrap_or_default(),
        )?;
        if disable_projections && compact_lookups != CompactLookups::ALL {
            tracing::warn!(
                compact_lookups = %compact_lookups,
                "PREVIEW_DISABLE_PROJECTIONS is set while some lookups still use legacy reads; \
                 those will scan events_local and hit the query time cap"
            );
        }

        tracing::info!(
            bind_addr = %bind_addr,
            clickhouse_url = %clickhouse_url,
            base_url = %base_url,
            site_name = %site_name,
            vip_count = vip_pubkeys.len(),
            vip_file = %vip_file,
            compact_lookups = %compact_lookups,
            disable_projections,
            "preview configuration loaded"
        );

        Ok(Self {
            bind_addr,
            clickhouse_url,
            clickhouse_database,
            base_url,
            site_name,
            vip_pubkeys: Arc::new(vip_pubkeys),
            compact_lookups,
            disable_projections,
        })
    }
}

/// Parse a boolean environment value (`1/true/yes/on`, `0/false/no/off`,
/// empty = false).
fn parse_bool(name: &str, value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => anyhow::bail!("{name}: expected true or false, got {other:?}"),
    }
}

/// Load VIP pubkeys from a file.
///
/// The file should contain one pubkey per line. Supported formats:
/// - 64-character hex pubkey
/// - `npub1...` bech32-encoded pubkey
///
/// Empty lines and lines starting with `#` are ignored.
/// If the file doesn't exist, returns an empty set (not an error).
pub fn load_vip_file(path: &str) -> HashSet<String> {
    let path = Path::new(path);
    if !path.exists() {
        return HashSet::new();
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to read VIP file");
            return HashSet::new();
        }
    };

    let mut pubkeys = HashSet::new();
    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Try as npub first
        if line.starts_with("npub1") {
            match Nip19::from_bech32(line) {
                Ok(Nip19::Pubkey(pk)) => {
                    pubkeys.insert(pk.to_hex());
                }
                _ => {
                    tracing::warn!(
                        file = %path.display(),
                        line = line_num + 1,
                        value = line,
                        "invalid npub in VIP file, skipping"
                    );
                }
            }
            continue;
        }

        // Try as 64-char hex
        if line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit()) {
            pubkeys.insert(line.to_lowercase());
            continue;
        }

        tracing::warn!(
            file = %path.display(),
            line = line_num + 1,
            value = line,
            "unrecognized format in VIP file, skipping (expected hex or npub)"
        );
    }

    pubkeys
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ToBech32;
    use std::io::Write;
    use std::sync::Mutex;

    /// Mutex to serialize config tests that manipulate env vars.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    const ENV_KEYS: &[&str] = &[
        "PREVIEW_BIND_ADDR",
        "CLICKHOUSE_URL",
        "CLICKHOUSE_DATABASE",
        "PREVIEW_BASE_URL",
        "PREVIEW_SITE_NAME",
        "PREVIEW_VIP_FILE",
        "PREVIEW_COMPACT_LOOKUPS",
        "PREVIEW_DISABLE_PROJECTIONS",
    ];

    /// Helper to run config tests with isolated env vars.
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
            assert_eq!(config.compact_lookups, CompactLookups::NONE);
            assert!(!config.disable_projections);
        });
    }

    #[test]
    fn config_disable_projections_parsed() {
        for (value, expected) in [
            ("true", true),
            ("1", true),
            ("ON", true),
            ("false", false),
            ("0", false),
            ("", false),
        ] {
            with_env_vars(&[("PREVIEW_DISABLE_PROJECTIONS", value)], || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.disable_projections, expected, "{value:?}");
            });
        }
        with_env_vars(&[("PREVIEW_DISABLE_PROJECTIONS", "maybe")], || {
            assert!(Config::from_env().is_err());
        });
    }

    #[test]
    fn config_compact_lookups_parsed() {
        with_env_vars(&[("PREVIEW_COMPACT_LOOKUPS", "profiles,zaps")], || {
            let config = Config::from_env().unwrap();
            assert!(config.compact_lookups.profiles);
            assert!(config.compact_lookups.zaps);
            assert!(!config.compact_lookups.addressable);
        });
    }

    #[test]
    fn config_compact_lookups_invalid_is_error() {
        with_env_vars(&[("PREVIEW_COMPACT_LOOKUPS", "everything")], || {
            assert!(Config::from_env().is_err());
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
    fn load_vip_file_nonexistent() {
        let result = load_vip_file("/tmp/nonexistent_vip_file_test");
        assert!(result.is_empty());
    }

    #[test]
    fn load_vip_file_hex_pubkeys() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let pk1 = "a".repeat(64);
        let pk2 = "b".repeat(64);
        writeln!(f, "{pk1}").unwrap();
        writeln!(f, "{pk2}").unwrap();

        let result = load_vip_file(f.path().to_str().unwrap());
        assert_eq!(result.len(), 2);
        assert!(result.contains(&pk1));
        assert!(result.contains(&pk2));
    }

    #[test]
    fn load_vip_file_npub_pubkeys() {
        let pk_hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let pk = nostr::PublicKey::from_hex(pk_hex).unwrap();
        let npub = pk.to_bech32().unwrap();

        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{npub}").unwrap();

        let result = load_vip_file(f.path().to_str().unwrap());
        assert_eq!(result.len(), 1);
        assert!(result.contains(pk_hex));
    }

    #[test]
    fn load_vip_file_mixed_formats() {
        let hex_pk = "a".repeat(64);
        let npub_hex = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
        let pk = nostr::PublicKey::from_hex(npub_hex).unwrap();
        let npub = pk.to_bech32().unwrap();

        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "# VIP pubkeys").unwrap();
        writeln!(f, "{hex_pk}").unwrap();
        writeln!(f).unwrap(); // empty line
        writeln!(f, "{npub}").unwrap();
        writeln!(f, "# another comment").unwrap();

        let result = load_vip_file(f.path().to_str().unwrap());
        assert_eq!(result.len(), 2);
        assert!(result.contains(&hex_pk));
        assert!(result.contains(npub_hex));
    }

    #[test]
    fn load_vip_file_skips_invalid() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "tooshort").unwrap();
        writeln!(f, "npub1invalid").unwrap();
        writeln!(f, "not-hex-at-all-{}!", "x".repeat(50)).unwrap();

        let result = load_vip_file(f.path().to_str().unwrap());
        assert!(result.is_empty());
    }

    #[test]
    fn load_vip_file_hex_lowercased() {
        let pk_upper = "A".repeat(64);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{pk_upper}").unwrap();

        let result = load_vip_file(f.path().to_str().unwrap());
        assert_eq!(result.len(), 1);
        assert!(result.contains(&"a".repeat(64)));
    }

    #[test]
    fn load_vip_file_with_env_override() {
        let pk = "c".repeat(64);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{pk}").unwrap();

        with_env_vars(&[("PREVIEW_VIP_FILE", f.path().to_str().unwrap())], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.vip_pubkeys.len(), 1);
            assert!(config.vip_pubkeys.contains(&pk));
        });
    }
}
