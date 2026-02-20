//! Content parsing and rendering for note text.
//!
//! Handles:
//! - URL detection and linking
//! - Image URL rendering as `<img>` tags
//! - `nostr:` URI resolution (npub, note, nevent, nprofile, naddr)
//! - Newline preservation
//! - HTML escaping (handled by maud automatically)

use std::collections::HashMap;

use maud::{Markup, html};
use nostr::ToBech32;
use nostr::nips::nip19::{FromBech32, Nip19};
use regex::Regex;
use std::sync::LazyLock;

use super::components::{is_safe_url, truncate};
use crate::query::ProfileMetadata;

/// A pre-fetched event for inline quote card rendering.
#[derive(Debug, Clone)]
pub struct QuotedEvent {
    /// Event ID (hex).
    pub id: String,
    /// Author pubkey (hex).
    pub pubkey: String,
    /// Event content text.
    pub content: String,
    /// Created at (unix timestamp).
    pub created_at: u32,
    /// Author profile metadata (if available).
    pub author: Option<ProfileMetadata>,
}

/// Regex for matching URLs in text content.
static URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>\)\]]+").expect("URL regex should compile"));

/// Regex for matching `nostr:` URIs in text content.
static NOSTR_URI_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"nostr:(npub1|nprofile1|note1|nevent1|naddr1)[a-z0-9]+")
        .expect("nostr URI regex should compile")
});

/// Regex for matching NIP-08 legacy `#[N]` tag references in text content.
/// These are positional references to the event's tags array.
static TAG_REF_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\[(\d+)\]").expect("tag ref regex should compile"));

/// Extract event IDs referenced in content (via nostr:note1/nevent1 URIs and NIP-08 e tags).
///
/// Returns hex event IDs suitable for database lookups.
pub fn extract_referenced_event_ids(text: &str, tags: &[Vec<String>]) -> Vec<String> {
    use std::collections::HashSet;

    let mut ids = HashSet::new();

    // First resolve tag references to get the full text
    let resolved = resolve_tag_references(text, tags);

    // Find nostr:note1... and nostr:nevent1... in the resolved text
    for cap in NOSTR_URI_REGEX.find_iter(&resolved) {
        let bech32 = cap.as_str().strip_prefix("nostr:").unwrap_or(cap.as_str());
        if let Ok(nip19) = Nip19::from_bech32(bech32) {
            match nip19 {
                Nip19::EventId(id) => {
                    ids.insert(id.to_hex());
                }
                Nip19::Event(e) => {
                    ids.insert(e.event_id.to_hex());
                }
                _ => {}
            }
        }
    }

    ids.into_iter().collect()
}

/// Image file extensions for inline rendering.
const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg"];

/// Video file extensions for inline rendering.
const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".webm", ".mov", ".m4v"];

/// Render note content as HTML with links, images, and nostr: URI resolution.
///
/// Images and videos are extracted from the content and rendered separately
/// below the text (images in a grid, videos as players).
///
/// `display_names` maps hex pubkeys to their display names for inline rendering
/// of `nostr:npub1...` mentions.
///
/// `tags` is the event's tags array, used for resolving NIP-08 `#[N]` references.
///
/// `quoted_events` maps event ID (hex) to pre-fetched event data for inline card rendering.
pub fn render_content(
    text: &str,
    display_names: &HashMap<String, String>,
    base_url: &str,
    tags: &[Vec<String>],
    quoted_events: &HashMap<String, QuotedEvent>,
) -> Markup {
    let resolved_text = resolve_tag_references(text, tags);
    let segments = parse_content_segments(&resolved_text);

    // Separate image/video URLs from text content
    let mut image_urls: Vec<String> = Vec::new();
    let mut video_urls: Vec<String> = Vec::new();

    // First pass: collect media URLs
    for segment in &segments {
        if let ContentSegment::Url(url) = segment
            && is_safe_url(url)
        {
            if is_image_url(url) {
                image_urls.push(url.clone());
            } else if is_video_url(url) {
                video_urls.push(url.clone());
            }
        }
    }

    html! {
        // Text content (images/videos stripped out)
        div class="content" {
            @for segment in &segments {
                @match segment {
                    ContentSegment::Text(text) => {
                        (text)
                    }
                    ContentSegment::Url(url) => {
                        // Skip media URLs — they'll be rendered in the grid/player below
                        @if is_image_url(url) || is_video_url(url) {
                            // Skip
                        } @else if is_safe_url(url) {
                            a href=(url) rel="nofollow noopener" target="_blank" {
                                (truncate_url(url))
                            }
                        } @else {
                            (url)
                        }
                    }
                    ContentSegment::NostrUri(uri, bech32) => {
                        @if let Some(rendered) = render_nostr_reference(bech32, display_names, base_url, quoted_events) {
                            (rendered)
                        } @else {
                            (uri)
                        }
                    }
                }
            }
        }

        // Image grid (below text)
        @if !image_urls.is_empty() {
            (render_image_grid(&image_urls))
        }

        // Video players (below images)
        @for video_url in &video_urls {
            div class="video-inline" {
                video controls="" preload="metadata" {
                    source src=(video_url) type="video/mp4";
                }
            }
        }

        // Lightbox overlay (hidden by default, activated by JS)
        div class="lightbox-overlay" id="lightbox" onclick="this.classList.remove('active')" {
            img id="lightbox-img" src="" alt="";
        }
    }
}

