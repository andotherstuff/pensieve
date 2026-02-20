//! Text note (kind 1) renderer.
//!
//! Renders a short-form text note with author info, parsed content
//! (URLs, images, nostr: mentions), engagement counts, and timestamp.

use std::collections::HashMap;

use maud::{Markup, html};

use super::components::{
    OpenGraphData, author_header, engagement_bar, nostr_link, page_shell, truncate,
};
use super::content::{QuotedEvent, render_content};
use crate::query::{EngagementCounts, EventRow, ProfileMetadata};

/// Render a text note preview page.
#[allow(clippy::too_many_arguments)]
pub fn render(
    event: &EventRow,
    author: Option<&ProfileMetadata>,
    author_npub: &str,
    nevent: &str,
    engagement: &EngagementCounts,
    reply_to_author: Option<&str>,
    reply_to_event_id: Option<&str>,
    display_names: &HashMap<String, String>,
    quoted_events: &HashMap<String, QuotedEvent>,
    base_url: &str,
    site_name: &str,
) -> Markup {
    let author_name = author.map(|a| a.display_name()).unwrap_or("Anonymous");
    let title = format!("{author_name} on Nostr");
    let description = truncate(&event.content, 200);
    let canonical = format!("{base_url}/{nevent}");

    let og_image = author
        .and_then(|a| a.picture.as_deref())
        .filter(|u| super::components::is_safe_url(u));

    let og = OpenGraphData {
        title: &title,
        description: &description,
        og_type: "article",
        image: og_image,
        twitter_card_type: "summary",
    };

    let body = html! {
        div class="card" {
            div class="card-body" {
                (author_header(author, author_npub, base_url))

                @if let Some(reply_author) = reply_to_author {
                    div class="reply-context" {
                        "Replying to "
                        @if let Some(eid) = reply_to_event_id {
                            a href={(base_url) "/" (eid)} { "@" (reply_author) }
                        } @else {
                            span { "@" (reply_author) }
                        }
                    }
                }

                (render_content(&event.content, display_names, base_url, &event.tags, quoted_events))

                (engagement_bar(engagement, event.created_at))
            }
        }
        (nostr_link(nevent))
    };

    page_shell(&title, &description, &canonical, og, body, site_name)
}
