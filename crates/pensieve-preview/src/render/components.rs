//! Shared HTML components used across all preview pages.
//!
//! These are maud functions that return `Markup` fragments for composition
//! into full pages.

use maud::{Markup, PreEscaped, html};

use crate::query::{EngagementCounts, ProfileMetadata};

/// Inline CSS for all preview pages.
///
/// Flat, modern design. No borders/shadows — uses spacing and subtle
/// background shifts for hierarchy. Phosphor icons via inline SVG.
pub const PAGE_CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
:root{--bg:#fafafa;--fg:#111;--fg2:#555;--fg3:#999;--accent:#9900CC;--accent-hover:#7a00a3;--surface:#fff;--border:rgba(153,0,204,.15);--mono:"SF Mono",SFMono-Regular,ui-monospace,Menlo,monospace}
body{font-family:Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;line-height:1.6;color:var(--fg);background:var(--bg);min-height:100vh;display:flex;flex-direction:column;align-items:center;padding:1.5rem 1rem}
main{max-width:680px;width:100%;flex:1}
a{color:var(--accent);text-decoration:none}
a:hover{text-decoration:underline}
img{max-width:100%;height:auto}
svg.icon{width:20px;height:20px;fill:currentColor;stroke:none;vertical-align:-3px;flex-shrink:0}

.card{padding:1.5rem;border:1px solid var(--border);border-radius:10px}
.card-body{padding:0}

.author{display:flex;align-items:center;gap:.85rem;margin-bottom:1.25rem;min-height:64px}
.author-pic{width:64px;height:64px;border-radius:50%;background:var(--accent);flex-shrink:0;display:flex;align-items:center;justify-content:center;color:#fff;font-weight:700;font-size:1.4rem;text-transform:uppercase;overflow:hidden;position:relative}
.author-pic img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}
.author-info{min-width:0;flex:1}
.author-name{font-weight:600;font-size:1.1rem;color:var(--fg)}
.author-name:hover{text-decoration:none;color:var(--accent)}
.author-nip05{font-family:var(--mono);color:var(--fg3);font-size:.8rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.author-npub{display:flex;align-items:center;gap:.3rem;margin-top:1px}
.author-npub-text{font-family:var(--mono);font-size:.75rem;color:var(--fg3);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.copy-btn{background:none;border:none;cursor:pointer;color:var(--fg2);padding:2px;border-radius:4px;flex-shrink:0;display:flex;align-items:center}
.copy-btn:hover{color:var(--accent)}
.copy-btn svg.icon{width:14px;height:14px}

.profile-banner-fallback{width:100%;height:180px;border-radius:8px;margin-bottom:-2rem;background:linear-gradient(135deg,#BF00FF,#7c3aed,#4f46e5);position:relative;overflow:hidden}
.profile-banner{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}
.profile-header{display:flex;align-items:flex-end;gap:1rem;padding:0;position:relative;z-index:1}
.profile-pic{width:96px;height:96px;border-radius:50%;border:3px solid var(--bg);background:var(--accent);flex-shrink:0;display:flex;align-items:center;justify-content:center;color:#fff;font-weight:700;font-size:2.2rem;text-transform:uppercase;overflow:hidden;position:relative}
.profile-pic img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}
.profile-body{padding:.75rem 0 0}
.profile-name{font-size:1.75rem;font-weight:700;letter-spacing:-.02em}
.profile-nip05{color:var(--fg3);font-size:.95rem}
.profile-about{margin:.75rem 0;white-space:pre-wrap;word-break:break-word;color:var(--fg2);line-height:1.65;font-size:1.05rem}
.profile-meta{display:flex;gap:1.25rem;flex-wrap:wrap;margin-top:.75rem;font-size:.9rem;color:var(--fg3)}
.profile-meta a{color:var(--accent)}
.profile-meta svg.icon{width:16px;height:16px;vertical-align:-3px;margin-right:2px}

.content{white-space:pre-wrap;word-break:break-word;font-size:1.05rem;line-height:1.75;color:var(--fg)}

.media-grid{display:grid;gap:4px;margin:.75rem 0;border-radius:8px;overflow:hidden}
.media-grid.g1{grid-template-columns:1fr}
.media-grid.g2{grid-template-columns:1fr 1fr}
.media-grid.g3{grid-template-columns:1fr 1fr;grid-template-rows:1fr 1fr}
.media-grid.g3 .media-item:first-child{grid-row:1/3}
.media-grid.g4{grid-template-columns:1fr 1fr;grid-template-rows:1fr 1fr}
.media-item{position:relative;overflow:hidden;cursor:pointer;aspect-ratio:1;background:var(--border)}
.media-grid.g1 .media-item{aspect-ratio:16/9}
.media-grid.g2 .media-item{aspect-ratio:1}
.media-item img{width:100%;height:100%;object-fit:cover;display:block;transition:transform .2s}
.media-item:hover img{transform:scale(1.03)}
.media-more{position:absolute;inset:0;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;color:#fff;font-size:1.5rem;font-weight:700}

.video-inline{margin:.75rem 0;border-radius:8px;overflow:hidden;background:#000}
.video-inline video{width:100%;display:block;border-radius:8px}

.lightbox-overlay{display:none;position:fixed;inset:0;background:rgba(0,0,0,.9);z-index:1000;align-items:center;justify-content:center;cursor:pointer;padding:1rem}
.lightbox-overlay.active{display:flex}
.lightbox-overlay img{max-width:95vw;max-height:95vh;object-fit:contain;border-radius:4px}

.reply-context{font-size:.9rem;color:var(--fg3);margin-bottom:.75rem}
.reply-context a{color:var(--accent)}

.quote-card{border:1px solid var(--border);border-radius:8px;padding:.85rem 1rem;margin:.75rem 0;text-decoration:none;display:block;color:var(--fg);transition:border-color .15s}
.quote-card:hover{border-color:var(--accent);text-decoration:none}
.quote-card-author{display:flex;align-items:center;gap:.5rem;margin-bottom:.4rem}
.quote-card-pic{width:24px;height:24px;border-radius:50%;background:var(--accent);flex-shrink:0;display:flex;align-items:center;justify-content:center;color:#fff;font-weight:600;font-size:.6rem;text-transform:uppercase;overflow:hidden;position:relative}
.quote-card-pic img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}
.quote-card-name{font-weight:600;font-size:.85rem;color:var(--fg)}
.quote-card-time{font-size:.75rem;color:var(--fg3);margin-left:auto}
.quote-card-content{font-size:.9rem;line-height:1.6;color:var(--fg2);white-space:pre-wrap;word-break:break-word;display:-webkit-box;-webkit-line-clamp:4;-webkit-box-orient:vertical;overflow:hidden}

.engagement{display:flex;align-items:center;justify-content:space-between;gap:1rem;margin-top:1.25rem;padding-top:1rem;border-top:1px solid var(--border);font-size:.9rem;color:var(--fg3)}
.engagement-left{display:flex;gap:1.5rem;align-items:center}
.engagement-left span{display:flex;align-items:center;gap:.4rem}
.engagement svg.icon{width:18px;height:18px}
.engagement-time{font-size:.8rem;color:var(--fg3);white-space:nowrap}

.kind-badge{display:inline-block;background:var(--bg);color:var(--fg3);font-size:.78rem;padding:.2rem .6rem;border-radius:100px;font-weight:500;letter-spacing:.02em;text-transform:uppercase;margin-bottom:.75rem;border:1px solid var(--border)}

.article-title{font-size:1.5rem;font-weight:700;margin-bottom:.5rem;line-height:1.3;letter-spacing:-.01em}
.article-summary{color:var(--fg2);margin:.75rem 0;line-height:1.65;font-size:1.05rem}
.article-image{width:100%;max-height:280px;object-fit:cover;border-radius:8px;margin-bottom:1rem}
.article-content{font-size:1.05rem;line-height:1.75;color:var(--fg);margin:1rem 0}
.article-content h1,.article-content h2,.article-content h3,.article-content h4{font-weight:700;margin:1.5rem 0 .75rem;color:var(--fg);letter-spacing:-.01em}
.article-content h1{font-size:1.4rem}
.article-content h2{font-size:1.25rem}
.article-content h3{font-size:1.1rem}
.article-content h4{font-size:1rem}
.article-content p{margin:.75rem 0}
.article-content ul,.article-content ol{margin:.75rem 0;padding-left:1.5rem}
.article-content li{margin:.3rem 0}
.article-content blockquote{border-left:3px solid var(--border);padding:.5rem 0 .5rem 1rem;margin:.75rem 0;color:var(--fg2)}
.article-content pre{background:var(--bg);border:1px solid var(--border);border-radius:6px;padding:.75rem 1rem;overflow-x:auto;margin:.75rem 0;font-size:.85rem;line-height:1.5}
.article-content code{font-family:var(--mono);font-size:.88em;background:var(--bg);padding:.15rem .35rem;border-radius:3px;border:1px solid var(--border)}
.article-content pre code{background:none;border:none;padding:0;font-size:inherit}
.article-content a{color:var(--accent)}
.article-content img{max-width:100%;border-radius:6px;margin:.75rem 0}
.article-content hr{border:none;border-top:1px solid var(--border);margin:1.5rem 0}
.article-content table{border-collapse:collapse;width:100%;margin:.75rem 0;font-size:.9rem}
.article-content th,.article-content td{border:1px solid var(--border);padding:.4rem .75rem;text-align:left}
.article-content th{background:var(--bg);font-weight:600}

.video-thumb{position:relative;width:100%;aspect-ratio:16/9;overflow:hidden;background:#000;border-radius:8px;margin-bottom:1rem}
.video-thumb img{width:100%;height:100%;object-fit:cover}
.video-play{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.25)}
.video-play::after{content:"";width:0;height:0;border-style:solid;border-width:18px 0 18px 32px;border-color:transparent transparent transparent #fff;filter:drop-shadow(0 1px 3px rgba(0,0,0,.3))}

.repost-header{font-size:.9rem;color:var(--fg3);padding:0 0 .75rem;display:flex;align-items:center;gap:.35rem}
.repost-header svg.icon{width:16px;height:16px}

.actions{margin-top:1.25rem;display:flex;justify-content:center}
.nostr-link{display:inline-flex;align-items:center;gap:.5rem;padding:.55rem 1.1rem;background:var(--accent);color:#fff;border-radius:6px;font-size:.9rem;font-weight:500;text-decoration:none;transition:background .15s}
.nostr-link:hover{background:var(--accent-hover);text-decoration:none}
.nostr-link svg.icon{fill:#fff;width:16px;height:16px}

.footer{text-align:center;margin-top:1rem;padding-top:.75rem;font-size:.8rem;color:var(--fg3);letter-spacing:.01em;width:100%;max-width:680px;display:flex;align-items:center;justify-content:center;gap:.25rem}
.footer a{color:var(--accent);text-decoration:none}
.footer a:hover{text-decoration:underline}

@media(prefers-color-scheme:dark){
:root{--bg:#0a0a0f;--fg:#e5e5e5;--fg2:#a0a0a0;--fg3:#666;--accent:#d946ef;--accent-hover:#e879f9;--surface:#111118;--border:rgba(191,0,255,.2)}
.profile-pic{border-color:var(--bg)}
.copy-btn:hover{background:#1a1a2e}
}
"#;

/// Inline CSS for error pages.
pub const ERROR_CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;display:flex;justify-content:center;align-items:center;min-height:100vh;background:#fafafa;color:#1a1a2e;padding:1rem}
.error-page{text-align:center;max-width:400px}
.error-page h1{font-size:1.5rem;margin-bottom:.75rem}
.error-page p{color:#666;margin-bottom:1rem;line-height:1.5}
.error-page a{color:#6c5ce7}
@media(prefers-color-scheme:dark){
body{background:#0f0f17;color:#e0e0e8}
.error-page p{color:#aaa}
.error-page a{color:#a29bfe}
}
"#;

/// Content-Security-Policy header value.
///
/// Allows inline styles and a small inline script for copy-to-clipboard.
/// No external scripts, no iframes, only HTTPS images.
pub const CSP_HEADER: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src https: data:; connect-src 'self'; form-action 'none'; frame-ancestors 'none'";

/// Render the full HTML page shell with `<head>`, OG tags, and body content.
pub fn page_shell(
    title: &str,
    description: &str,
    canonical_url: &str,
    og: OpenGraphData<'_>,
    body_content: Markup,
    site_name: &str,
) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                meta name="description" content=(description);
                link rel="canonical" href=(canonical_url);

                // Open Graph
                meta property="og:title" content=(og.title);
                meta property="og:description" content=(og.description);
                meta property="og:url" content=(canonical_url);
                meta property="og:site_name" content=(site_name);
                meta property="og:type" content=(og.og_type);
                @if let Some(image) = og.image {
                    meta property="og:image" content=(image);
                }

                // Twitter Card
                meta name="twitter:card" content=(og.twitter_card_type);
                meta name="twitter:title" content=(og.title);
                meta name="twitter:description" content=(og.description);
                @if let Some(image) = og.image {
                    meta name="twitter:image" content=(image);
                }

                // Prevent search indexing of individual events (optional, can remove later)
                // We DO want crawlers to fetch for link previews, but may not want
                // these pages in search results initially.

                style { (PreEscaped(PAGE_CSS)) }
            }
            body {
                main { (body_content) }
                footer class="footer" {
                    "Powered by "
                    a href="https://github.com/andotherstuff/pensieve" { "Pensieve" }
                    ", "
                    a href="https://andotherstuff.org" { "And Other Stuff" }
                }
            }
        }
    }
}

/// Open Graph metadata for a page.
pub struct OpenGraphData<'a> {
    /// OG title.
    pub title: &'a str,
    /// OG description.
    pub description: &'a str,
    /// OG type (e.g., "profile", "article", "website").
    pub og_type: &'a str,
    /// OG image URL (must be HTTPS).
    pub image: Option<&'a str>,
    /// Twitter card type ("summary", "summary_large_image").
    pub twitter_card_type: &'a str,
}

/// Render an author header with picture, name, NIP-05, and truncated npub with copy.
pub fn author_header(metadata: Option<&ProfileMetadata>, npub: &str, base_url: &str) -> Markup {
    let name = metadata.map(|m| m.display_name()).unwrap_or("Anonymous");
    let nip05 = metadata.and_then(|m| m.nip05.as_deref());
    let picture = metadata.and_then(|m| m.picture.as_deref());
    let truncated_npub = truncate_npub_display(npub);

    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();

    html! {
        div class="author" {
            div class="author-pic" {
                (initial.as_str())
                @if let Some(pic_url) = picture {
                    @if is_safe_url(pic_url) {
                        img src=(pic_url) alt=(name) loading="lazy" onerror="this.style.display='none'";
                    }
                }
            }
            div class="author-info" {
                a href={(base_url) "/" (npub)} class="author-name" {
                    (name)
                }
                @if let Some(nip05_val) = nip05 {
                    div class="author-nip05" { (nip05_val) }
                }
                div class="author-npub" {
                    span class="author-npub-text" title=(npub) { (truncated_npub) }
                    button class="copy-btn" onclick=(format!("navigator.clipboard.writeText('{}').then(()=>{{this.innerHTML='Copied!';setTimeout(()=>this.innerHTML='{}',1500)}})", npub, ICON_COPY.replace('"', "&quot;"))) title="Copy npub" {
                        (PreEscaped(ICON_COPY))
                    }
                }
            }
        }
    }
}

// -- Phosphor icon SVGs (fill variants) --

/// Heart icon (Phosphor fill)
const ICON_HEART: &str = r#"<svg class="icon" viewBox="0 0 256 256"><path d="M240,94c0,70-103.79,126.66-108.21,129a8,8,0,0,1-7.58,0C119.79,220.66,16,164,16,94A62.07,62.07,0,0,1,78,32c20.65,0,38.73,8.88,50,23.89C139.27,40.88,157.35,32,178,32A62.07,62.07,0,0,1,240,94Z"/></svg>"#;

/// Chat bubble icon (Phosphor chat-centered, fill)
const ICON_CHAT: &str = r#"<svg class="icon" viewBox="0 0 256 256"><path d="M232,56V184a16,16,0,0,1-16,16H155.57l-13.68,23.94a16,16,0,0,1-27.78,0L100.43,200H40a16,16,0,0,1-16-16V56A16,16,0,0,1,40,40H216A16,16,0,0,1,232,56Z"/></svg>"#;

/// Repost icon (Phosphor repeat, fill)
const ICON_REPOST: &str = r#"<svg class="icon" viewBox="0 0 256 256"><path d="M213.66,66.34l-32-32a8,8,0,0,0-11.32,11.32L188.69,64H48A16,16,0,0,0,32,80v40a8,8,0,0,0,16,0V80H188.69l-18.35,18.34a8,8,0,0,0,11.32,11.32l32-32A8,8,0,0,0,213.66,66.34Zm-40,120H67.31l18.35-18.34a8,8,0,0,0-11.32-11.32l-32,32a8,8,0,0,0,0,11.32l32,32a8,8,0,0,0,11.32-11.32L67.31,192H208a16,16,0,0,0,16-16V136a8,8,0,0,0-16,0v40Z"/></svg>"#;

/// Copy icon (Phosphor copy, fill)
pub const ICON_COPY: &str = r#"<svg class="icon" viewBox="0 0 256 256"><path d="M216,32H88a8,8,0,0,0-8,8V80H40a8,8,0,0,0-8,8V216a8,8,0,0,0,8,8H168a8,8,0,0,0,8-8V176h40a8,8,0,0,0,8-8V40A8,8,0,0,0,216,32Zm-56,176H48V96H160Zm48-48H176V88a8,8,0,0,0-8-8H96V48H208Z"/></svg>"#;

/// Arrow square out icon (Phosphor arrow-square-out, fill)
const ICON_EXTERNAL: &str = r#"<svg class="icon" viewBox="0 0 256 256"><path d="M228,104a12,12,0,0,1-24,0V69l-59.51,59.51a12,12,0,0,1-17-17L187,52H152a12,12,0,0,1,0-24h64a12,12,0,0,1,12,12Zm-44,44a12,12,0,0,0-12,12v52H52V92h52a12,12,0,0,0,0-24H48A20,20,0,0,0,28,88V216a20,20,0,0,0,20,20H176a20,20,0,0,0,20-20V160A12,12,0,0,0,184,148Z"/></svg>"#;

/// Lightning bolt icon (Phosphor lightning, fill)
pub const ICON_LIGHTNING: &str = r#"<svg class="icon" viewBox="0 0 256 256"><path d="M213.85,125.46l-112,120a8,8,0,0,1-13.69-7l14.66-73.33L57.45,143.37a8,8,0,0,1-5.3-11.83l112-120a8,8,0,0,1,13.69,7L163.18,91.87l45.37,21.76A8,8,0,0,1,213.85,125.46Z"/></svg>"#;

/// Render engagement counts + timestamp on one line.
///
/// Engagement icons on the left, timestamp on the right.
pub fn engagement_bar(counts: &EngagementCounts, created_at: u32) -> Markup {
    let timestamp_str = format_timestamp(created_at);

    html! {
        div class="engagement" {
            div class="engagement-left" {
                @if counts.reactions > 0 {
                    span title="Reactions" {
                        (PreEscaped(ICON_HEART)) " " (format_count(counts.reactions))
                    }
                }
                @if counts.comments > 0 {
                    span title="Replies" {
                        (PreEscaped(ICON_CHAT)) " " (format_count(counts.comments))
                    }
                }
                span title="Reposts" {
                    (PreEscaped(ICON_REPOST)) " " (format_count(counts.reposts))
                }
                span title=(format!("{} zaps", counts.zap_count)) {
                    (PreEscaped(ICON_LIGHTNING)) " " (format_sats(counts.zap_msats))
                }
            }
            @if let Some((display, iso)) = timestamp_str {
                time class="engagement-time" datetime=(iso) {
                    (display)
                }
            }
        }
    }
}

/// Format a timestamp as "Mon DD, YYYY HH:MM UTC".
/// Returns None if created_at is 0 (missing).
fn format_timestamp(created_at: u32) -> Option<(String, String)> {
    if created_at == 0 {
        return None;
    }

    let ts = chrono::DateTime::from_timestamp(i64::from(created_at), 0)?;
    let display = ts.format("%b %d, %Y %H:%M UTC").to_string();
    let iso = ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    Some((display, iso))
}

/// Format millisatoshis as a human-readable sats string.
fn format_sats(msats: u64) -> String {
    let sats = msats / 1000;
    if sats >= 1_000_000 {
        format!("{:.1}M sats", sats as f64 / 1_000_000.0)
    } else if sats >= 1_000 {
        format!("{:.1}K sats", sats as f64 / 1_000.0)
    } else {
        format!("{sats} sats")
    }
}

/// Render a "Open in Nostr" button on its own line.
pub fn nostr_link(nostr_uri: &str) -> Markup {
    html! {
        div class="actions" {
            a class="nostr-link" href=(format!("nostr:{nostr_uri}")) {
                (PreEscaped(ICON_EXTERNAL)) " Open in Nostr"
            }
        }
    }
}

/// Render a kind badge (e.g., "Kind 30023 - Long-form").
pub fn kind_badge(kind: u16) -> Markup {
    let label = match kind {
        0 => "Profile",
        1 => "Note",
        3 => "Contacts",
        6 => "Repost",
        7 => "Reaction",
        16 => "Generic Repost",
        1063 => "File Metadata",
        9735 => "Zap Receipt",
        30023 => "Long-form Article",
        30024 => "Draft Article",
        34235 => "Video",
        34236 => "Short Video",
        _ => "",
    };

    html! {
        @if label.is_empty() {
            span class="kind-badge" { "Kind " (kind) }
        } @else {
            span class="kind-badge" { (label) }
        }
    }
}

/// Check if a URL is safe to use in `src` or `href` attributes.
pub fn is_safe_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Format a large number with K/M suffixes for display.
fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Truncate a string to a maximum length, appending "..." if truncated.
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Truncate an npub/hex key for display (first 8 + last 4 chars).
pub fn truncate_key(key: &str) -> String {
    if key.len() <= 16 {
        return key.to_string();
    }
    format!("{}...{}", &key[..12], &key[key.len() - 4..])
}

/// Truncate an npub for display: first 16 chars + ... + last 12 chars.
pub fn truncate_npub_display(npub: &str) -> String {
    if npub.len() <= 32 {
        return npub.to_string();
    }
    format!("{}...{}", &npub[..16], &npub[npub.len() - 12..])
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- truncate() tests --

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_shorter_than_max() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_longer_than_max() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_ascii_boundary() {
        assert_eq!(truncate("abcdefghij", 3), "abc...");
    }

    #[test]
    fn truncate_unicode_multibyte() {
        // "café" — 'é' is 2 bytes in UTF-8
        let result = truncate("café", 4);
        // byte 4 lands inside 'é', so it backs up to byte 3
        assert_eq!(result, "caf...");
    }

    #[test]
    fn truncate_unicode_emoji() {
        // '🎉' is 4 bytes
        let result = truncate("🎉hello", 2);
        // byte 2 is inside the emoji, backs up to 0
        assert_eq!(result, "...");
    }

    #[test]
    fn truncate_zero_max() {
        assert_eq!(truncate("hello", 0), "...");
    }

    #[test]
    fn truncate_unicode_cjk() {
        // CJK characters are 3 bytes each
        let result = truncate("你好世界", 6);
        assert_eq!(result, "你好...");
    }

    #[test]
    fn truncate_very_long_string() {
        let long = "a".repeat(10_000);
        let result = truncate(&long, 100);
        assert_eq!(result.len(), 103); // 100 + "..."
        assert!(result.ends_with("..."));
    }

    // -- truncate_key() tests --

    #[test]
    fn truncate_key_short_key() {
        assert_eq!(truncate_key("abc"), "abc");
    }

    #[test]
    fn truncate_key_exactly_16_chars() {
        assert_eq!(truncate_key("abcdefghijklmnop"), "abcdefghijklmnop");
    }

    #[test]
    fn truncate_key_17_chars() {
        assert_eq!(truncate_key("abcdefghijklmnopq"), "abcdefghijkl...nopq");
    }

    #[test]
    fn truncate_key_npub_length() {
        // Typical npub is 63 chars
        let npub = "npub1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqx9hd7";
        let result = truncate_key(npub);
        assert_eq!(result.len(), 12 + 3 + 4); // first 12 + "..." + last 4
        assert!(result.starts_with("npub1qqqqqqq"));
        assert!(result.ends_with("hd7"));
    }

    #[test]
    fn truncate_key_empty() {
        assert_eq!(truncate_key(""), "");
    }

    // -- truncate_npub_display() tests --

    #[test]
    fn truncate_npub_short_string() {
        assert_eq!(truncate_npub_display("short"), "short");
    }

    #[test]
    fn truncate_npub_exactly_32_chars() {
        let s = "a".repeat(32);
        assert_eq!(truncate_npub_display(&s), s);
    }

    #[test]
    fn truncate_npub_33_chars() {
        let s = "a".repeat(33);
        let result = truncate_npub_display(&s);
        // first 16 + "..." + last 12
        assert_eq!(result, format!("{}...{}", "a".repeat(16), "a".repeat(12)));
    }

    #[test]
    fn truncate_npub_real_npub() {
        let npub = "npub1sg6plzptd64u62a878hep2kev88swjh3tw00gjsfl8f237lmu63q0uf63m";
        let result = truncate_npub_display(npub);
        assert!(result.starts_with("npub1sg6plzptd64"));
        assert!(result.ends_with("mu63q0uf63m"));
        assert!(result.contains("..."));
    }

    // -- is_safe_url() tests --

    #[test]
    fn is_safe_url_https() {
        assert!(is_safe_url("https://example.com"));
    }

    #[test]
    fn is_safe_url_http() {
        assert!(is_safe_url("http://example.com"));
    }

    #[test]
    fn is_safe_url_javascript() {
        assert!(!is_safe_url("javascript:alert(1)"));
    }

    #[test]
    fn is_safe_url_data_uri() {
        assert!(!is_safe_url("data:text/html,<script>alert(1)</script>"));
    }

    #[test]
    fn is_safe_url_empty() {
        assert!(!is_safe_url(""));
    }

    #[test]
    fn is_safe_url_ftp() {
        assert!(!is_safe_url("ftp://example.com/file"));
    }

    #[test]
    fn is_safe_url_relative() {
        assert!(!is_safe_url("/path/to/resource"));
    }

    #[test]
    fn is_safe_url_just_protocol() {
        assert!(is_safe_url("https://"));
        assert!(is_safe_url("http://"));
    }

    #[test]
    fn is_safe_url_case_sensitive() {
        // Scheme matching is case-sensitive
        assert!(!is_safe_url("HTTPS://example.com"));
    }

    // -- format_count() tests (private, tested within module) --

    #[test]
    fn format_count_zero() {
        assert_eq!(format_count(0), "0");
    }

    #[test]
    fn format_count_small() {
        assert_eq!(format_count(42), "42");
        assert_eq!(format_count(999), "999");
    }

    #[test]
    fn format_count_thousands() {
        assert_eq!(format_count(1_000), "1.0K");
        assert_eq!(format_count(1_500), "1.5K");
        assert_eq!(format_count(999_999), "1000.0K");
    }

    #[test]
    fn format_count_millions() {
        assert_eq!(format_count(1_000_000), "1.0M");
        assert_eq!(format_count(2_500_000), "2.5M");
        assert_eq!(format_count(42_000_000), "42.0M");
    }

    // -- format_sats() tests (private, tested within module) --

    #[test]
    fn format_sats_zero() {
        assert_eq!(format_sats(0), "0 sats");
    }

    #[test]
    fn format_sats_sub_sat() {
        // 999 msats = 0 sats (integer division)
        assert_eq!(format_sats(999), "0 sats");
    }

    #[test]
    fn format_sats_one_sat() {
        assert_eq!(format_sats(1_000), "1 sats");
    }

    #[test]
    fn format_sats_hundreds() {
        assert_eq!(format_sats(500_000), "500 sats");
    }

    #[test]
    fn format_sats_thousands() {
        assert_eq!(format_sats(1_000_000), "1.0K sats");
        assert_eq!(format_sats(21_500_000), "21.5K sats");
    }

    #[test]
    fn format_sats_millions() {
        assert_eq!(format_sats(1_000_000_000), "1.0M sats");
        assert_eq!(format_sats(2_100_000_000), "2.1M sats");
    }

    // -- format_timestamp() tests (private, tested within module) --

    #[test]
    fn format_timestamp_zero() {
        assert!(format_timestamp(0).is_none());
    }

    #[test]
    fn format_timestamp_valid() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let result = format_timestamp(1_704_067_200);
        assert!(result.is_some());
        let (display, iso) = result.unwrap();
        assert_eq!(display, "Jan 01, 2024 00:00 UTC");
        assert_eq!(iso, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn format_timestamp_epoch() {
        // Unix epoch = 1 (not 0, which returns None)
        let result = format_timestamp(1);
        assert!(result.is_some());
        let (display, iso) = result.unwrap();
        assert!(display.contains("1970"));
        assert!(iso.starts_with("1970-"));
    }

    #[test]
    fn format_timestamp_future() {
        // A far-future timestamp (year 2100ish)
        let result = format_timestamp(4_102_444_800);
        assert!(result.is_some());
        let (display, _iso) = result.unwrap();
        assert!(display.contains("2099") || display.contains("2100"));
    }

    // -- kind_badge() tests --

    #[test]
    fn kind_badge_known_kinds() {
        let markup = kind_badge(1);
        let html = markup.into_string();
        assert!(html.contains("Note"));

        let markup = kind_badge(30023);
        let html = markup.into_string();
        assert!(html.contains("Long-form Article"));

        let markup = kind_badge(34235);
        let html = markup.into_string();
        assert!(html.contains("Video"));
    }

    #[test]
    fn kind_badge_unknown_kind() {
        let markup = kind_badge(9999);
        let html = markup.into_string();
        assert!(html.contains("Kind 9999"));
    }

    // -- nostr_link() tests --

    #[test]
    fn nostr_link_renders_href() {
        let markup = nostr_link("nevent1abc123");
        let html = markup.into_string();
        assert!(html.contains("nostr:nevent1abc123"));
        assert!(html.contains("Open in Nostr"));
    }

    // -- engagement_bar() tests --

    #[test]
    fn engagement_bar_all_zero() {
        let counts = EngagementCounts::default();
        let markup = engagement_bar(&counts, 0);
        let html = markup.into_string();
        // Reposts and zaps always show (even as 0)
        assert!(html.contains("Reposts"));
        assert!(html.contains("0 sats"));
    }

    #[test]
    fn engagement_bar_with_reactions() {
        let counts = EngagementCounts {
            reactions: 42,
            ..Default::default()
        };
        let markup = engagement_bar(&counts, 0);
        let html = markup.into_string();
        assert!(html.contains("42"));
        assert!(html.contains("Reactions"));
    }

    #[test]
    fn engagement_bar_with_timestamp() {
        let counts = EngagementCounts::default();
        let markup = engagement_bar(&counts, 1_704_067_200);
        let html = markup.into_string();
        assert!(html.contains("Jan 01, 2024"));
    }

    #[test]
    fn engagement_bar_with_zaps() {
        let counts = EngagementCounts {
            zap_msats: 21_000_000,
            zap_count: 5,
            ..Default::default()
        };
        let markup = engagement_bar(&counts, 0);
        let html = markup.into_string();
        assert!(html.contains("21.0K sats"));
        assert!(html.contains("5 zaps"));
    }
}
