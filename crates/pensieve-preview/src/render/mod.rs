//! HTML rendering for Nostr event previews.
//!
//! Each event kind has a specialized renderer that produces a complete HTML page
//! with appropriate Open Graph tags and visual layout.
//!
//! All rendering uses [maud](https://maud.lambda.xyz/) for compile-time HTML
//! generation with automatic XSS protection (all dynamic values are escaped).

pub mod article;
pub mod components;
pub mod content;
pub mod generic;
pub mod note;
pub mod profile;
pub mod repost;
pub mod video;

use std::collections::HashMap;

use maud::Markup;

use crate::query::{self, ProfileMetadata};
use crate::resolve::ResolvedContent;
use crate::state::AppState;

/// Render a resolved Nostr entity into a complete HTML page.
///
/// Dispatches to the appropriate kind-specific renderer based on the
/// resolved content type and event kind.
pub async fn render_page(
    state: &AppState,
    content: &ResolvedContent,
) -> Result<Markup, crate::error::PreviewError> {
    let base_url = &state.config.base_url;
    let site_name = &state.config.site_name;

    match content {
        ResolvedContent::Profile {
            pubkey_hex: _,
            npub,
            nprofile,
            metadata,
            ..
        } => Ok(profile::render(
            content.pubkey_hex_if_profile().unwrap_or_default(),
            npub,
            nprofile,
            metadata,
            base_url,
            site_name,
        )),

        ResolvedContent::Event {
            event,
            author,
            author_npub,
            nevent,
            engagement,
            reply_to_author,
            reply_to_event_id,
            ..
        } => {
            // Collect pubkeys referenced in content for display name resolution
            let referenced_pubkeys = extract_referenced_pubkeys(&event.content, &event.tags);
            let display_names = if referenced_pubkeys.is_empty() {
                HashMap::new()
            } else {
                query::fetch_display_names(&state.clickhouse, &referenced_pubkeys).await?
            };

            // Fetch quoted events for inline card rendering (depth 1 only)
            let referenced_event_ids =
                content::extract_referenced_event_ids(&event.content, &event.tags);
            let quoted_events = fetch_quoted_events(state, &referenced_event_ids).await?;

            match event.kind {
                // Kind 1: Text note
                1 => Ok(note::render(
                    event,
                    author.as_ref(),
                    author_npub,
                    nevent,
                    engagement,
                    reply_to_author.as_deref(),
                    reply_to_event_id.as_deref(),
                    &display_names,
                    &quoted_events,
                    base_url,
                    site_name,
                )),

                // Kind 30023: Long-form article
                30023 => Ok(article::render(
                    event,
                    author.as_ref(),
                    author_npub,
                    nevent,
                    engagement,
                    &display_names,
                    base_url,
                    site_name,
                )),

                // Kinds 34235/34236: Video
                34235 | 34236 => Ok(video::render(
                    event,
                    author.as_ref(),
                    author_npub,
                    nevent,
                    engagement,
                    base_url,
                    site_name,
                )),

                // Kinds 6/16: Repost
                6 | 16 => {
                    // Try to fetch the original reposted event
                    let reposted_id = event
                        .tags
                        .iter()
                        .find(|t| t.len() >= 2 && t[0] == "e")
                        .map(|t| t[1].clone());

                    let (original_event, original_author, original_npub) = if let Some(ref id) =
                        reposted_id
                    {
                        let orig = query::fetch_event_by_id(&state.clickhouse, id).await?;
                        if let Some(ref orig_event) = orig {
                            let orig_profile =
                                query::fetch_profile(&state.clickhouse, &orig_event.pubkey).await?;
                            let orig_meta =
                                orig_profile.map(|p| ProfileMetadata::from_json(&p.content));
                            let orig_npub = nostr::PublicKey::from_hex(&orig_event.pubkey)
                                .ok()
                                .and_then(|pk| nostr::ToBech32::to_bech32(&pk).ok());
                            (Some(orig_event.clone()), orig_meta, orig_npub)
                        } else {
                            (None, None, None)
                        }
                    } else {
                        (None, None, None)
                    };

                    Ok(repost::render(
                        event,
                        author.as_ref(),
                        author_npub,
                        nevent,
                        engagement,
                        original_event.as_ref(),
                        original_author.as_ref(),
                        original_npub.as_deref(),
                        &display_names,
                        &quoted_events,
                        base_url,
                        site_name,
                    ))
                }

                // All other kinds: generic renderer
                _ => Ok(generic::render(
                    event,
                    author.as_ref(),
                    author_npub,
                    nevent,
                    engagement,
                    &display_names,
                    &quoted_events,
                    base_url,
                    site_name,
                )),
            }
        }
    }
}

