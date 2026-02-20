//! Generic fallback renderer for any unrecognized event kind.
//!
//! Shows the event kind, raw content (truncated), author info, and timestamp.

use std::collections::HashMap;

use maud::{Markup, html};

use super::components::{
    OpenGraphData, author_header, engagement_bar, kind_badge, nostr_link, page_shell, truncate,
};
use super::content::{QuotedEvent, render_content};
use crate::query::{EngagementCounts, EventRow, ProfileMetadata};

/// Render a generic event preview page.
#[allow(clippy::too_many_arguments)]
pub fn render(
    event: &EventRow,
    author: Option<&ProfileMetadata>,
    author_npub: &str,
    nevent: &str,
    engagement: &EngagementCounts,
    display_names: &HashMap<String, String>,
    quoted_events: &HashMap<String, QuotedEvent>,
    base_url: &str,
    site_name: &str,
) -> Markup {
    let author_name = author.map(|a| a.display_name()).unwrap_or("Anonymous");
    let title = format!("Nostr Event (Kind {}) by {author_name}", event.kind);
    let description = if event.content.is_empty() {
        format!("Kind {} event on Nostr", event.kind)
    } else {
        truncate(&event.content, 200)
    };
    let canonical = format!("{base_url}/{nevent}");

    let og_image = author
        .and_then(|a| a.picture.as_deref())
        .filter(|u| super::components::is_safe_url(u));

    let og = OpenGraphData {
        title: &title,
        description: &description,
        og_type: "website",
        image: og_image,
        twitter_card_type: "summary",
    };

    let body = html! {
        div class="card" {
            div class="card-body" {
                (kind_badge(event.kind))
                (author_header(author, author_npub, base_url))

                @if !event.content.is_empty() {
                    (render_content(&event.content, display_names, base_url, &event.tags, quoted_events))
                } @else {
                    p class="content" {
                        em { "This event has no text content." }
                    }
                }

                (engagement_bar(engagement, event.created_at))
            }
        }
        (nostr_link(nevent))
    };

    page_shell(&title, &description, &canonical, og, body, site_name)
}
