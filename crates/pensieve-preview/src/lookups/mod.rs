//! Compact ClickHouse lookups for preview reads.
//!
//! Profiles, zap totals and naddr coordinates can be read either from
//! `events_local` (legacy, projection-dependent) or from the compact lookup
//! tables created by `docs/migrations/prepared/017_preview_compact_lookups.sql`.
//! [`Lookups`] picks the source per lookup from [`CompactLookups`], which is
//! configured with `PREVIEW_COMPACT_LOOKUPS`, so each switch is reversible by a
//! restart while the legacy reads stay available.
//!
//! The submodules other than [`compact`] implement the `preview-lookups`
//! operator tool: bounded backfill, validation against the legacy reads, and a
//! latency/storage benchmark. See `docs/preview_compact_lookups.md`.

pub mod backfill;
pub mod bench;
pub mod compact;
pub mod ops;
pub mod oracle;
pub mod plan;
pub mod status;
pub mod target;
pub mod validate;

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::time::Instant;

use clickhouse::Client;

use crate::error::PreviewError;
use crate::query::{self, EngagementCounts, EventRow, ProfileRow};

/// Server-side execution cap for compact lookups, in seconds.
const COMPACT_MAX_EXECUTION_SECS: &str = "5";

/// Lookups slower than this are logged at WARN.
const SLOW_LOOKUP_MS: u128 = 1_000;

/// Which preview lookups read the compact tables instead of `events_local`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactLookups {
    /// Kind 0 profiles and display names from `profile_latest`.
    pub profiles: bool,
    /// Zap totals from `zap_receipts_by_target`.
    pub zaps: bool,
    /// naddr coordinates from `addressable_latest`.
    pub addressable: bool,
}

impl CompactLookups {
    /// Every lookup uses the compact tables.
    pub const ALL: Self = Self {
        profiles: true,
        zaps: true,
        addressable: true,
    };

    /// Every lookup uses the legacy `events_local` reads.
    pub const NONE: Self = Self {
        profiles: false,
        zaps: false,
        addressable: false,
    };
}

/// Error returned for an unrecognised `PREVIEW_COMPACT_LOOKUPS` value.
#[derive(Debug, thiserror::Error)]
#[error(
    "invalid compact lookup setting {0:?}: expected `none`, `all`, or a comma list of `profiles`, `zaps`, `addressable`"
)]
pub struct ParseCompactLookupsError(String);

impl FromStr for CompactLookups {
    type Err = ParseCompactLookupsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let tokens: Vec<String> = value
            .split(',')
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        match tokens.as_slice() {
            [] => return Ok(Self::NONE),
            [only] if only == "none" => return Ok(Self::NONE),
            [only] if only == "all" => return Ok(Self::ALL),
            _ => {}
        }

        let mut lookups = Self::NONE;
        for token in &tokens {
            match token.as_str() {
                "profiles" => lookups.profiles = true,
                "zaps" => lookups.zaps = true,
                "addressable" => lookups.addressable = true,
                _ => return Err(ParseCompactLookupsError(value.to_string())),
            }
        }
        Ok(lookups)
    }
}

impl fmt::Display for CompactLookups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::NONE {
            return f.write_str("none");
        }
        if *self == Self::ALL {
            return f.write_str("all");
        }
        let names: Vec<&str> = [
            (self.profiles, "profiles"),
            (self.zaps, "zaps"),
            (self.addressable, "addressable"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        f.write_str(&names.join(","))
    }
}

/// Preview read layer that routes each lookup to the legacy or compact source.
///
/// Every query carries `log_comment = 'pensieve-preview:<lookup>:<source>'`
/// so `system.query_log` can compare latency per lookup and source during
/// rollout.
#[derive(Clone)]
pub struct Lookups {
    client: Client,
    compact: CompactLookups,
}

impl Lookups {
    /// Create a read layer over `client` with the given compact selection.
    pub fn new(client: Client, compact: CompactLookups) -> Self {
        Self { client, compact }
    }

    /// The configured compact selection.
    pub fn compact(&self) -> CompactLookups {
        self.compact
    }

    fn client_for(&self, lookup: &str, compact: bool) -> Client {
        let source = if compact { "compact" } else { "legacy" };
        let client = self
            .client
            .clone()
            .with_option("log_comment", format!("pensieve-preview:{lookup}:{source}"));
        if compact {
            client
                .with_option("max_execution_time", COMPACT_MAX_EXECUTION_SECS)
                .with_option("timeout_overflow_mode", "throw")
        } else {
            client
        }
    }

    /// Fetch a single event by id (primary-key read; no compact variant).
    pub async fn fetch_event_by_id(
        &self,
        event_id: &str,
    ) -> Result<Option<EventRow>, PreviewError> {
        let client = self.client_for("event", false);
        timed("event", false, query::fetch_event_by_id(&client, event_id)).await
    }

    /// Fetch the latest kind 0 profile for a pubkey.
    pub async fn fetch_profile(&self, pubkey: &str) -> Result<Option<ProfileRow>, PreviewError> {
        let compact = self.compact.profiles;
        let client = self.client_for("profile", compact);
        if compact {
            timed("profile", true, compact::fetch_profile(&client, pubkey)).await
        } else {
            timed("profile", false, query::fetch_profile(&client, pubkey)).await
        }
    }

