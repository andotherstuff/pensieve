//! Open Graph image generation.
//!
//! Generates branded OG preview images on the fly:
//! - Black background (1200x630, standard OG dimensions)
//! - "NSTR.TO" block lettering centered
//! - Author avatar composited as a circle in the bottom-left corner
//!
//! Images are cached in-memory to avoid regeneration on repeated requests.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::error::PreviewError;
use crate::resolve;
use crate::state::AppState;

/// OG image dimensions (standard Open Graph).
const OG_WIDTH: u32 = 1200;
const OG_HEIGHT: u32 = 630;

/// Avatar size in the bottom-left corner.
const AVATAR_SIZE: u32 = 120;

/// Padding from the edge for the avatar.
const AVATAR_PADDING: u32 = 40;

/// Handle a request for an OG preview image.
///
/// Route: `GET /og/{identifier}.png`
pub async fn og_image_handler(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> Result<Response, PreviewError> {
    let identifier = identifier
        .strip_suffix(".png")
        .unwrap_or(&identifier)
        .trim();

    // Check OG image cache
    if let Some(cached) = state.og_cache.get(identifier).await {
        tracing::trace!(identifier = %identifier, "og image cache hit");
        return Ok(png_response(&cached));
    }

    tracing::trace!(identifier = %identifier, "og image cache miss, generating");

    // Resolve the identifier to get the avatar URL
    let avatar_url = match resolve::resolve(&state, identifier).await {
        Ok(content) => extract_avatar_url(&content),
        Err(_) => None,
    };

    // Fetch avatar bytes if we have a URL
    let avatar_data = if let Some(url) = &avatar_url {
        fetch_avatar(url).await
    } else {
        None
    };

    // Generate the OG image
    let png_bytes = generate_og_image(avatar_data.as_deref())?;

    // Cache the result
    state
        .og_cache
        .insert(identifier.to_string(), png_bytes.clone())
        .await;

    Ok(png_response(&png_bytes))
}

/// Build an HTTP response with PNG content and cache headers.
fn png_response(png_bytes: &[u8]) -> Response {
    let headers = [
        (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600, s-maxage=86400"),
        ),
    ];

    (StatusCode::OK, headers, png_bytes.to_vec()).into_response()
}

/// Extract the avatar URL from resolved content.
fn extract_avatar_url(content: &resolve::ResolvedContent) -> Option<String> {
    match content {
        resolve::ResolvedContent::Profile { metadata, .. } => metadata.picture.clone(),
        resolve::ResolvedContent::Event { author, .. } => {
            author.as_ref().and_then(|a| a.picture.clone())
        }
    }
}

/// Fetch an avatar image from a URL, with a timeout.
///
/// Returns the raw image bytes, or `None` if the fetch fails.
///
/// SSRF guard: the `picture` URL is attacker-controlled (it comes from a Nostr
/// profile event), so before fetching we require an `http(s)` scheme, resolve the
/// host and reject any address that is not globally routable (loopback, private,
/// link-local/metadata, CGNAT, etc.), and disable redirect following so a public
/// URL cannot bounce to an internal one. (A determined attacker could still race
/// DNS between our resolution and reqwest's; pinning the resolved IP would close
/// that, but redirect-disable + resolve-check covers the practical cases.)
async fn fetch_avatar(url: &str) -> Option<Vec<u8>> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default().unwrap_or(443);

    // Resolve the host and ensure every resolved address is globally routable.
    let mut resolved_any = false;
    for addr in tokio::net::lookup_host((host, port)).await.ok()? {
        resolved_any = true;
        if !is_global_ip(addr.ip()) {
            tracing::debug!(%host, ip = %addr.ip(), "blocked avatar fetch to non-global address");
            return None;
        }
    }
    if !resolved_any {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;

    let resp = client.get(url).send().await.ok()?;

    if !resp.status().is_success() {
        return None;
    }

    // Limit to 5MB to avoid memory issues
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() > 5_000_000 {
        return None;
    }

    Some(bytes.to_vec())
}

/// Whether an IP address is globally routable (i.e. safe to fetch from).
///
/// Conservatively rejects loopback, private, link-local (incl. 169.254.169.254
/// cloud metadata), CGNAT, documentation, unspecified, and IPv6 ULA/link-local.
fn is_global_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_global_v4(v4),
        std::net::IpAddr::V6(v6) => {
            // IPv4-mapped IPv6 (::ffff:a.b.c.d) must be judged by its IPv4 value.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_global_v4(v4);
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00   // ULA fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80) // link-local fe80::/10
        }
    }
}