/// Render images in a 2x2 grid with +N overflow indicator.
fn render_image_grid(urls: &[String]) -> Markup {
    let count = urls.len();
    let display_count = count.min(4);
    let grid_class = format!("media-grid g{}", display_count.min(4));
    let overflow = if count > 4 { count - 3 } else { 0 };

    html! {
        div class=(grid_class) {
            @for (i, url) in urls.iter().take(display_count).enumerate() {
                div class="media-item" onclick=(format!("document.getElementById('lightbox-img').src='{}';document.getElementById('lightbox').classList.add('active')", url)) {
                    img src=(url) alt="" loading="lazy";
                    // Show +N overlay on the last visible image if there are more
                    @if overflow > 0 && i == display_count - 1 {
                        div class="media-more" { "+" (overflow) }
                    }
                }
            }
        }
    }
}

/// Check if a URL points to a video file.
fn is_video_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    VIDEO_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Resolve NIP-08 `#[N]` tag references in content text.
///
/// Replaces `#[0]`, `#[1]`, etc. with appropriate `nostr:` URIs based on the
/// tag at that index. For `p` tags, generates `nostr:npub1...`. For `e` tags,
/// generates `nostr:note1...`. Unknown tag types are left as-is.
fn resolve_tag_references(text: &str, tags: &[Vec<String>]) -> String {
    TAG_REF_REGEX
        .replace_all(text, |caps: &regex::Captures| {
            let index: usize = caps[1].parse().unwrap_or(usize::MAX);
            let Some(tag) = tags.get(index) else {
                return caps[0].to_string();
            };
            if tag.len() < 2 {
                return caps[0].to_string();
            }

            match tag[0].as_str() {
                "p" => {
                    // Pubkey tag — generate nostr:npub1... URI for the segment parser
                    if let Ok(pk) = nostr::PublicKey::from_hex(&tag[1]) {
                        let npub = pk.to_bech32().unwrap_or_default();
                        return format!("nostr:{npub}");
                    }
                    caps[0].to_string()
                }
                "e" => {
                    // Event tag — generate nostr:note1...
                    if let Ok(id) = nostr::EventId::from_hex(&tag[1]) {
                        let note = id.to_bech32().unwrap_or_default();
                        return format!("nostr:{note}");
                    }
                    caps[0].to_string()
                }
                _ => caps[0].to_string(),
            }
        })
        .into_owned()
}

/// Segments of parsed content.
#[derive(Debug)]
enum ContentSegment {
    /// Plain text (will be auto-escaped by maud).
    Text(String),
    /// A URL to be linked or embedded.
    Url(String),
    /// A `nostr:` URI with the full URI and the bech32 part.
    NostrUri(String, String),
}

/// Parse content text into segments of plain text, URLs, and nostr: URIs.
fn parse_content_segments(text: &str) -> Vec<ContentSegment> {
    let mut segments = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Find the earliest match of either a URL or nostr: URI
        let url_match = URL_REGEX.find(remaining);
        let nostr_match = NOSTR_URI_REGEX.find(remaining);

        let earliest = match (url_match, nostr_match) {
            (Some(u), Some(n)) => {
                if u.start() <= n.start() {
                    Some(('u', u.start(), u.end(), u.as_str().to_string()))
                } else {
                    Some(('n', n.start(), n.end(), n.as_str().to_string()))
                }
            }
            (Some(u), None) => Some(('u', u.start(), u.end(), u.as_str().to_string())),
            (None, Some(n)) => Some(('n', n.start(), n.end(), n.as_str().to_string())),
            (None, None) => None,
        };

        match earliest {
            Some((kind, start, end, matched)) => {
                // Add any text before the match
                if start > 0 {
                    segments.push(ContentSegment::Text(remaining[..start].to_string()));
                }

                match kind {
                    'u' => segments.push(ContentSegment::Url(matched)),
                    'n' => {
                        let bech32 = matched.strip_prefix("nostr:").unwrap_or(&matched);
                        segments.push(ContentSegment::NostrUri(
                            matched.clone(),
                            bech32.to_string(),
                        ));
                    }
                    _ => unreachable!(),
                }

                remaining = &remaining[end..];
            }
            None => {
                // No more matches, add remaining text
                segments.push(ContentSegment::Text(remaining.to_string()));
                break;
            }
        }
    }

    segments
}

