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
        .time_to_live(DEFAULT_TTL)
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
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
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
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
{
    if let Some(value) = try_get_cached(cache, key).await? {
        return Ok(value);
    }

    let key_lock = {
        let mut map = IN_FLIGHT.lock().await;
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };

    let _guard = key_lock.lock().await;

    if let Some(value) = try_get_cached(cache, key).await? {
        return Ok(value);
    }

    tracing::debug!(key = %key, ttl_secs = ttl.as_secs(), "cache miss, computing");
    let value = compute().await?;

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
}

async fn try_get_cached<T>(cache: &ResponseCache, key: &str) -> Result<Option<T>, ApiError>
where
    T: DeserializeOwned,
{
    if let Some(entry) = cache.get(key).await {
        if chrono::Utc::now() > entry.expires_at {
            cache.invalidate(key).await;
            tracing::debug!(key = %key, expired_at = %entry.expires_at, "cache expired");
            return Ok(None);
        }

        return match serde_json::from_str(&entry.json) {
            Ok(value) => {
                tracing::debug!(
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
}
