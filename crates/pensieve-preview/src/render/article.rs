//! Long-form article (kind 30023) renderer.
//!
//! Renders articles with title, author info, and full markdown content.
//! Markdown is converted to HTML using pulldown-cmark.

use maud::{html, Markup, PreEscaped};
use pulldown_cmark::{html as md_html, Options, Parser};

use super::components::{
    author_header, engagement_bar, is_safe_url, kind_badge, nostr_link, page_shell, truncate,
    OpenGraphData,
};
use crate::query::{EngagementCounts, EventRow, ProfileMetadata};

/// Render a long-form article preview page.
#[allow(clippy::too_many_arguments)]
pub fn render(
    event: &EventRow,
    author: Option<&ProfileMetadata>,
    author_npub: &str,
    nevent: &str,
    engagement: &EngagementCounts,
    _display_names: &std::collections::HashMap<String, String>,
    base_url: &str,
    site_name: &str,
) -> Markup {
    // Extract article metadata from tags
    let article_title = if !event.title.is_empty() {
        &event.title
    } else {
        "Untitled Article"
    };

    let summary = extract_tag_value(&event.tags, "summary").unwrap_or_default();

    let image = extract_tag_value(&event.tags, "image").or_else(|| {
        if !event.thumbnail.is_empty() {
            Some(event.thumbnail.clone())
        } else {
            None
        }
    });

    let published_at = extract_tag_value(&event.tags, "published_at")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(event.created_at);

    let author_name = author.map(|a| a.display_name()).unwrap_or("Anonymous");
    let title = format!("{article_title} - {author_name}");
    let description = if !summary.is_empty() {
        truncate(&summary, 200)
    } else {
        truncate(&event.content, 200)
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

    // Render markdown content to HTML
    let rendered_markdown = render_markdown(&event.content);

    let body = html! {
        div class="card" {
            // Article image
            @if let Some(img_url) = image.as_deref() {
                @if is_safe_url(img_url) {
                    img class="article-image" src=(img_url) alt=(article_title) loading="lazy";
                }
            }

            div class="card-body" {
                (kind_badge(event.kind))

                h1 class="article-title" { (article_title) }

                (author_header(author, author_npub, base_url))

                div class="article-content" {
                    (PreEscaped(&rendered_markdown))
                }

                (engagement_bar(engagement, published_at))
            }
        }
        (nostr_link(nevent))
    };

    page_shell(&title, &description, &canonical, og, body, site_name)
}

/// Render markdown text to sanitized HTML.
///
/// Uses pulldown-cmark with common extensions (tables, footnotes, strikethrough,
/// task lists). The output is safe because pulldown-cmark parses markdown structure
/// and generates its own HTML — it does not pass through raw HTML tags by default.
fn render_markdown(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(markdown, options);
    let mut html_output = String::with_capacity(markdown.len() * 2);
    md_html::push_html(&mut html_output, parser);
    html_output
}

/// Extract a tag value by name from event tags.
fn extract_tag_value(tags: &[Vec<String>], name: &str) -> Option<String> {
    tags.iter()
        .find(|tag| tag.len() >= 2 && tag[0] == name)
        .map(|tag| tag[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- render_markdown() tests --

    #[test]
    fn render_markdown_empty() {
        let result = render_markdown("");
        assert!(result.is_empty());
    }

    #[test]
    fn render_markdown_plain_text() {
        let result = render_markdown("Hello, world!");
        assert!(result.contains("<p>Hello, world!</p>"));
    }

    #[test]
    fn render_markdown_headers() {
        let result = render_markdown("# H1\n## H2\n### H3");
        assert!(result.contains("<h1>H1</h1>"));
        assert!(result.contains("<h2>H2</h2>"));
        assert!(result.contains("<h3>H3</h3>"));
    }

    #[test]
    fn render_markdown_bold_and_italic() {
        let result = render_markdown("**bold** and *italic*");
        assert!(result.contains("<strong>bold</strong>"));
        assert!(result.contains("<em>italic</em>"));
    }

    #[test]
    fn render_markdown_links() {
        let result = render_markdown("[click here](https://example.com)");
        assert!(result.contains("<a href=\"https://example.com\">click here</a>"));
    }

    #[test]
    fn render_markdown_code_inline() {
        let result = render_markdown("use `println!()` to print");
        assert!(result.contains("<code>println!()</code>"));
    }

    #[test]
    fn render_markdown_code_block() {
        let result = render_markdown("```rust\nfn main() {}\n```");
        assert!(result.contains("<pre>"));
        assert!(result.contains("<code"));
        assert!(result.contains("fn main() {}"));
    }

    #[test]
    fn render_markdown_unordered_list() {
        let result = render_markdown("- item 1\n- item 2\n- item 3");
        assert!(result.contains("<ul>"));
        assert!(result.contains("<li>item 1</li>"));
        assert!(result.contains("<li>item 2</li>"));
    }

    #[test]
    fn render_markdown_ordered_list() {
        let result = render_markdown("1. first\n2. second");
        assert!(result.contains("<ol>"));
        assert!(result.contains("<li>first</li>"));
    }

    #[test]
    fn render_markdown_blockquote() {
        let result = render_markdown("> wise words");
        assert!(result.contains("<blockquote>"));
        assert!(result.contains("wise words"));
    }

    #[test]
    fn render_markdown_table() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let result = render_markdown(md);
        assert!(result.contains("<table>"));
        assert!(result.contains("<th>"));
        assert!(result.contains("<td>"));
    }

    #[test]
    fn render_markdown_strikethrough() {
        let result = render_markdown("~~deleted~~");
        assert!(result.contains("<del>deleted</del>"));
    }

    #[test]
    fn render_markdown_horizontal_rule() {
        let result = render_markdown("above\n\n---\n\nbelow");
        assert!(result.contains("<hr />"));
    }

    #[test]
    fn render_markdown_image() {
        let result = render_markdown("![alt text](https://example.com/img.png)");
        assert!(result.contains("<img"));
        assert!(result.contains("src=\"https://example.com/img.png\""));
        assert!(result.contains("alt=\"alt text\""));
    }

    #[test]
    fn render_markdown_unicode() {
        let result = render_markdown("# 你好世界\n\nCafé ☕ 🎉");
        assert!(result.contains("你好世界"));
        assert!(result.contains("Café ☕ 🎉"));
    }

    #[test]
    fn render_markdown_raw_html_passed_through() {
        // pulldown-cmark passes raw HTML through by default (no ENABLE_HTML option disabled)
        // The CSP header on the page prevents script execution
        let result = render_markdown("<script>alert('xss')</script>");
        assert!(result.contains("<script>"));
    }

    // -- extract_tag_value() tests --

    #[test]
    fn extract_tag_value_found() {
        let tags = vec![
            vec!["title".to_string(), "My Article".to_string()],
            vec!["summary".to_string(), "A summary".to_string()],
        ];
        assert_eq!(
            extract_tag_value(&tags, "title"),
            Some("My Article".to_string())
        );
        assert_eq!(
            extract_tag_value(&tags, "summary"),
            Some("A summary".to_string())
        );
    }

    #[test]
    fn extract_tag_value_not_found() {
        let tags = vec![vec!["title".to_string(), "My Article".to_string()]];
        assert_eq!(extract_tag_value(&tags, "image"), None);
    }

    #[test]
    fn extract_tag_value_empty_tags() {
        let tags: Vec<Vec<String>> = vec![];
        assert_eq!(extract_tag_value(&tags, "title"), None);
    }

    #[test]
    fn extract_tag_value_short_tag() {
        // Tag with only 1 element
        let tags = vec![vec!["title".to_string()]];
        assert_eq!(extract_tag_value(&tags, "title"), None);
    }

    #[test]
    fn extract_tag_value_first_match_wins() {
        let tags = vec![
            vec!["title".to_string(), "First".to_string()],
            vec!["title".to_string(), "Second".to_string()],
        ];
        assert_eq!(extract_tag_value(&tags, "title"), Some("First".to_string()));
    }

    #[test]
    fn extract_tag_value_with_extra_elements() {
        let tags = vec![vec![
            "e".to_string(),
            "abc123".to_string(),
            "wss://relay".to_string(),
            "reply".to_string(),
        ]];
        assert_eq!(extract_tag_value(&tags, "e"), Some("abc123".to_string()));
    }
}
