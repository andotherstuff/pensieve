//! Profile (kind 0) renderer.
//!
//! Renders a user's profile page with banner, avatar, name, NIP-05,
//! about text, and metadata links.

use maud::{html, Markup};

use maud::PreEscaped;

use super::components::{
    self, is_safe_url, nostr_link, page_shell, truncate, truncate_npub_display, OpenGraphData,
    ICON_COPY,
};
use crate::query::ProfileMetadata;

/// Render a profile preview page.
pub fn render(
    _pubkey_hex: &str,
    npub: &str,
    nprofile: &str,
    metadata: &ProfileMetadata,
    base_url: &str,
    site_name: &str,
) -> Markup {
    let name = metadata.display_name();
    let about = metadata.about.as_deref().unwrap_or("");
    let title = format!("{name} on Nostr");
    let description = if about.is_empty() {
        format!("Nostr profile: {npub}")
    } else {
        truncate(about, 200)
    };
    let canonical = format!("{base_url}/{npub}");

    let og_image_url = format!("{base_url}/og/{npub}.png");

    let og = OpenGraphData {
        title: &title,
        description: &description,
        og_type: "profile",
        image: Some(&og_image_url),
        twitter_card_type: "summary_large_image",
    };

    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();

    let body = html! {
        div class="card" {
            // Banner — always show gradient, overlay image if available
            div class="profile-banner-fallback" {
                @if let Some(banner_url) = metadata.banner.as_deref() {
                    @if is_safe_url(banner_url) {
                        img class="profile-banner" src=(banner_url) alt="" loading="lazy" onerror="this.style.display='none'";
                    }
                }
            }

            // Profile header (avatar + name, overlapping banner)
            div class="profile-header" {
                div class="profile-pic" {
                    (initial.as_str())
                    @if let Some(pic_url) = metadata.picture.as_deref() {
                        @if is_safe_url(pic_url) {
                            img src=(pic_url) alt=(name) loading="lazy" onerror="this.style.display='none'";
                        }
                    }
                }
            }

            div class="profile-body" {
                div class="profile-name" { (name) }

                @if let Some(nip05) = metadata.nip05.as_deref() {
                    div class="profile-nip05" { (nip05) }
                }

                div class="author-npub" {
                    span class="author-npub-text" title=(npub) { (truncate_npub_display(npub)) }
                    button class="copy-btn" onclick=(format!("navigator.clipboard.writeText('{}').then(()=>{{this.innerHTML='Copied!';setTimeout(()=>this.innerHTML='{}',1500)}})", npub, ICON_COPY.replace('"', "&quot;"))) title="Copy npub" {
                        (PreEscaped(ICON_COPY))
                    }
                }

                @if !about.is_empty() {
                    p class="profile-about" { (about) }
                }

                // Metadata links
                div class="profile-meta" {
                    @if let Some(website) = metadata.website.as_deref() {
                        @if is_safe_url(website) {
                            a href=(website) rel="nofollow noopener" target="_blank" {
                                (components::truncate(website.strip_prefix("https://").or_else(|| website.strip_prefix("http://")).unwrap_or(website), 40))
                            }
                        }
                    }

                    @if let Some(lud16) = metadata.lud16.as_deref() {
                        span title="Lightning Address" {
                            (maud::PreEscaped(components::ICON_LIGHTNING)) " " (lud16)
                        }
                    }
                }

            }
        }
        (nostr_link(nprofile))
    };

    page_shell(&title, &description, &canonical, og, body, site_name)
}
