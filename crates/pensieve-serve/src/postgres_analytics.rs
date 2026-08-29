//! Fixed-memory readers for versioned Postgres sketch products.

use futures_util::{TryStreamExt, pin_mut};
use pensieve_core::DistinctSketchUnion;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

const SECONDS_PER_HOUR: i64 = 3_600;
const MAX_WINDOW_HOURS: i64 = 24 * 366;

/// Estimate distinct event authors from the exact current run's flexible leaves.
pub async fn estimate_current_flexible_distinct(
    client: &Client,
    since_epoch: i64,
    until_epoch: i64,
    kind: Option<u16>,
) -> anyhow::Result<u64> {
    validate_window(since_epoch, until_epoch, SECONDS_PER_HOUR, MAX_WINDOW_HOURS)?;
    let metadata = client
        .query_opt(
            "SELECT products.product_id,products.complete_through_epoch
               FROM pensieve_analytics.current_run_metadata current
               JOIN pensieve_analytics.flexible_distinct_products products
                 ON products.run_id=current.run_id
                AND products.evidence_sha256 =
                    current.validation ->> 'flexible_distinct_evidence_sha256'
                AND products.validation_evidence_sha256 =
                    current.validation ->> 'flexible_distinct_validation_sha256'",
            &[],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("current flexible-distinct product is unavailable"))?;
    let product_id: String = metadata.get(0);
    let complete_through: i64 = metadata.get(1);
    if until_epoch > complete_through {
        anyhow::bail!("flexible-distinct window exceeds its complete-hour boundary");
    }
    let kind = kind.map(i32::from);
    let mut union = DistinctSketchUnion::new();
    let params: [&(dyn ToSql + Sync); 4] = [&product_id, &since_epoch, &until_epoch, &kind];
    let rows = client
        .query_raw(
            "SELECT sketch
               FROM pensieve_analytics.flexible_distinct_leaves
              WHERE product_id=$1 AND hour_epoch >= $2 AND hour_epoch < $3
                AND ($4::integer IS NULL OR kind=$4)
              ORDER BY hour_epoch,kind",
            params,
        )
        .await?;
    pin_mut!(rows);
    let mut leaf_count = 0_u64;
    while let Some(row) = rows.try_next().await? {
        let sketch: Vec<u8> = row.get(0);
        union
            .push_serialized(&sketch)
            .map_err(|error| anyhow::anyhow!("decode flexible-distinct leaf: {error}"))?;
        leaf_count = leaf_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("flexible-distinct leaf count overflow"))?;
    }
    metrics::histogram!("api_postgres_distinct_leaves_per_union").record(leaf_count as f64);
    Ok(union.finish().estimate())
}

fn validate_window(
    since_epoch: i64,
    until_epoch: i64,
    alignment: i64,
    maximum_units: i64,
) -> anyhow::Result<()> {
    if since_epoch < 0
        || since_epoch >= until_epoch
        || since_epoch % alignment != 0
        || until_epoch % alignment != 0
        || (until_epoch - since_epoch) / alignment > maximum_units
    {
        anyhow::bail!("Postgres distinct window is invalid or unbounded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SECONDS_PER_HOUR, validate_window};

    #[test]
    fn distinct_windows_are_positive_aligned_and_bounded() {
        assert!(validate_window(0, SECONDS_PER_HOUR, SECONDS_PER_HOUR, 1).is_ok());
        assert!(validate_window(1, SECONDS_PER_HOUR, SECONDS_PER_HOUR, 1).is_err());
        assert!(validate_window(0, 0, SECONDS_PER_HOUR, 1).is_err());
        assert!(validate_window(0, 2 * SECONDS_PER_HOUR, SECONDS_PER_HOUR, 1).is_err());
    }
}
