//! In-memory response caching with moka.
//!
//! Provides server-side caching for expensive ClickHouse queries. Each cached
//! entry stores serialized JSON with metadata, enabling fast responses for
//! repeated requests.
//!
//! ## Cache Key Strategy
//!
//! Cache keys should include:
//! - Endpoint name (e.g., "new_users", "active_users_daily")
//! - All query parameters that affect the response
//!
//! ## TTL Guidelines
//!
//! | Data Type | TTL | Examples |
//! |-----------|-----|----------|
//! | Real-time | 10-30s | latest_event |
//! | Aggregates | 5 min | total_events, total_pubkeys |
//! | Time series | 5-15 min | active_users, new_users |
//! | Stable data | 30-60 min | earliest_event, kind stats |

use std::future::Future;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use moka::future::Cache;
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;

use crate::error::ApiError;

/// Default cache capacity (number of entries).
pub const DEFAULT_CACHE_CAPACITY: u64 = 1000;

/// Default TTL for cached entries.
pub const DEFAULT_TTL: Duration = ttl::AGGREGATES;

type InFlightMap = Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>;

/// Cached response with metadata.
#[derive(Clone, Debug)]
pub struct CachedEntry {
    /// Serialized JSON response.
    pub json: String,
    /// When this entry was cached.
    pub cached_at: chrono::DateTime<chrono::Utc>,
    /// When this cache entry expires.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Type alias for the response cache.
pub type ResponseCache = Cache<String, CachedEntry>;

/// Per-key in-flight locks to prevent cache stampedes.
static IN_FLIGHT: std::sync::LazyLock<InFlightMap> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Create a new response cache with default settings.
pub fn new_cache() -> ResponseCache {
    Cache::builder()
        .max_capacity(DEFAULT_CACHE_CAPACITY)
        // Freshness is enforced by each entry's expires_at, not a shorter global TTL.
        .time_to_idle(ttl::STABLE)
        .build()
}

/// Get a cached value or compute and cache it.
///
/// This is the main caching helper. It:
/// 1. Checks if a valid cached entry exists for the key
/// 2. If found, deserializes and returns it
/// 3. If not found, calls the compute function
/// 4. Caches the result and returns it
///
/// # Arguments
///
/// * `cache` - The moka cache instance
/// * `key` - Unique cache key for this request
/// * `compute` - Async function that computes the value if not cached
///
/// # Example
///
/// ```ignore
/// let result = get_or_compute(&state.cache, &cache_key, || async {
///     // Expensive ClickHouse query here
///     query_database(&state).await
/// }).await?;
/// ```
pub async fn get_or_compute<T, F, Fut>(
    cache: &ResponseCache,
    key: &str,
    compute: F,
) -> Result<T, ApiError>
where
    T: Serialize + DeserializeOwned + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, ApiError>> + Send + 'static,
{
    get_or_compute_with_ttl(cache, key, DEFAULT_TTL, compute).await
}

/// Get a cached value or compute and cache it with per-entry TTL.
pub async fn get_or_compute_with_ttl<T, F, Fut>(
    cache: &ResponseCache,
    key: &str,
    ttl: Duration,
    compute: F,
) -> Result<T, ApiError>
where
    T: Serialize + DeserializeOwned + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, ApiError>> + Send + 'static,
{
    if let Some(value) = try_get_cached(cache, key).await? {
        return Ok(value);
    }

    let key_lock = {
        let mut map = IN_FLIGHT.lock().await;
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };

    // Serve at most one additional TTL of stale aggregate data while a refresh runs.
    // Never serve stale real-time watermarks.
    let stale: Option<T> = match cache.get(key).await {
        Some(entry)
            if ttl > ttl::REALTIME
                && chrono::Utc::now()
                    <= entry.expires_at + chrono::Duration::from_std(ttl).unwrap_or_default() =>
        {
            serde_json::from_str(&entry.json).ok()
        }
        _ => None,
    };
    let guard = match key_lock.clone().try_lock_owned() {
        Ok(guard) => guard,
        Err(_) => {
            if let Some(value) = stale {
                return Ok(value);
            }
            key_lock.lock_owned().await
        }
    };

    if let Some(value) = try_get_cached(cache, key).await? {
        return Ok(value);
    }

    let cache = cache.clone();
    let key = key.to_owned();
    // Own the refresh and its lock independently of the HTTP request lifetime.
    let refresh = tokio::spawn(async move {
        let _guard = guard;
        tracing::trace!(key = %key, ttl_secs = ttl.as_secs(), "cache miss, computing");
        let value = compute().await.inspect_err(|error| {
            tracing::warn!(key = %key, error = %error, "cache refresh failed");
        })?;

        match serde_json::to_string(&value) {
            Ok(json) => {
                let now = chrono::Utc::now();
                let expires_at = now
                    + chrono::Duration::from_std(ttl)
                        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
                let entry = CachedEntry {
                    json,
                    cached_at: now,
                    expires_at,
                };
                cache.insert(key.to_string(), entry).await;
            }
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "failed to serialize for cache");
            }
        }

        Ok(value)
    });
    if let Some(value) = stale {
        return Ok(value);
    }
    refresh
        .await
        .map_err(|error| ApiError::Internal(error.into()))?
}

