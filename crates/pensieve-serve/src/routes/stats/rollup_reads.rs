//! ClickHouse reads for `/stats/publishers` and `/stats/longform` from the
//! migration 019 rollup tables (`publisher_activity`, `longform_latest`).
//!
//! Both endpoints used to scan `events_local` and time out at the 20 s query
//! cap. When the rollup tables do not exist (019 not applied, or rolled back)
//! they answer `503 metric_unavailable` at once instead of falling back to a
//! scan. See `crate::rollups` and `docs/api_rollups.md`.

use clickhouse::Row;
use serde::Deserialize;

use super::{LongformStats, PublisherRow};
use crate::error::ApiError;
use crate::rollups::{
    PublisherParamError, is_unknown_table, longform_sql, publisher_params, publishers_sql,
};
use crate::state::AppState;

/// Map a missing rollup table to `503 metric_unavailable`.
fn missing_rollup_is_unavailable(error: ApiError) -> ApiError {
    match &error {
        ApiError::Database(inner) if is_unknown_table(inner) => {
            tracing::warn!(
                error = %inner,
                "API rollup table missing; apply docs/migrations/prepared/019_api_publishers_longform.sql"
            );
            ApiError::MetricUnavailable
        }
        _ => error,
    }
}

/// Top publishers from `publisher_activity`.
///
/// The window is whole UTC days: it starts at the beginning of the UTC day
/// containing `now() - days` and has no upper bound, so it covers between
/// `days` and `days + 1` days. Kinds 445 and 1059 are not ranked and `days`
/// must be 1..=366 (the rollup keeps 400 days).
pub(super) async fn fetch_clickhouse_publishers(
    state: &AppState,
    days: u32,
    kind: Option<u16>,
    limit: u32,
) -> Result<Vec<PublisherRow>, ApiError> {
    let kind = publisher_params(days, kind)
        .map_err(|e: PublisherParamError| ApiError::BadRequest(e.to_string()))?;
    state
        .clickhouse
        .query(&publishers_sql())
        .bind(days)
        .bind(kind)
        .bind(limit)
        .fetch_all()
        .await
        .map_err(missing_rollup_is_unavailable)
}

#[derive(Debug, Deserialize, Row)]
struct LongformTotals {
    articles_count: u64,
    unique_authors: u64,
    total_content_length: u64,
}

/// Long-form totals from `longform_latest`: one article per (pubkey, d_tag),
/// measured at its latest version. With `days`, only articles whose latest
/// version was created in the last `days` days count.
pub(super) async fn fetch_clickhouse_longform(
    state: &AppState,
    days: Option<u32>,
) -> Result<LongformStats, ApiError> {
    let query = state.clickhouse.query(&longform_sql(days.is_some()));
    let query = match days {
        Some(days) => query.bind(days),
        None => query,
    };
    let totals: LongformTotals = query
        .fetch_one()
        .await
        .map_err(missing_rollup_is_unavailable)?;
    Ok(longform_stats(
        totals.articles_count,
        totals.unique_authors,
        totals.total_content_length,
    ))
}

/// Response for the given totals; an empty set averages to 0.
fn longform_stats(
    articles_count: u64,
    unique_authors: u64,
    total_content_length: u64,
) -> LongformStats {
    LongformStats {
        articles_count,
        unique_authors,
        avg_content_length: if articles_count == 0 {
            0.0
        } else {
            total_content_length as f64 / articles_count as f64
        },
        total_content_length,
        estimated_total_words: total_content_length / 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tables_become_metric_unavailable() {
        let missing = ApiError::Database(clickhouse::error::Error::BadResponse(
            "Code: 60. DB::Exception: Unknown table expression identifier 'longform_latest' \
             in scope SELECT 1. (UNKNOWN_TABLE)"
                .into(),
        ));
        assert!(matches!(
            missing_rollup_is_unavailable(missing),
            ApiError::MetricUnavailable
        ));
        let other = ApiError::Database(clickhouse::error::Error::BadResponse(
            "Code: 159. DB::Exception: Timeout exceeded. (TIMEOUT_EXCEEDED)".into(),
        ));
        assert!(matches!(
            missing_rollup_is_unavailable(other),
            ApiError::Database(_)
        ));
        assert!(matches!(
            missing_rollup_is_unavailable(ApiError::Overloaded),
            ApiError::Overloaded
        ));
    }

    #[test]
    fn longform_stats_average_and_words() {
        let s = longform_stats(4, 3, 102);
        assert_eq!(s.articles_count, 4);
        assert_eq!(s.unique_authors, 3);
        assert_eq!(s.total_content_length, 102);
        assert_eq!(s.avg_content_length, 25.5);
        assert_eq!(s.estimated_total_words, 20);
        let empty = longform_stats(0, 0, 0);
        assert_eq!(empty.avg_content_length, 0.0);
        assert_eq!(empty.estimated_total_words, 0);
    }
}
