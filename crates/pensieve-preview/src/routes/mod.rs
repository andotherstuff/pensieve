//! Route definitions for the preview service.
//!
//! ## Routes
//!
//! - `GET /` - Home page
//! - `GET /health` - Health check (JSON)
//! - `GET /robots.txt` - Crawler instructions
//! - `GET /llms.txt` - LLM agent description
//! - `GET /llms-full.txt` - LLM agent full documentation
//! - `GET /{identifier}` - Preview page (or .json for API)

mod health;
mod home;
pub mod json;
mod llms;
mod og;
mod preview;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::state::AppState;

/// Build the complete preview service router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home::home_page))
        .route("/health", get(health::health_check))
        .route("/robots.txt", get(robots_txt))
        .route("/favicon.svg", get(favicon_svg))
        .route("/favicon.ico", get(favicon_svg))
        .route("/app.js", get(app_js))
        .route("/llms.txt", get(llms::llms_txt))
        .route("/llms-full.txt", get(llms::llms_full_txt))
        .route("/og/{identifier}", get(og::og_image_handler))
        .route("/{identifier}", get(preview::preview_handler))
        .with_state(state)
}

/// Serve robots.txt allowing all crawlers.
async fn robots_txt() -> impl IntoResponse {
    (
        [("content-type", "text/plain; charset=utf-8")],
        "User-agent: *\nAllow: /\n",
    )
}

/// SVG favicon — bold lowercase "n", adapts to light/dark mode.
const FAVICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><style>text{font-family:Inter,-apple-system,sans-serif}@media(prefers-color-scheme:light){rect{fill:#fff}text{fill:#111}}@media(prefers-color-scheme:dark){rect{fill:#0a0a0f}text{fill:#e5e5e5}}</style><rect width="32" height="32" rx="6" fill="#fff"/><text x="16" y="25" text-anchor="middle" font-size="26" font-weight="800" fill="#111">n</text></svg>"##;

/// Serve the SVG favicon.
async fn favicon_svg() -> impl IntoResponse {
    (
        [
            ("content-type", "image/svg+xml"),
            ("cache-control", "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
}

/// Client-side behavior served as a same-origin script.
///
/// All interactivity (image lightbox, copy-to-clipboard buttons, hiding broken
/// images, and the home-page search box) lives here so the Content-Security-Policy
/// can forbid inline scripts and inline event handlers (`script-src 'self'`).
/// Everything is wired with event delegation + `data-` attributes, so no
/// attacker-controlled value is ever interpolated into executable JS.
const APP_JS: &str = r#"(function () {
  "use strict";
  // 'error' events don't bubble, so hide broken images via a capture-phase listener.
  document.addEventListener("error", function (e) {
    var t = e.target;
    if (t && t.tagName === "IMG" && t.hasAttribute("data-hide-on-error")) {
      t.style.display = "none";
    }
  }, true);

  document.addEventListener("click", function (e) {
    var el = e.target;
    if (!el || !el.closest) return;

    var item = el.closest(".media-item[data-full]");
    if (item) {
      var img = document.getElementById("lightbox-img");
      var box = document.getElementById("lightbox");
      if (img && box) {
        img.src = item.getAttribute("data-full");
        box.classList.add("active");
      }
      return;
    }

    if (el.closest(".lightbox-overlay")) {
      var overlay = document.getElementById("lightbox");
      if (overlay) overlay.classList.remove("active");
      return;
    }

    var copy = el.closest(".copy-btn[data-copy]");
    if (copy && navigator.clipboard) {
      var orig = copy.innerHTML;
      navigator.clipboard.writeText(copy.getAttribute("data-copy")).then(function () {
        copy.innerHTML = "Copied!";
        setTimeout(function () { copy.innerHTML = orig; }, 1500);
      });
      return;
    }
  });

  var inp = document.querySelector(".home-input");
  if (inp) {
    inp.addEventListener("input", function () {
      var v = this.value;
      if (v.toLowerCase().indexOf("nostr:") === 0) this.value = v.slice(6);
    });
  }
  var form = document.querySelector(".home-search");
  if (form) {
    form.addEventListener("submit", function (e) {
      e.preventDefault();
      var q = inp ? inp.value.trim().replace(/\s+/g, "") : "";
      if (q) window.location.href = "/" + encodeURIComponent(q);
    });
  }
})();
"#;

/// Serve the client-side script.
async fn app_js() -> impl IntoResponse {
    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("cache-control", "public, max-age=86400"),
        ],
        APP_JS,
    )
}