async fn try_get_cached<T>(cache: &ResponseCache, key: &str) -> Result<Option<T>, ApiError>
where
    T: DeserializeOwned,
{
    if let Some(entry) = cache.get(key).await {
        if chrono::Utc::now() > entry.expires_at {
            tracing::trace!(key = %key, expired_at = %entry.expires_at, "cache expired");
            return Ok(None);
        }

        return match serde_json::from_str(&entry.json) {
            Ok(value) => {
                tracing::trace!(
                    key = %key,
                    cached_at = %entry.cached_at,
                    expires_at = %entry.expires_at,
                    "cache hit"
                );
                Ok(Some(value))
            }
            Err(e) => {
                cache.invalidate(key).await;
                tracing::warn!(key = %key, error = %e, "failed to deserialize cached entry");
                Ok(None)
            }
        };
    }

    Ok(None)
}

/// Common TTL values for different endpoint types.
pub mod ttl {
    use std::time::Duration;

    /// Real-time data (e.g., latest event) - 10 seconds
    pub const REALTIME: Duration = Duration::from_secs(10);

    /// Combined overview containing the live watermark - 1 minute.
    pub const OVERVIEW: Duration = Duration::from_secs(60);

    /// Fast-changing aggregates (e.g., total counts) - 5 minutes
    pub const AGGREGATES: Duration = Duration::from_secs(300);

    /// Time series data (e.g., daily active users) - 10 minutes
    pub const TIME_SERIES: Duration = Duration::from_secs(600);

