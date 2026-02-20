//! Repost (kinds 6/16) renderer.
//!
//! Renders reposts with a "Reposted by @name" header and the original
//! event content below.

use std::collections::HashMap;

use maud::{html, Markup};

use super::components::{
    author_header, engagement_bar, nostr_link, page_shell, truncate, truncate_key, OpenGraphData,
};
use super::content::{render_content, QuotedEvent};
use crate::query::{EngagementCounts, EventRow, ProfileMetadata};

/// Render a repost preview page.
///
/// If the original event is available, it's rendered inline.
/// Otherwise, we show the repost with a reference link.
#[allow(clippy::too_many_arguments)]
pub fn render(
    event: &EventRow,
    author: Option<&ProfileMetadata>,
    author_npub: &str,
    nevent: &str,
    engagement: &EngagementCounts,
    original_event: Option<&EventRow>,
    original_author: Option<&ProfileMetadata>,
    original_author_npub: Option<&str>,
    display_names: &HashMap<String, String>,
    quoted_events: &HashMap<String, QuotedEvent>,
    base_url: &str,
    site_name: &str,
) -> Markup {
    let reposter_name = author.map(|a| a.display_name()).unwrap_or("Someone");

    let (title, description) = if let Some(orig) = original_event {
        let orig_author_name = original_author
            .map(|a| a.display_name())
            .unwrap_or("Anonymous");
        (
            format!("{reposter_name} reposted {orig_author_name}"),
            truncate(&orig.content, 200),
        )
    } else {
        (
            format!("{reposter_name} reposted"),
            "Reposted a Nostr event".to_string(),
        )
    };

    let canonical = format!("{base_url}/{nevent}");

    let og_image_url = format!("{base_url}/og/{nevent}.png");

    let og = OpenGraphData {
        title: &title,
        description: &description,
        og_type: "article",
        image: Some(&og_image_url),
        twitter_card_type: "summary_large_image",
    };

    let body = html! {
        div class="card" {
            // Repost header
            div class="repost-header" {
                a href={(base_url) "/" (author_npub)} { (reposter_name) }
                " reposted"
            }

            div class="card-body" {
                @if let Some(orig) = original_event {
                    // Render original event content
                    (author_header(
                        original_author,
                        original_author_npub.unwrap_or(author_npub),
                        base_url,
                    ))
                    (render_content(&orig.content, display_names, base_url, &orig.tags, quoted_events))
                    (engagement_bar(engagement, orig.created_at))
                } @else {
                    // Original event not available
                    p { "Reposted event not found in archive." }
                    @if let Some(ref_id) = get_reposted_event_id(event) {
                        p {
                            "Original event: "
                            a href={(base_url) "/" (ref_id)} { (truncate_key(&ref_id)) }
                        }
                    }
                }

            }
        }
        (nostr_link(nevent))
    };

    page_shell(&title, &description, &canonical, og, body, site_name)
}

/// Extract the reposted event ID from the repost event's tags.
fn get_reposted_event_id(event: &EventRow) -> Option<String> {
    event
        .tags
        .iter()
        .find(|tag| tag.len() >= 2 && tag[0] == "e")
        .map(|tag| tag[1].clone())
}