/// Render a nostr: reference as a linked element or quote card.
fn render_nostr_reference(
    bech32: &str,
    display_names: &HashMap<String, String>,
    base_url: &str,
    quoted_events: &HashMap<String, QuotedEvent>,
) -> Option<Markup> {
    let nip19 = Nip19::from_bech32(bech32).ok()?;

    match nip19 {
        Nip19::Pubkey(pk) => {
            let hex = pk.to_hex();
            let npub = pk.to_bech32().ok()?;
            let name = display_names
                .get(&hex)
                .map(|s| format!("@{s}"))
                .unwrap_or_else(|| format!("@{}", super::components::truncate_key(&npub)));

            Some(html! {
                a href={(base_url) "/" (npub)} { (name) }
            })
        }
        Nip19::Profile(profile) => {
            let hex = profile.public_key.to_hex();
            let npub = profile.public_key.to_bech32().ok()?;
            let name = display_names
                .get(&hex)
                .map(|s| format!("@{s}"))
                .unwrap_or_else(|| format!("@{}", super::components::truncate_key(&npub)));

            Some(html! {
                a href={(base_url) "/" (npub)} { (name) }
            })
        }
        Nip19::EventId(id) => {
            let hex = id.to_hex();
            // Try to render as a quote card if we have the event
            if let Some(quoted) = quoted_events.get(&hex) {
                return Some(render_quote_card(quoted, base_url));
            }
            let note = id.to_bech32().ok()?;
            Some(html! {
                a href={(base_url) "/" (note)} {
                    (super::components::truncate_key(&note))
                }
            })
        }
        Nip19::Event(event) => {
            let hex = event.event_id.to_hex();
            // Try to render as a quote card if we have the event
            if let Some(quoted) = quoted_events.get(&hex) {
                return Some(render_quote_card(quoted, base_url));
            }
            let nevent = nostr::nips::nip19::Nip19Event::new(event.event_id)
                .to_bech32()
                .ok()?;
            Some(html! {
                a href={(base_url) "/" (nevent)} {
                    (super::components::truncate_key(&nevent))
                }
            })
        }
        Nip19::Coordinate(coord) => {
            let naddr = coord.to_bech32().ok()?;
            Some(html! {
                a href={(base_url) "/" (naddr)} {
                    (super::components::truncate_key(&naddr))
                }
            })
        }
        _ => None,
    }
}

