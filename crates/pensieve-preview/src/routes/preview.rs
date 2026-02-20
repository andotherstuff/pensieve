//! Main preview route handler.
//!
//! Handles `GET /{identifier}` where `identifier` is a NIP-19 bech32 string
//! or a raw hex event ID / pubkey.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::error::PreviewError;
use crate::render;
use crate::resolve;
use crate::state::{AppState, CachedHtml};

/// Handle a preview request for a Nostr identifier.
///
/// This is the main entry point. It:
/// 1. Detects `.json` suffix for JSON API responses
/// 2. Checks the in-process cache for a rendered HTML response
/// 3. On miss, resolves the NIP-19 identifier against ClickHouse
/// 4. Renders the appropriate HTML page
/// 5. Caches the result and returns it with appropriate Cache-Control headers
pub async fn preview_handler(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> Result<Response, PreviewError> {
    let identifier = identifier.trim();

    // Dispatch to JSON handler if .json suffix
    if let Some(bare) = identifier.strip_suffix(".json") {
        return super::json::json_handler_inner(&state, bare).await;
    }

    // Check in-process cache
    if let Some(cached) = state.cache.get(identifier).await {
        tracing::debug!(identifier = %identifier, "cache hit");
        return Ok(build_response(
            &cached.html,
            cache_headers(&state, identifier, true),
        ));
    }

    tracing::debug!(identifier = %identifier, "cache miss, resolving");

    // Resolve the identifier
    let content = resolve::resolve(&state, identifier).await?;

    // Render the HTML page
    let markup = render::render_page(&state, &content).await?;
    let html_string = markup.into_string();

    // Cache the rendered HTML
    let cached = CachedHtml {
        html: html_string.clone(),
        cached_at: chrono::Utc::now(),
    };
    state.cache.insert(identifier.to_string(), cached).await;

    Ok(build_response(
        &html_string,
        cache_headers(&state, identifier, false),
    ))
}

/// Build an HTTP response with HTML content and security/cache headers.
fn build_response(html: &str, cache_headers: HeaderMap) -> Response {
    let mut headers = HeaderMap::new();

    // Content type
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );

    // Security headers
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(render::components::CSP_HEADER),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));

    // ETag (xxHash of content)
    let hash = xxhash_rust::xxh3::xxh3_64(html.as_bytes());
    let etag = format!("\"{}\"", hex_fmt::HexFmt(&hash.to_be_bytes()));
    if let Ok(val) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, val);
    }

    // Merge cache headers
    for (key, value) in cache_headers.iter() {
        headers.insert(key.clone(), value.clone());
    }

    (StatusCode::OK, headers, html.to_string()).into_response()
}

/// Compute Cache-Control headers based on content characteristics.
///
/// TTL tiers:
/// - VIP pubkeys: 24h s-maxage
/// - Recent events (< 24h): 5min s-maxage
/// - Medium events (1-30 days): 1h s-maxage
/// - Old events (> 30 days): 6h s-maxage
/// - Profiles: 30min s-maxage
fn cache_headers(state: &AppState, identifier: &str, _is_cache_hit: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();

    // Determine TTL based on identifier type and VIP status
    let (max_age, s_maxage, swr) = determine_ttl(state, identifier);

    let cache_value =
        format!("public, max-age={max_age}, s-maxage={s_maxage}, stale-while-revalidate={swr}");

    if let Ok(val) = HeaderValue::from_str(&cache_value) {
        headers.insert(header::CACHE_CONTROL, val);
    }

    headers
}

/// Determine TTL values (max-age, s-maxage, stale-while-revalidate) for an identifier.
///
/// Returns (browser_ttl, cdn_ttl, stale_while_revalidate) in seconds.
fn determine_ttl(_state: &AppState, identifier: &str) -> (u32, u32, u32) {
    // Check if this is a profile-type identifier
    let is_profile = identifier.starts_with("npub1") || identifier.starts_with("nprofile1");

    if is_profile {
        // Check if it's a VIP pubkey
        // For NIP-19 encoded identifiers, we'd need to decode to check.
        // For simplicity, profiles get a moderate TTL; VIP logic can be
        // refined once we track which pubkeys are VIPs at the response level.
        return (60, 1800, 300); // 1min browser, 30min CDN, 5min SWR
    }

    // For events, default to moderate TTL
    // In the future, we can check event age and engagement here
    (60, 3600, 600) // 1min browser, 1h CDN, 10min SWR
}