/// Extract pubkeys referenced in content (via nostr: URIs) and tags.
fn extract_referenced_pubkeys(content: &str, tags: &[Vec<String>]) -> Vec<String> {
    use nostr::nips::nip19::{FromBech32, Nip19};
    use std::collections::HashSet;
    use std::sync::LazyLock;

    static NOSTR_PUBKEY_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"nostr:(npub1|nprofile1)[a-z0-9]+").unwrap()
    });

    let mut pubkeys = HashSet::new();

    // Extract from nostr: URIs in content
    for cap in NOSTR_PUBKEY_RE.find_iter(content) {
        let bech32 = cap.as_str().strip_prefix("nostr:").unwrap_or(cap.as_str());
        if let Ok(nip19) = Nip19::from_bech32(bech32) {
            match nip19 {
                Nip19::Pubkey(pk) => {
                    pubkeys.insert(pk.to_hex());
                }
                Nip19::Profile(p) => {
                    pubkeys.insert(p.public_key.to_hex());
                }
                _ => {}
            }
        }
    }

    // Extract from p tags
    for tag in tags {
        if tag.len() >= 2 && tag[0] == "p" && tag[1].len() == 64 {
            pubkeys.insert(tag[1].clone());
        }
    }

    pubkeys.into_iter().collect()
}

/// Fetch quoted events for inline card rendering.
///
/// Only fetches events that exist in the database. This is depth-1 only —
/// the content of quoted events is rendered as plain text, not recursively embedded.
/// All quoted events are fetched in parallel for performance.
async fn fetch_quoted_events(
    state: &crate::state::AppState,
    event_ids: &[String],
) -> Result<HashMap<String, content::QuotedEvent>, crate::error::PreviewError> {
    if event_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Fetch all quoted events in parallel
    let futures: Vec<_> = event_ids
        .iter()
        .map(|id| async {
            let row = query::fetch_event_by_id(&state.clickhouse, id).await;
            (id.clone(), row)
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    // For events that exist, fetch their author profiles in parallel
    let found: Vec<_> = results
        .into_iter()
        .filter_map(|(id, result)| match result {
            Ok(Some(row)) => Some((id, row)),
            _ => None,
        })
        .collect();

    let profile_futures: Vec<_> = found
        .iter()
        .map(|(id, row)| async {
            let profile = query::fetch_profile(&state.clickhouse, &row.pubkey).await;
            (id.clone(), profile)
        })
        .collect();

    let profile_results = futures::future::join_all(profile_futures).await;

    let profiles: HashMap<String, Option<ProfileMetadata>> = profile_results
        .into_iter()
        .map(|(id, result)| {
            let meta = result
                .ok()
                .flatten()
                .map(|p| ProfileMetadata::from_json(&p.content));
            (id, meta)
        })
        .collect();

    let quoted = found
        .into_iter()
        .map(|(id, row)| {
            let author_meta = profiles.get(&id).cloned().flatten();
            (
                id,
                content::QuotedEvent {
                    id: row.id,
                    pubkey: row.pubkey,
                    content: row.content,
                    created_at: row.created_at,
                    author: author_meta,
                },
            )
        })
        .collect();

    Ok(quoted)
}

/// Helper trait extension for `ResolvedContent`.
impl ResolvedContent {
    /// Get the pubkey hex if this is a profile.
    fn pubkey_hex_if_profile(&self) -> Option<&str> {
        match self {
            Self::Profile { pubkey_hex, .. } => Some(pubkey_hex),
            _ => None,
        }
    }
}
