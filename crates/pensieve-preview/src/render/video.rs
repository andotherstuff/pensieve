//! Video (kinds 34235/34236) renderer.
//!
//! Renders video events with thumbnail, title, author info,
//! and engagement counts.

use maud::{Markup, html};

use super::components::{
    OpenGraphData, author_header, engagement_bar, is_safe_url, kind_badge, nostr_link, page_shell,
    truncate,
};
use crate::query::{EngagementCounts, EventRow, ProfileMetadata};

/// Render a video preview page.
#[allow(clippy::too_many_arguments)]
pub fn render(
    event: &EventRow,
    author: Option<&ProfileMetadata>,
    author_npub: &str,
    nevent: &str,
    engagement: &EngagementCounts,
    base_url: &str,
    site_name: &str,
) -> Markup {
    let video_title = if !event.title.is_empty() {
        &event.title
    } else {
        "Untitled Video"
    };

    let author_name = author.map(|a| a.display_name()).unwrap_or("Anonymous");
    let title = format!("{video_title} - {author_name}");
    let description = truncate(&event.content, 200);
    let canonical = format!("{base_url}/{nevent}");

    let thumbnail = if !event.thumbnail.is_empty() {
        Some(event.thumbnail.as_str())
    } else {
        None
    }
    .filter(|u| is_safe_url(u));

    let og_image = thumbnail.or_else(|| {
        author
            .and_then(|a| a.picture.as_deref())
            .filter(|u| is_safe_url(u))
    });

    let og = OpenGraphData {
        title: &title,
        description: &description,
        og_type: "video.other",
        image: og_image,
        twitter_card_type: "summary_large_image",
    };

    let video_src = if !event.video_url.is_empty() {
        Some(event.video_url.as_str())
    } else {
        None
    };

    let body = html! {
        div class="card" {
            div class="card-body" {
                (kind_badge(event.kind))

                h1 class="article-title" { (video_title) }

                (author_header(author, author_npub, base_url))

                @if !event.content.is_empty() {
                    p class="article-summary" { (truncate(&event.content, 300)) }
                }

                // Video player or thumbnail below content
                @if let Some(src) = video_src {
                    div class="video-inline" {
                        video controls="" preload="metadata" poster=(thumbnail.unwrap_or("")) {
                            source src=(src) type="video/mp4";
                        }
                    }
                } @else if let Some(thumb_url) = thumbnail {
                    div class="video-thumb" {
                        img src=(thumb_url) alt=(video_title) loading="lazy";
                        div class="video-play" {}
                    }
                }

                (engagement_bar(engagement, event.created_at))
            }
        }
        (nostr_link(nevent))
    };

    page_shell(&title, &description, &canonical, og, body, site_name)
}
