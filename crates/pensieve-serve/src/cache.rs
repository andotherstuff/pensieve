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

use moka::future::Cache;
use serde::{Serialize, de::DeserializeOwned};

use crate::error::ApiError;

/// Default cache capacity (number of entries).
pub const DEFAULT_CACHE_CAPACITY: u64 = 1000;

/// Default TTL for cached entries.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Cached response with metadata.
#[derive(Clone, Debug)]
pub struct CachedEntry {
    /// Serialized JSON response.
    pub json: String,
    /// When this entry was cached.
    pub cached_at: chrono::DateTime<chrono::Utc>,
}

/// Type alias for the response cache.
pub type ResponseCache = Cache<String, CachedEntry>;

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
    // Check cache first
    if let Some(entry) = cache.get(key).await {
        match serde_json::from_str(&entry.json) {
            Ok(value) => {
                tracing::debug!(key = %key, cached_at = %entry.cached_at, "cache hit");
                return Ok(value);
            }
            Err(e) => {
                // Corrupted cache entry - log and continue to recompute
                tracing::warn!(key = %key, error = %e, "failed to deserialize cached entry");
            }
        }
    }

    // Cache miss - compute the value
    tracing::debug!(key = %key, "cache miss, computing");
    let value = compute().await?;

    // Serialize and cache the result
    match serde_json::to_string(&value) {
        Ok(json) => {
            let entry = CachedEntry {
                json,
                cached_at: chrono::Utc::now(),
            };
            cache.insert(key.to_string(), entry).await;
        }
        Err(e) => {
            // Failed to serialize - log but still return the value
            tracing::warn!(key = %key, error = %e, "failed to serialize for cache");
        }
    }

    Ok(value)
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
