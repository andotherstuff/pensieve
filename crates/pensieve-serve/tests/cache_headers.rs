//! `Cache-Control` through the full router: the `/api/v1` nest, auth and the
//! cache-header middleware, exactly as `pensieve-serve` serves them.
//!
//! The middleware sits on the router nested at `/api/v1`, where axum strips
//! the nest prefix from the request URI. Before this test it compared full
//! `/api/v1/...` paths, so every endpoint got the 60 s default.
//!
//! No ClickHouse is needed: the ClickHouse-backed handlers are answered from a
//! pre-filled response cache, `/ping` touches no database, and the relay routes
//! read a scratch SQLite database.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use pensieve_serve::cache::CachedEntry;
use pensieve_serve::{AppState, Config, router};
use rusqlite::Connection;
use tower::ServiceExt;

const TOKEN: &str = "cache-headers-test-token";

/// Router over an unreachable ClickHouse with the given response-cache
/// entries pre-filled, plus a relay database.
async fn api(dir: &tempfile::TempDir, cached: &[(&str, &str)]) -> Router {
    let relay_db = dir.path().join("relay-stats.db");
    Connection::open(&relay_db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE relays (url TEXT PRIMARY KEY, status TEXT);
             CREATE TABLE relay_stats_hourly (relay_url TEXT, hour_start INTEGER,
                 events_received INTEGER, events_novel INTEGER);",
        )
        .unwrap();
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        // Nothing listens here: a cache miss would fail, not reach a server.
        clickhouse_url: "http://127.0.0.1:9".into(),
        clickhouse_database: "nostr".into(),
        latest_event_watermark_path: None,
        latest_event_watermark_max_age_secs: 900,
        api_tokens: [TOKEN.to_string()].into_iter().collect(),
        relay_db_path: Some(relay_db),
    };
    let state = AppState::new(config).await.unwrap();
    let now = chrono::Utc::now();
    for (key, json) in cached {
        state
            .cache
            .insert(
                (*key).to_string(),
                CachedEntry {
                    json: (*json).to_string(),
                    cached_at: now,
                    expires_at: now + chrono::Duration::hours(1),
                },
            )
            .await;
    }
    router(state)
}

async fn get(api: &Router, path: &str, if_none_match: Option<&str>) -> axum::response::Response {
    let mut request = Request::get(path).header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if let Some(etag) = if_none_match {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    api.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn cache_control(response: &axum::response::Response) -> Option<&str> {
    response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|value| value.to_str().unwrap())
}

#[tokio::test]
async fn each_endpoint_gets_its_own_cache_policy() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(
        &dir,
        &[
            ("latest_event", r#"{"timestamp":1760000000}"#),
            ("total_events", r#"{"count":42}"#),
            ("total_pubkeys", r#"{"count":7}"#),
            ("total_kinds", r#"{"count":3}"#),
            ("earliest_event", r#"{"timestamp":1606719600}"#),
            ("kind_activity:kind=1&group_by=day&limit=30", "[]"),
            ("hourly_activity:days=7&kind=", "[]"),
        ],
    )
    .await;

    let expected = [
        (
            "/api/v1/stats/activity/hourly?days=7",
            "private, max-age=600, stale-while-revalidate=1800",
        ),
        (
            "/api/v1/stats/events/latest",
            "private, max-age=10, stale-while-revalidate=30",
        ),
        (
            "/api/v1/stats/events/total",
            "private, max-age=300, stale-while-revalidate=600",
        ),
        (
            "/api/v1/stats/pubkeys/total",
            "private, max-age=300, stale-while-revalidate=600",
        ),
        (
            "/api/v1/kinds/1/activity",
            "private, max-age=600, stale-while-revalidate=1800",
        ),
        (
            "/api/v1/stats/kinds/total",
            "private, max-age=3600, stale-while-revalidate=7200",
        ),
        (
            "/api/v1/stats/events/earliest",
            "private, max-age=3600, stale-while-revalidate=7200",
        ),
        (
            "/api/v1/ping",
            "private, max-age=60, stale-while-revalidate=300",
        ),
        (
            "/api/v1/relays/summary",
            "private, max-age=60, stale-while-revalidate=300",
        ),
    ];
    for (path, policy) in expected {
        let response = get(&api, path, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(cache_control(&response), Some(policy), "{path}");
        assert!(response.headers().contains_key(header::ETAG), "{path}");
    }

    // `/stats/engagement` has the 10-minute policy but is disabled: a 503
    // carries no cache headers until it is re-enabled.
    let engagement = get(&api, "/api/v1/stats/engagement", None).await;
    assert_eq!(engagement.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cache_control(&engagement), None);
}

#[tokio::test]
async fn not_modified_keeps_the_endpoint_policy() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(&dir, &[("latest_event", r#"{"timestamp":1760000000}"#)]).await;

    let first = get(&api, "/api/v1/stats/events/latest", None).await;
    let etag = first.headers()[header::ETAG].to_str().unwrap().to_string();
    let body = first.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], br#"{"timestamp":1760000000}"#);

    let revalidated = get(&api, "/api/v1/stats/events/latest", Some(&etag)).await;
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        cache_control(&revalidated),
        Some("private, max-age=10, stale-while-revalidate=30")
    );
}

#[tokio::test]
async fn errors_are_not_marked_cacheable() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(&dir, &[]).await;

    let unauthorized = api
        .clone()
        .oneshot(
            Request::get("/api/v1/stats/events/latest")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(cache_control(&unauthorized), None);

    let bad_request = get(&api, "/api/v1/kinds/1/activity?group_by=year", None).await;
    assert_eq!(bad_request.status(), StatusCode::BAD_REQUEST);
    assert_eq!(cache_control(&bad_request), None);
}
