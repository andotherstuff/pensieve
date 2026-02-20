//! ClickHouse query layer for fetching events, profiles, and engagement data.
//!
//! All queries are point lookups by primary key or small result sets,
//! designed for sub-10ms response times.

use clickhouse::Client;
use serde::Deserialize;

use crate::error::PreviewError;

/// A row from the `events_local` table.
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
pub struct EventRow {
    /// Event ID (hex).
    pub id: String,
    /// Author public key (hex).
    pub pubkey: String,
    /// Unix timestamp of event creation.
    pub created_at: u32,
    /// Event kind number.
    pub kind: u16,
    /// Event content (text, JSON, etc.).
    pub content: String,
    /// Event tags as nested arrays.
    pub tags: Vec<Vec<String>>,
    /// Materialized d_tag for replaceable events.
    pub d_tag: String,
    /// Materialized title (for articles/videos).
    pub title: String,
    /// Materialized thumbnail URL.
    pub thumbnail: String,
    /// Materialized video URL.
    pub video_url: String,
}

/// A row from the `user_profiles` view (latest kind 0 content per pubkey).
#[derive(Debug, Clone, Deserialize, clickhouse::Row)]
pub struct ProfileRow {
    /// Public key (hex).
    pub pubkey: String,
    /// Event ID of the kind 0 event (hex).
    pub event_id: String,
    /// JSON content of the latest kind 0 event.
    pub content: String,
}

/// Parsed profile metadata from kind 0 JSON content.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ProfileMetadata {
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// User-facing display name (takes priority over name).
    #[serde(default)]
    pub display_name: Option<String>,
    /// Short biography/description.
    #[serde(default)]
    pub about: Option<String>,
    /// Profile picture URL.
    #[serde(default)]
    pub picture: Option<String>,
    /// Banner image URL.
    #[serde(default)]
    pub banner: Option<String>,
    /// NIP-05 identifier (e.g., "user@domain.com").
    #[serde(default)]
    pub nip05: Option<String>,
    /// Lightning address for zaps.
    #[serde(default)]
    pub lud16: Option<String>,
    /// Website URL.
    #[serde(default)]
    pub website: Option<String>,
}

impl ProfileMetadata {
    /// Get the best display name available, falling back through options.
    pub fn display_name(&self) -> &str {
        self.display_name
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("Anonymous")
    }

    /// Parse from kind 0 JSON content string.
    pub fn from_json(content: &str) -> Self {
        serde_json::from_str(content).unwrap_or_default()
    }
}

/// Engagement counts for an event.
#[derive(Debug, Clone, Default)]
pub struct EngagementCounts {
    /// Number of reactions (kind 7).
    pub reactions: u64,
    /// Number of comments/replies (kind 1 referencing this event).
    pub comments: u64,
    /// Number of reposts (kind 6/16).
    pub reposts: u64,
    /// Total zap amount in millisatoshis.
    pub zap_msats: u64,
    /// Number of zaps.
    pub zap_count: u64,
}

/// Fetch a single event by its hex ID.
pub async fn fetch_event_by_id(
    client: &Client,
    event_id: &str,
) -> Result<Option<EventRow>, PreviewError> {
    let result = client
        .query(
            "SELECT id, pubkey, created_at, kind, content, tags, \
             d_tag, title, thumbnail, video_url \
             FROM events_local \
             WHERE id = ? \
             LIMIT 1",
        )
        .bind(event_id)
        .fetch_optional::<EventRow>()
        .await?;

    Ok(result)
}

/// Fetch the latest profile (kind 0) for a pubkey.
pub async fn fetch_profile(
    client: &Client,
    pubkey: &str,
) -> Result<Option<ProfileRow>, PreviewError> {
    let result = client
        .query(
            "SELECT pubkey, argMax(id, created_at) AS event_id, \
             argMax(content, created_at) AS content \
             FROM events_local \
             WHERE pubkey = ? AND kind = 0 \
             GROUP BY pubkey \
             LIMIT 1",
        )
        .bind(pubkey)
        .fetch_optional::<ProfileRow>()
        .await?;

    Ok(result)
}