    /// Stable/slow-changing data (e.g., earliest event) - 1 hour
    pub const STABLE: Duration = Duration::from_secs(3600);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn stale_overview_is_bounded_and_realtime_never_uses_stale() {
        for (key, ttl, age, expected) in [
            ("overview_stale", ttl::OVERVIEW, 1, 1),
            ("overview_expired", ttl::OVERVIEW, 61, 2),
            ("watermark_stale", ttl::REALTIME, 1, 2),
        ] {
            let cache = new_cache();
            cache
                .insert(
                    key.to_owned(),
                    CachedEntry {
                        json: "1".to_owned(),
                        cached_at: chrono::Utc::now() - chrono::Duration::seconds(120),
                        expires_at: chrono::Utc::now() - chrono::Duration::seconds(age),
                    },
                )
                .await;
            let (release, wait) = tokio::sync::oneshot::channel();
            let request_cache = cache.clone();
            let request = tokio::spawn(async move {
                get_or_compute_with_ttl(&request_cache, key, ttl, || async move {
                    wait.await.unwrap();
                    Ok(2u64)
                })
                .await
                .unwrap()
            });
            if expected == 1 {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(1), request)
                        .await
                        .unwrap()
                        .unwrap(),
                    1
                );
                release.send(()).unwrap();
            } else {
                tokio::task::yield_now().await;
                assert!(!request.is_finished());
                release.send(()).unwrap();
                assert_eq!(request.await.unwrap(), 2);
            }
        }
    }

    #[tokio::test]
    async fn stale_aggregate_returns_while_single_refresh_runs() {
        let cache = new_cache();
        cache
            .insert(
                "stale_aggregate".to_owned(),
                CachedEntry {
                    json: "1".to_owned(),
                    cached_at: chrono::Utc::now() - chrono::Duration::seconds(301),
                    expires_at: chrono::Utc::now() - chrono::Duration::seconds(1),
                },
            )
            .await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let value: u64 = get_or_compute(&cache, "stale_aggregate", move || async move {
            started_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            Ok(2u64)
        })
        .await
        .unwrap();
        assert_eq!(value, 1);
        started_rx.await.unwrap();
        let value: u64 = get_or_compute(&cache, "stale_aggregate", || async {
            panic!("duplicate refresh")
        })
        .await
        .unwrap();
        assert_eq!(value, 1);
        finish_tx.send(()).unwrap();
    }

    #[tokio::test]
    async fn refresh_survives_cancelled_http_caller() {
        let cache = new_cache();
        let request_cache = cache.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(async move {
            get_or_compute(&request_cache, "cancelled_refresh", move || async move {
                started_tx.send(()).unwrap();
                finish_rx.await.unwrap();
                Ok(42u64)
            })
            .await
        });
        started_rx.await.unwrap();
        caller.abort();
        let _ = caller.await;
        finish_tx.send(()).unwrap();
        let result: u64 = get_or_compute(&cache, "cancelled_refresh", || async {
            panic!("must reuse the surviving refresh")
        })
        .await
        .unwrap();
        assert_eq!(42u64, result);
    }

    #[tokio::test]
    async fn test_cache_hit() {
        let cache = new_cache();
        let key = "test_key";

        // First call - cache miss
        let result: i32 = get_or_compute(&cache, key, || async { Ok(42) })
            .await
            .unwrap();
        assert_eq!(result, 42);

        // Second call - cache hit (compute should not be called)
        let result: i32 = get_or_compute(&cache, key, || async {
            panic!("compute should not be called on cache hit")
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_cache_different_keys() {
        let cache = new_cache();

        let result1: i32 = get_or_compute(&cache, "key1", || async { Ok(1) })
            .await
            .unwrap();
        let result2: i32 = get_or_compute(&cache, "key2", || async { Ok(2) })
            .await
            .unwrap();

        assert_eq!(result1, 1);
        assert_eq!(result2, 2);
    }

    #[tokio::test]
    async fn test_cache_entry_expires_and_recomputes() {
        let cache = new_cache();
        let calls = Arc::new(AtomicUsize::new(0));

        let first: usize =
            get_or_compute_with_ttl(&cache, "expiring_key", Duration::from_millis(25), {
                let calls = Arc::clone(&calls);
                move || async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(n)
                }
            })
            .await
            .unwrap();

        let second: usize =
            get_or_compute_with_ttl(&cache, "expiring_key", Duration::from_millis(25), {
                let calls = Arc::clone(&calls);
                move || async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(n)
                }
            })
            .await
            .unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(35)).await;

        let third: usize =
            get_or_compute_with_ttl(&cache, "expiring_key", Duration::from_millis(25), {
                let calls = Arc::clone(&calls);
                move || async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(n)
                }
            })
            .await
            .unwrap();

        assert_eq!(third, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_concurrent_cache_miss_is_coalesced_per_key() {
        let cache = new_cache();
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(tokio::sync::Barrier::new(8));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let calls = Arc::clone(&calls);
            let start = Arc::clone(&start);

            handles.push(tokio::spawn(async move {
                start.wait().await;
                get_or_compute_with_ttl(
                    &cache,
                    "coalesced_key",
                    Duration::from_secs(1),
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok::<u64, ApiError>(99)
                    },
                )
                .await
            }));
        }

        for handle in handles {
            let value = handle.await.unwrap().unwrap();
            assert_eq!(value, 99);
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_invalid_cached_json_recomputes_and_replaces_entry() {
        let cache = new_cache();
        cache
            .insert(
                "bad_json".to_string(),
                CachedEntry {
                    json: "not-json".to_string(),
                    cached_at: chrono::Utc::now(),
                    expires_at: chrono::Utc::now() + chrono::Duration::seconds(60),
                },
            )
            .await;

        let value: i32 = get_or_compute(&cache, "bad_json", || async { Ok(7) })
            .await
            .unwrap();
        assert_eq!(value, 7);

        let entry = cache.get("bad_json").await.unwrap();
        assert_eq!(entry.json, "7");
    }
}
