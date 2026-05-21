//! JSON endpoint for raw event data.
//!
//! Serves the raw Nostr event as JSON at `GET /{identifier}.json`.
//! Designed for LLM agents and programmatic consumers.
//!
//! Format:
//! ```json
//! {
//!   "event": { /* raw nostr event */ },
//!   "author_metadata": { /* kind 0 profile of the author */ },
//!   "mentions_metadata": { "pubkey_hex": { /* kind 0 profile */ }, ... },
//!   "engagement": { "reactions": N, "comments": N, "reposts": N, "zap_msats": N, "zap_count": N }
//! }
//! ```

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::error::PreviewError;
use crate::query::{self, ProfileMetadata};
use crate::resolve;
use crate::state::AppState;

/// Build a JSON metadata object from a `ProfileMetadata` and optional event ID.
fn metadata_json(meta: &ProfileMetadata, event_id: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "metadata_event_id": event_id,
        "name": meta.name,
        "display_name": meta.display_name,
        "about": meta.about,
        "picture": meta.picture,
        "banner": meta.banner,
        "nip05": meta.nip05,
        "lud16": meta.lud16,
        "website": meta.website,
    })
}

/// Inner handler called from the preview handler when `.json` suffix is detected.
pub async fn json_handler_inner(
    state: &AppState,
    identifier: &str,
) -> Result<Response, PreviewError> {
    let content = resolve::resolve(state, identifier).await?;

    let json_body = match &content {
        resolve::ResolvedContent::Profile {
            pubkey_hex,
            npub,
            metadata,
            profile_event_id,
            ..
        } => {
            serde_json::json!({
                "event": {
                    "kind": 0,
                    "pubkey": pubkey_hex,
                    "content": serde_json::to_string(metadata).unwrap_or_default(),
                },
                "author_metadata": metadata_json(metadata, profile_event_id.as_deref()),
                "mentions_metadata": {},
                "engagement": null,
                "npub": npub,
            })
        }
        resolve::ResolvedContent::Event {
            event,
            author,
            author_profile_event_id,
            engagement,
            ..
        } => {
            // Build mentions_metadata: resolve p-tagged pubkeys' profiles.
            // Cap the count and batch into a single query so a malicious event
            // with thousands of `p` tags can't trigger an N+1 query amplification.
            const MAX_MENTIONS: usize = 50;
            let mut mentioned_pubkeys: Vec<String> = event
                .tags
                .iter()
                .filter(|t| t.len() >= 2 && t[0] == "p" && t[1].len() == 64)
                .map(|t| t[1].clone())
                .collect();
            mentioned_pubkeys.sort();
            mentioned_pubkeys.dedup();
            mentioned_pubkeys.truncate(MAX_MENTIONS);

            let profiles = query::fetch_profiles(&state.clickhouse, &mentioned_pubkeys)
                .await
                .unwrap_or_default();
            let mut mentions_map = serde_json::Map::new();
            for pk in &mentioned_pubkeys {
                if let Some(profile_row) = profiles.get(pk) {
                    let meta = ProfileMetadata::from_json(&profile_row.content);
                    mentions_map.insert(
                        pk.clone(),
                        metadata_json(&meta, Some(&profile_row.event_id)),
                    );
                }
            }

            let author_meta = author
                .as_ref()
                .map(|m| metadata_json(m, author_profile_event_id.as_deref()));

            serde_json::json!({
                "event": {
                    "id": event.id,
                    "pubkey": event.pubkey,
                    "created_at": event.created_at,
                    "kind": event.kind,
                    "content": event.content,
                    "tags": event.tags,
                },
                "author_metadata": author_meta,
                "mentions_metadata": serde_json::Value::Object(mentions_map),
                "engagement": {
                    "reactions": engagement.reactions,
                    "comments": engagement.comments,
                    "reposts": engagement.reposts,
                    "zap_msats": engagement.zap_msats,
                    "zap_count": engagement.zap_count,
                },
            })
        }
    };

    let json_string =
        serde_json::to_string_pretty(&json_body).map_err(|e| PreviewError::Internal(e.into()))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60, s-maxage=3600, stale-while-revalidate=600"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );

    Ok((StatusCode::OK, headers, json_string).into_response())
}