/// Render a quoted event as an inline card.
/// Content is rendered as plain text only (depth 1, no recursive embeds).
fn render_quote_card(quoted: &QuotedEvent, base_url: &str) -> Markup {
    let author_name = quoted
        .author
        .as_ref()
        .map(|a| a.display_name())
        .unwrap_or("Anonymous");
    let author_pic = quoted.author.as_ref().and_then(|a| a.picture.as_deref());
    let initial = author_name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();

    let link_id = nostr::EventId::from_hex(&quoted.id)
        .ok()
        .and_then(|id| id.to_bech32().ok())
        .unwrap_or_else(|| quoted.id.clone());

    // Format timestamp
    let time_str = if quoted.created_at > 0 {
        chrono::DateTime::from_timestamp(i64::from(quoted.created_at), 0)
            .map(|dt| dt.format("%b %d, %Y").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Truncate content for the card (plain text, no embeds)
    let display_content = truncate(&quoted.content, 280);

    html! {
        a class="quote-card" href={(base_url) "/" (link_id)} {
            div class="quote-card-author" {
                div class="quote-card-pic" {
                    (initial.as_str())
                    @if let Some(pic_url) = author_pic {
                        @if is_safe_url(pic_url) {
                            img src=(pic_url) alt="" loading="lazy" onerror="this.style.display='none'";
                        }
                    }
                }
                span class="quote-card-name" { (author_name) }
                @if !time_str.is_empty() {
                    span class="quote-card-time" { (time_str) }
                }
            }
            div class="quote-card-content" { (display_content) }
        }
    }
}

/// Check if a URL points to an image.
fn is_image_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    // Strip query parameters for extension check
    let path = lower.split('?').next().unwrap_or(&lower);
    IMAGE_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Truncate a URL for display, removing protocol and limiting length.
fn truncate_url(url: &str) -> String {
    let display = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    if display.len() > 50 {
        format!("{}...", &display[..47])
    } else {
        display.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_segments_plain_text() {
        let segments = parse_content_segments("Hello, world!");
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "Hello, world!"));
    }

    #[test]
    fn test_parse_segments_with_url() {
        let segments = parse_content_segments("Check out https://example.com for more");
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "Check out "));
        assert!(matches!(&segments[1], ContentSegment::Url(u) if u == "https://example.com"));
        assert!(matches!(&segments[2], ContentSegment::Text(t) if t == " for more"));
    }

    #[test]
    fn test_is_image_url() {
        assert!(is_image_url("https://example.com/photo.jpg"));
        assert!(is_image_url("https://example.com/photo.PNG"));
        assert!(is_image_url("https://example.com/photo.webp?w=800"));
        assert!(!is_image_url("https://example.com/page.html"));
        assert!(!is_image_url("https://example.com/file.pdf"));
    }

    #[test]
    fn test_truncate_url() {
        assert_eq!(truncate_url("https://example.com"), "example.com");
        assert_eq!(
            truncate_url(
                "https://example.com/a-very-long-path-that-exceeds-fifty-characters-in-total"
            ),
            "example.com/a-very-long-path-that-exceeds-fifty..."
        );
    }

    #[test]
    fn test_resolve_tag_references_p_tag() {
        let tags = vec![vec![
            "p".to_string(),
            "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2".to_string(),
        ]];
        let result = resolve_tag_references("hello #[0]!", &tags);
        assert!(result.starts_with("hello nostr:npub1"));
        assert!(result.ends_with("!"));
        assert!(!result.contains("#[0]"));
    }

    #[test]
    fn test_resolve_tag_references_e_tag() {
        let tags = vec![vec![
            "e".to_string(),
            "a84c5de86efc2ec2cff7bad077c4171e09146b633b7ad117fffe088d9579ac33".to_string(),
        ]];
        let result = resolve_tag_references("see #[0]", &tags);
        assert!(result.starts_with("see nostr:note1"));
        assert!(!result.contains("#[0]"));
    }

    #[test]
    fn test_resolve_tag_references_out_of_bounds() {
        let tags = vec![];
        let result = resolve_tag_references("broken #[3]", &tags);
        assert_eq!(result, "broken #[3]");
    }

    #[test]
    fn test_resolve_tag_references_mixed() {
        let tags = vec![
            vec![
                "p".to_string(),
                "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2".to_string(),
            ],
            vec!["t".to_string(), "nostr".to_string()],
            vec![
                "e".to_string(),
                "a84c5de86efc2ec2cff7bad077c4171e09146b633b7ad117fffe088d9579ac33".to_string(),
            ],
        ];
        let result = resolve_tag_references("hello #[0], see #[2]? #[1] stays", &tags);
        assert!(result.contains("nostr:npub1"));
        assert!(result.contains("nostr:note1"));
        // #[1] is a "t" tag, which we don't resolve — it stays as-is
        assert!(result.contains("#[1]"));
    }

    // -- Additional parse_content_segments tests --

    #[test]
    fn test_parse_segments_empty_content() {
        let segments = parse_content_segments("");
        assert!(segments.is_empty());
    }

    #[test]
    fn test_parse_segments_only_newlines() {
        let segments = parse_content_segments("\n\n\n");
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "\n\n\n"));
    }

    #[test]
    fn test_parse_segments_nostr_uri() {
        let npub = "npub1sg6plzptd64u62a878hep2kev88swjh3tw00gjsfl8f237lmu63q0uf63m";
        let input = format!("hey nostr:{npub} check this");
        let segments = parse_content_segments(&input);
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "hey "));
        assert!(
            matches!(&segments[1], ContentSegment::NostrUri(uri, bech) if uri.contains(npub) && bech == npub)
        );
        assert!(matches!(&segments[2], ContentSegment::Text(t) if t == " check this"));
    }

    #[test]
    fn test_parse_segments_nostr_nevent() {
        let nevent = "nevent1qqsqzh75xs5mkljtarlz82jk225vksu4m6wp355taepnwdphlhdfz6gnwh8jr";
        let input = format!("see nostr:{nevent}");
        let segments = parse_content_segments(&input);
        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "see "));
        assert!(matches!(&segments[1], ContentSegment::NostrUri(_, bech) if bech == nevent));
    }

    #[test]
    fn test_parse_segments_mixed_url_and_nostr() {
        let npub = "npub1sg6plzptd64u62a878hep2kev88swjh3tw00gjsfl8f237lmu63q0uf63m";
        let input = format!("visit https://example.com and follow nostr:{npub}");
        let segments = parse_content_segments(&input);
        assert_eq!(segments.len(), 4);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "visit "));
        assert!(matches!(&segments[1], ContentSegment::Url(u) if u == "https://example.com"));
        assert!(matches!(&segments[2], ContentSegment::Text(t) if t == " and follow "));
        assert!(matches!(&segments[3], ContentSegment::NostrUri(_, _)));
    }

    #[test]
    fn test_parse_segments_multiple_urls() {
        let segments = parse_content_segments("see https://a.com and https://b.com for more");
        assert_eq!(segments.len(), 5);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "see "));
        assert!(matches!(&segments[1], ContentSegment::Url(u) if u == "https://a.com"));
        assert!(matches!(&segments[2], ContentSegment::Text(t) if t == " and "));
        assert!(matches!(&segments[3], ContentSegment::Url(u) if u == "https://b.com"));
        assert!(matches!(&segments[4], ContentSegment::Text(t) if t == " for more"));
    }

    #[test]
    fn test_parse_segments_url_at_start() {
        let segments = parse_content_segments("https://example.com is great");
        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], ContentSegment::Url(u) if u == "https://example.com"));
        assert!(matches!(&segments[1], ContentSegment::Text(t) if t == " is great"));
    }

    #[test]
    fn test_parse_segments_url_at_end() {
        let segments = parse_content_segments("check https://example.com");
        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], ContentSegment::Text(t) if t == "check "));
        assert!(matches!(&segments[1], ContentSegment::Url(u) if u == "https://example.com"));
    }

    // -- Additional is_image_url tests --

    #[test]
    fn test_is_image_url_svg() {
        assert!(is_image_url("https://example.com/logo.svg"));
    }

    #[test]
    fn test_is_image_url_gif() {
        assert!(is_image_url("https://example.com/anim.gif"));
    }

    #[test]
    fn test_is_image_url_with_query_params() {
        assert!(is_image_url("https://example.com/photo.jpg?width=800&q=90"));
    }

    #[test]
    fn test_is_image_url_not_image() {
        assert!(!is_image_url("https://example.com/document.pdf"));
        assert!(!is_image_url("https://example.com/video.mp4"));
        assert!(!is_image_url("https://example.com/"));
    }

    #[test]
    fn test_is_image_url_empty() {
        assert!(!is_image_url(""));
    }

    // -- Additional truncate_url tests --

    #[test]
    fn test_truncate_url_http() {
        assert_eq!(truncate_url("http://example.com"), "example.com");
    }

    #[test]
    fn test_truncate_url_no_protocol() {
        assert_eq!(truncate_url("example.com/path"), "example.com/path");
    }

    #[test]
    fn test_truncate_url_exactly_50_display_chars() {
        // After stripping "https://", the display part is exactly 50 chars
        let domain = "a".repeat(50);
        let url = format!("https://{domain}");
        assert_eq!(truncate_url(&url), domain);
    }

    #[test]
    fn test_truncate_url_51_display_chars() {
        let domain = "a".repeat(51);
        let url = format!("https://{domain}");
        let result = truncate_url(&url);
        assert_eq!(result.len(), 50); // 47 + "..."
        assert!(result.ends_with("..."));
    }

    // -- Additional resolve_tag_references tests --

    #[test]
    fn test_resolve_tag_references_empty_text() {
        let result = resolve_tag_references("", &[]);
        assert_eq!(result, "");
    }

    #[test]
    fn test_resolve_tag_references_no_refs() {
        let result = resolve_tag_references("just plain text", &[]);
        assert_eq!(result, "just plain text");
    }

    #[test]
    fn test_resolve_tag_references_short_tag() {
        // Tag with only 1 element (missing value)
        let tags = vec![vec!["p".to_string()]];
        let result = resolve_tag_references("#[0]", &tags);
        assert_eq!(result, "#[0]");
    }

    #[test]
    fn test_resolve_tag_references_invalid_hex() {
        let tags = vec![vec!["p".to_string(), "not_valid_hex".to_string()]];
        let result = resolve_tag_references("#[0]", &tags);
        assert_eq!(result, "#[0]");
    }

    #[test]
    fn test_resolve_tag_references_multiple_same_index() {
        let tags = vec![vec![
            "p".to_string(),
            "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2".to_string(),
        ]];
        let result = resolve_tag_references("#[0] and #[0]", &tags);
        // Both references should be resolved
        assert!(!result.contains("#[0]"));
        assert_eq!(result.matches("nostr:npub1").count(), 2);
    }
}
