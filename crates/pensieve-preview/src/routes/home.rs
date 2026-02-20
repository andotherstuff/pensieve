//! Home page — simple landing page explaining the service.

use axum::response::IntoResponse;
use maud::{DOCTYPE, PreEscaped, html};

use crate::render::components::PAGE_CSS;

/// Render the home page.
pub async fn home_page() -> impl IntoResponse {
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "nstr.to — Nostr Event Previews" }
                meta name="description" content="Fast, static preview pages for any Nostr event. Just add any npub, note, or nevent to the URL.";
                meta property="og:title" content="nstr.to";
                meta property="og:description" content="Fast, static preview pages for any Nostr event.";
                meta property="og:type" content="website";
                style { (PreEscaped(PAGE_CSS)) }
                style { (PreEscaped(HOME_CSS)) }
            }
            body {
                main class="home" {
                    h1 class="home-title" {
                        "nstr" span class="home-dot" { ".to" }
                    }
                    p class="home-tagline" {
                        "Fast, static preview pages for any Nostr event."
                    }

                    div class="home-how" {
                        p {
                            "Add any Nostr identifier to the URL:"
                        }
                        div class="home-examples" {
                            div class="home-example" {
                                span class="home-label" { "Profile" }
                                code { "nstr.to/" span class="home-highlight" { "npub1..." } }
                            }
                            div class="home-example" {
                                span class="home-label" { "Note" }
                                code { "nstr.to/" span class="home-highlight" { "note1..." } }
                            }
                            div class="home-example" {
                                span class="home-label" { "Event" }
                                code { "nstr.to/" span class="home-highlight" { "nevent1..." } }
                            }
                            div class="home-example" {
                                span class="home-label" { "Article" }
                                code { "nstr.to/" span class="home-highlight" { "naddr1..." } }
                            }
                        }
                    }

                    div class="home-agents" {
                        p { "And for agents:" }
                        div class="home-examples" {
                            div class="home-example" {
                                span class="home-label" { "JSON" }
                                code { "nstr.to/nevent1..." span class="home-highlight" { ".json" } }
                            }
                        }
                    }

                    div class="home-try" {
                        p { "Try one:" }
                        a href="/npub1zuuajd7u3sx8xu92yav9jwxpr839cs0kc3q6t56vd5u9q033xmhsk6c2uc" {
                            "View a profile"
                        }
                        a href="/nevent1qqsy7azsu70uny8dpxrnk72guunl6yhscj7c2hqyz4n93j283x609xgru7sw6" {
                            "View a note"
                        }
                    }
                }
                footer class="footer" {
                    "Powered by "
                    a href="https://github.com/andotherstuff/pensieve" { "Pensieve" }
                    ", "
                    a href="https://andotherstuff.org" { "And Other Stuff" }
                }
            }
        }
    };

    markup
}

/// Additional CSS for the home page only.
const HOME_CSS: &str = r#"
.home{display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:60vh;text-align:center;padding:2rem 1rem}
.home-title{font-size:3.5rem;font-weight:800;letter-spacing:-.04em;color:var(--fg)}
.home-dot{color:var(--accent)}
.home-tagline{font-size:1.15rem;color:var(--fg2);margin-top:.5rem;max-width:400px}
.home-how{margin-top:2.5rem;width:100%;max-width:420px}
.home-how>p{font-size:.95rem;color:var(--fg2);margin-bottom:1rem}
.home-examples{display:flex;flex-direction:column;gap:.5rem}
.home-example{display:flex;align-items:center;gap:.75rem;padding:.5rem .75rem;border-radius:6px;border:1px solid var(--border)}
.home-label{font-size:.75rem;font-weight:600;color:var(--fg3);text-transform:uppercase;letter-spacing:.05em;width:52px;text-align:right;flex-shrink:0}
.home-example code{font-family:var(--mono);font-size:.85rem;color:var(--fg2)}
.home-highlight{color:var(--accent);font-weight:600}
.home-agents{margin-top:1.75rem;width:100%;max-width:420px}
.home-agents>p{font-size:.95rem;color:var(--fg2);margin-bottom:1rem}
.home-try{margin-top:2rem;display:flex;flex-direction:column;align-items:center;gap:.75rem}
.home-try p{font-size:.85rem;color:var(--fg3)}
.home-try a{font-size:.9rem;color:var(--accent);text-decoration:none}
.home-try a:hover{text-decoration:underline}
"#;