    /// Batch-fetch latest kind 0 profiles.
    pub async fn fetch_profiles(
        &self,
        pubkeys: &[String],
    ) -> Result<HashMap<String, ProfileRow>, PreviewError> {
        let compact = self.compact.profiles;
        let client = self.client_for("profiles", compact);
        if compact {
            timed("profiles", true, compact::fetch_profiles(&client, pubkeys)).await
        } else {
            timed("profiles", false, query::fetch_profiles(&client, pubkeys)).await
        }
    }

    /// Batch-fetch display names.
    pub async fn fetch_display_names(
        &self,
        pubkeys: &[String],
    ) -> Result<HashMap<String, String>, PreviewError> {
        let compact = self.compact.profiles;
        let client = self.client_for("display_names", compact);
        if compact {
            timed(
                "display_names",
                true,
                compact::fetch_display_names(&client, pubkeys),
            )
            .await
        } else {
            timed(
                "display_names",
                false,
                query::fetch_display_names(&client, pubkeys),
            )
            .await
        }
    }

    /// Fetch the latest event for an naddr coordinate.
    ///
    /// Kinds outside the `addressable_latest` coverage always use the legacy
    /// query.
    pub async fn fetch_replaceable(
        &self,
        kind: u16,
        pubkey: &str,
        d_tag: &str,
    ) -> Result<Option<EventRow>, PreviewError> {
        let compact = self.compact.addressable && compact::addressable_kind_covered(kind);
        let client = self.client_for("replaceable", compact);
        if compact {
            timed(
                "replaceable",
                true,
                compact::fetch_replaceable(&client, kind, pubkey, d_tag),
            )
            .await
        } else {
            timed(
                "replaceable",
                false,
                query::fetch_replaceable(&client, kind, pubkey, d_tag),
            )
            .await
        }
    }

    /// Fetch engagement counts; zap totals come from the configured source.
    pub async fn fetch_engagement(&self, event_id: &str) -> Result<EngagementCounts, PreviewError> {
        let compact = self.compact.zaps;
        let counts_client = self.client_for("engagement", false);
        let zap_client = self.client_for("zaps", compact);
        let zaps = async {
            if compact {
                timed(
                    "zaps",
                    true,
                    compact::fetch_zap_totals(&zap_client, event_id),
                )
                .await
            } else {
                timed(
                    "zaps",
                    false,
                    query::fetch_zap_totals(&zap_client, event_id),
                )
                .await
            }
        };
        query::fetch_engagement_with_zaps(&counts_client, event_id, zaps).await
    }
}

async fn timed<T>(
    lookup: &'static str,
    compact: bool,
    work: impl std::future::Future<Output = Result<T, PreviewError>>,
) -> Result<T, PreviewError> {
    let started = Instant::now();
    let result = work.await;
    let elapsed_ms = started.elapsed().as_millis();
    let source = if compact { "compact" } else { "legacy" };
    if elapsed_ms >= SLOW_LOOKUP_MS {
        tracing::warn!(
            lookup,
            source,
            elapsed_ms,
            ok = result.is_ok(),
            "slow preview lookup"
        );
    } else {
        tracing::debug!(
            lookup,
            source,
            elapsed_ms,
            ok = result.is_ok(),
            "preview lookup"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_none_and_empty() {
        assert_eq!("".parse::<CompactLookups>().unwrap(), CompactLookups::NONE);
        assert_eq!(
            " , ".parse::<CompactLookups>().unwrap(),
            CompactLookups::NONE
        );
        assert_eq!(
            "none".parse::<CompactLookups>().unwrap(),
            CompactLookups::NONE
        );
        assert_eq!(
            "NONE".parse::<CompactLookups>().unwrap(),
            CompactLookups::NONE
        );
    }

    #[test]
    fn parse_all() {
        assert_eq!(
            "all".parse::<CompactLookups>().unwrap(),
            CompactLookups::ALL
        );
        assert_eq!(
            "profiles,zaps,addressable"
                .parse::<CompactLookups>()
                .unwrap(),
            CompactLookups::ALL
        );
    }

    #[test]
    fn parse_subsets() {
        let profiles = "profiles".parse::<CompactLookups>().unwrap();
        assert!(profiles.profiles && !profiles.zaps && !profiles.addressable);

        let two = " Zaps , addressable ".parse::<CompactLookups>().unwrap();
        assert!(!two.profiles && two.zaps && two.addressable);

        let repeated = "profiles,profiles".parse::<CompactLookups>().unwrap();
        assert!(repeated.profiles && !repeated.zaps);
    }

    #[test]
    fn parse_rejects_unknown_and_mixed_keywords() {
        assert!("profile".parse::<CompactLookups>().is_err());
        assert!("none,profiles".parse::<CompactLookups>().is_err());
        assert!("all,zaps".parse::<CompactLookups>().is_err());
        assert!("yes".parse::<CompactLookups>().is_err());
    }

    #[test]
    fn display_round_trips() {
        for value in [
            "none",
            "all",
            "profiles",
            "zaps",
            "addressable",
            "profiles,zaps",
        ] {
            let parsed = value.parse::<CompactLookups>().unwrap();
            assert_eq!(parsed.to_string(), value);
            assert_eq!(
                parsed.to_string().parse::<CompactLookups>().unwrap(),
                parsed
            );
        }
        let zaps_addr = "addressable,zaps".parse::<CompactLookups>().unwrap();
        assert_eq!(zaps_addr.to_string(), "zaps,addressable");
    }
}