/// Fetch a replaceable event by (kind, pubkey, d_tag).
///
/// Used for addressable events like NIP-23 articles (kind 30023),
/// videos (kind 34235/34236), etc.
pub async fn fetch_replaceable(
    client: &Client,
    kind: u16,
    pubkey: &str,
    d_tag: &str,
) -> Result<Option<EventRow>, PreviewError> {
    // For replaceable events, get the latest version
    let result = client
        .query(
            "SELECT id, pubkey, created_at, kind, content, tags, \
             d_tag, title, thumbnail, video_url \
             FROM events_local \
             WHERE kind = ? AND pubkey = ? AND d_tag = ? \
             ORDER BY created_at DESC \
             LIMIT 1",
        )
        .bind(kind)
        .bind(pubkey)
        .bind(d_tag)
        .fetch_optional::<EventRow>()
        .await?;

    Ok(result)
}

/// Fetch engagement counts (reactions, comments, reposts) for an event.
pub async fn fetch_engagement(
    client: &Client,
    event_id: &str,
) -> Result<EngagementCounts, PreviewError> {
    // Fetch all three counts in parallel using separate queries
    // (ClickHouse handles these as fast point lookups on SummingMergeTree tables)

    #[derive(Debug, Deserialize, clickhouse::Row)]
    struct CountRow {
        count: u64,
    }

    #[derive(Debug, Deserialize, clickhouse::Row)]
    struct ZapRow {
        total_msats: u64,
        zap_count: u64,
    }

    let (reactions, comments, reposts, zaps) = tokio::try_join!(
        async {
            client
                .query(
                    "SELECT sum(reaction_count) AS count \
                     FROM reaction_counts \
                     WHERE target_event_id = ?",
                )
                .bind(event_id)
                .fetch_optional::<CountRow>()
                .await
                .map(|r| r.map_or(0, |r| r.count))
                .map_err(PreviewError::from)
        },
        async {
            client
                .query(
                    "SELECT sum(comment_count) AS count \
                     FROM comment_counts \
                     WHERE target_event_id = ?",
                )
                .bind(event_id)
                .fetch_optional::<CountRow>()
                .await
                .map(|r| r.map_or(0, |r| r.count))
                .map_err(PreviewError::from)
        },
        async {
            client
                .query(
                    "SELECT sum(repost_count) AS count \
                     FROM repost_counts \
                     WHERE target_event_id = ?",
                )
                .bind(event_id)
                .fetch_optional::<CountRow>()
                .await
                .map(|r| r.map_or(0, |r| r.count))
                .map_err(PreviewError::from)
        },
        async {
            client
                .query(
                    "SELECT sum(z.amount_msats) AS total_msats, count() AS zap_count \
                     FROM events_local e \
                     INNER JOIN zap_amounts_data z ON e.id = z.event_id \
                     WHERE e.kind = 9735 \
                     AND arrayExists(t -> t[1] = 'e' AND t[2] = ?, e.tags)",
                )
                .bind(event_id)
                .fetch_optional::<ZapRow>()
                .await
                .map(|r| {
                    r.unwrap_or(ZapRow {
                        total_msats: 0,
                        zap_count: 0,
                    })
                })
                .map_err(PreviewError::from)
        },
    )?;

    Ok(EngagementCounts {
        reactions,
        comments,
        reposts,
        zap_msats: zaps.total_msats,
        zap_count: zaps.zap_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ProfileMetadata::from_json tests --

    #[test]
    fn profile_metadata_from_valid_json() {
        let json = r#"{"name":"alice","display_name":"Alice","about":"hello","picture":"https://example.com/pic.jpg","nip05":"alice@example.com"}"#;
        let meta = ProfileMetadata::from_json(json);
        assert_eq!(meta.name.as_deref(), Some("alice"));
        assert_eq!(meta.display_name.as_deref(), Some("Alice"));
        assert_eq!(meta.about.as_deref(), Some("hello"));
        assert_eq!(meta.picture.as_deref(), Some("https://example.com/pic.jpg"));
        assert_eq!(meta.nip05.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn profile_metadata_from_invalid_json() {
        let meta = ProfileMetadata::from_json("not json at all");
        assert_eq!(meta.name, None);
        assert_eq!(meta.display_name, None);
    }

    #[test]
    fn profile_metadata_from_empty_string() {
        let meta = ProfileMetadata::from_json("");
        assert_eq!(meta.name, None);
        assert_eq!(meta.display_name, None);
    }

    #[test]
    fn profile_metadata_from_empty_object() {
        let meta = ProfileMetadata::from_json("{}");
        assert_eq!(meta.name, None);
        assert_eq!(meta.display_name, None);
        assert_eq!(meta.about, None);
    }

    #[test]
    fn profile_metadata_from_partial_json() {
        let json = r#"{"name":"bob"}"#;
        let meta = ProfileMetadata::from_json(json);
        assert_eq!(meta.name.as_deref(), Some("bob"));
        assert_eq!(meta.display_name, None);
        assert_eq!(meta.about, None);
        assert_eq!(meta.picture, None);
    }

    #[test]
    fn profile_metadata_from_json_with_extra_fields() {
        let json = r#"{"name":"carol","unknown_field":"ignored","lud16":"carol@tips.com","website":"https://carol.dev"}"#;
        let meta = ProfileMetadata::from_json(json);
        assert_eq!(meta.name.as_deref(), Some("carol"));
        assert_eq!(meta.lud16.as_deref(), Some("carol@tips.com"));
        assert_eq!(meta.website.as_deref(), Some("https://carol.dev"));
    }

    #[test]
    fn profile_metadata_from_json_unicode() {
        let json = r#"{"name":"田中太郎","about":"こんにちは 🎉","display_name":"太郎"}"#;
        let meta = ProfileMetadata::from_json(json);
        assert_eq!(meta.name.as_deref(), Some("田中太郎"));
        assert_eq!(meta.display_name.as_deref(), Some("太郎"));
        assert_eq!(meta.about.as_deref(), Some("こんにちは 🎉"));
    }

    // -- ProfileMetadata::display_name() tests --

    #[test]
    fn display_name_prefers_display_name() {
        let meta = ProfileMetadata {
            name: Some("alice".to_string()),
            display_name: Some("Alice".to_string()),
            ..Default::default()
        };
        assert_eq!(meta.display_name(), "Alice");
    }

    #[test]
    fn display_name_falls_back_to_name() {
        let meta = ProfileMetadata {
            name: Some("alice".to_string()),
            display_name: None,
            ..Default::default()
        };
        assert_eq!(meta.display_name(), "alice");
    }

    #[test]
    fn display_name_anonymous_when_none() {
        let meta = ProfileMetadata::default();
        assert_eq!(meta.display_name(), "Anonymous");
    }

    #[test]
    fn display_name_empty_display_name_still_used() {
        // Empty string is Some(""), which is still returned
        let meta = ProfileMetadata {
            name: Some("alice".to_string()),
            display_name: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(meta.display_name(), "");
    }

    // -- EngagementCounts default --

    #[test]
    fn engagement_counts_default() {
        let counts = EngagementCounts::default();
        assert_eq!(counts.reactions, 0);
        assert_eq!(counts.comments, 0);
        assert_eq!(counts.reposts, 0);
        assert_eq!(counts.zap_msats, 0);
        assert_eq!(counts.zap_count, 0);
    }
}

/// Batch-fetch display names for a set of pubkeys.
///
/// Returns a map of pubkey (hex) -> display name. Pubkeys without
/// a profile will not be included in the result.
pub async fn fetch_display_names(
    client: &Client,
    pubkeys: &[String],
) -> Result<std::collections::HashMap<String, String>, PreviewError> {
    if pubkeys.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    #[derive(Debug, Deserialize, clickhouse::Row)]
    struct NameRow {
        pubkey: String,
        content: String,
    }

    // Build a query with IN clause for batch lookup
    let placeholders: Vec<&str> = pubkeys.iter().map(|_| "?").collect();
    let query_str = format!(
        "SELECT pubkey, argMax(content, created_at) AS content \
         FROM events_local \
         WHERE pubkey IN ({}) AND kind = 0 \
         GROUP BY pubkey",
        placeholders.join(", ")
    );

    let mut query = client.query(&query_str);
    for pk in pubkeys {
        query = query.bind(pk);
    }

    let rows = query.fetch_all::<NameRow>().await?;

    let names = rows
        .into_iter()
        .filter_map(|row| {
            let meta = ProfileMetadata::from_json(&row.content);
            let name = meta.display_name().to_string();
            if name == "Anonymous" {
                None
            } else {
                Some((row.pubkey, name))
            }
        })
        .collect();

    Ok(names)
}