/// IPv4 global-routability check.
fn is_global_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64)) // CGNAT 100.64.0.0/10
}

/// Font family string for SVG text (sans single quotes that confuse `format!`).
const FONT_FAMILY: &str = "Inter, -apple-system, BlinkMacSystemFont, Segoe UI, Roboto, sans-serif";

/// Generate the OG image as a PNG.
///
/// Layout:
/// - 1200x630 black background
/// - "NSTR.TO" centered in white block lettering
/// - Optional avatar as a circle in the bottom-left corner
fn generate_og_image(avatar_bytes: Option<&[u8]>) -> Result<Vec<u8>, PreviewError> {
    let mut svg = String::with_capacity(4096);

    // SVG header + black background
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{w}" height="{h}" viewBox="0 0 {w} {h}"><rect width="{w}" height="{h}" fill="#000"/>"##,
        w = OG_WIDTH,
        h = OG_HEIGHT,
    ));

    // Avatar in bottom-left corner (if available)
    if let Some(bytes) = avatar_bytes {
        let mime = detect_image_mime(bytes);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);

        let x = AVATAR_PADDING;
        let y = OG_HEIGHT - AVATAR_SIZE - AVATAR_PADDING;
        let r = AVATAR_SIZE / 2;
        let cx = x + r;
        let cy = y + r;

        svg.push_str(&format!(
            r##"<defs><clipPath id="ac"><circle cx="{cx}" cy="{cy}" r="{r}"/></clipPath></defs><circle cx="{cx}" cy="{cy}" r="{r}" fill="#222"/><image href="data:{mime};base64,{b64}" x="{x}" y="{y}" width="{sz}" height="{sz}" clip-path="url(#ac)" preserveAspectRatio="xMidYMid slice"/>"##,
            cx = cx,
            cy = cy,
            r = r,
            mime = mime,
            b64 = b64,
            x = x,
            y = y,
            sz = AVATAR_SIZE,
        ));
    }

    // Centered "NSTR.TO" text
    let text_x = OG_WIDTH / 2;
    let text_y = OG_HEIGHT / 2;
    svg.push_str(&format!(
        r##"<text x="{x}" y="{y}" text-anchor="middle" dominant-baseline="central" font-family="{font}" font-size="96" font-weight="900" letter-spacing="-2" fill="#fff">NSTR<tspan fill="#d946ef">.TO</tspan></text>"##,
        x = text_x,
        y = text_y,
        font = FONT_FAMILY,
    ));

    svg.push_str("</svg>");

    // Parse and render the SVG to a pixel buffer
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(&svg, &options)
        .map_err(|e| PreviewError::Internal(anyhow::anyhow!("SVG parse error: {e}")))?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(OG_WIDTH, OG_HEIGHT)
        .ok_or_else(|| PreviewError::Internal(anyhow::anyhow!("failed to create pixmap")))?;

    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );

    // Encode as PNG
    let png_data = pixmap
        .encode_png()
        .map_err(|e| PreviewError::Internal(anyhow::anyhow!("PNG encode error: {e}")))?;

    Ok(png_data)
}

/// Detect MIME type from image bytes (basic magic byte detection).
fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        // Default to JPEG — most profile pictures are JPEG
        "image/jpeg"
    }
}
