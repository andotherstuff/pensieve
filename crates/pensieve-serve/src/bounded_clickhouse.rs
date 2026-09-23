//! API-only ClickHouse admission control. Ingestion uses its own client.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use clickhouse::{Client, Row, query::Query, sql::Bind};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::ApiError;

/// Read client with two running queries and at most sixteen queued calls.
#[derive(Clone)]
pub struct BoundedClickHouse {
    client: Client,
    permits: Arc<Semaphore>,
    admission: Arc<Semaphore>,
}

impl BoundedClickHouse {
    /// Create an API client with server-side CPU and execution limits.
    pub fn new(url: &str, database: &str) -> Self {
        Self {
            client: Client::default()
                .with_url(url)
                .with_database(database)
                .with_option("max_threads", "4")
                .with_option("max_execution_time", "20")
                .with_option("timeout_overflow_mode", "throw")
                .with_option("cancel_http_readonly_queries_on_client_close", "1"),
            permits: Arc::new(Semaphore::new(2)),
            admission: Arc::new(Semaphore::new(18)),
        }
    }

    /// Prepare a query; admission happens only when it is executed.
    pub fn query(&self, sql: &str) -> BoundedQuery {
        BoundedQuery {
            query: self.client.query(sql),
            permits: self.permits.clone(),
            admission: self.admission.clone(),
        }
    }
}

/// Query whose permit remains held even if its HTTP caller disconnects.
pub struct BoundedQuery {
    query: Query,
    permits: Arc<Semaphore>,
    admission: Arc<Semaphore>,
}

async fn admitted<T, F>(
    permits: Arc<Semaphore>,
    admission: Arc<Semaphore>,
    work: F,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: Future<Output = clickhouse::error::Result<T>> + Send + 'static,
{
    admitted_with_timeout(permits, admission, work, Duration::from_secs(25)).await
}

async fn admitted_with_timeout<T, F>(
    permits: Arc<Semaphore>,
    admission: Arc<Semaphore>,
    work: F,
    deadline: Duration,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: Future<Output = clickhouse::error::Result<T>> + Send + 'static,
{
    let slot = admission
        .try_acquire_owned()
        .map_err(|_| ApiError::Overloaded)?;
    let permit = tokio::time::timeout(std::time::Duration::from_secs(2), permits.acquire_owned())
        .await
        .map_err(|_| ApiError::Overloaded)?
        .map_err(|_| ApiError::Overloaded)?;
    // Do not free a slot on HTTP cancellation while ClickHouse is still working.
    tokio::spawn(async move {
        let _permit = permit;
        let _slot = slot;
        // The server's 20-second deadline is cooperative. Bound connection and
        // body-read stalls too, even after the original HTTP caller disconnects.
        tokio::time::timeout(deadline, work)
            .await
            .map_err(|_| ApiError::Overloaded)?
            .map_err(ApiError::from)
    })
    .await
    .map_err(|error| ApiError::Internal(error.into()))?
}

impl BoundedQuery {
    /// Bind an escaped SQL argument.
    pub fn bind(mut self, value: impl Bind) -> Self {
        self.query = self.query.bind(value);
        self
    }

    /// Fetch a single owned row.
    pub async fn fetch_one<T>(self) -> Result<T, ApiError>
    where
        T: Row + DeserializeOwned + Send + 'static,
    {
        admitted(self.permits, self.admission, self.query.fetch_one()).await
    }

    /// Fetch an optional owned row.
    pub async fn fetch_optional<T>(self) -> Result<Option<T>, ApiError>
    where
        T: Row + DeserializeOwned + Send + 'static,
    {
        admitted(self.permits, self.admission, self.query.fetch_optional()).await
    }

    /// Fetch all result rows while holding admission through response completion.
    pub async fn fetch_all<T>(self) -> Result<Vec<T>, ApiError>
    where
        T: Row + DeserializeOwned + Send + 'static,
    {
        admitted(self.permits, self.admission, self.query.fetch_all()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_work_is_dropped_and_all_slots_are_released() {
        let permits = Arc::new(Semaphore::new(1));
        let admission = Arc::new(Semaphore::new(1));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let result = admitted_with_timeout(
            permits.clone(),
            admission.clone(),
            async move {
                let _tx = tx;
                std::future::pending::<clickhouse::error::Result<()>>().await
            },
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(result, Err(ApiError::Overloaded)));
        assert!(rx.await.is_err());
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(admission.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_release_running_slot() {
        let permits = Arc::new(Semaphore::new(1));
        let admission = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(admitted(permits.clone(), admission.clone(), async move {
            started_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            Ok(42)
        }));
        started_rx.await.unwrap();
        caller.abort();
        let _ = caller.await;
        assert_eq!(permits.available_permits(), 0);
        assert!(matches!(
            admitted(permits.clone(), admission, async { Ok(1) }).await,
            Err(ApiError::Overloaded)
        ));
        finish_tx.send(()).unwrap();
        let _released = tokio::time::timeout(std::time::Duration::from_secs(1), permits.acquire())
            .await
            .unwrap()
            .unwrap();
    }
}
