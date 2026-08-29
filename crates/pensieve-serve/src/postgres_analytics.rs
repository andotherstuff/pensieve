//! Fixed-memory readers for versioned Postgres sketch products.

use futures_util::{TryStreamExt, pin_mut};
use pensieve_core::{DistinctSketchUnion, ZAP_DISTINCT_SKETCH_LG_K};
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

const SECONDS_PER_HOUR: i64 = 3_600;
const MAX_WINDOW_HOURS: i64 = 24 * 366;
const SECONDS_PER_DAY: i64 = 86_400;
const MAX_WINDOW_DAYS: i64 = 366;

/// Immutable semantic and distinct-product identities accepted by one current run.
pub struct CurrentZapProducts {
    /// Accepted exact semantic product.
    pub semantic_product_id: String,
    distinct_product_id: String,
    /// Last complete UTC-day boundary shared by both products.
    pub complete_through_epoch: i64,
}

/// Immutable long-form semantic and flexible-distinct identities from one run.
pub struct CurrentLongformProducts {
    /// Accepted exact semantic product.
    pub semantic_product_id: String,
    flexible_product_id: String,
    /// Last complete UTC-day boundary used by the public bounded window.
    pub complete_through_epoch: i64,
    flexible_complete_through_epoch: i64,
}

/// Immutable exact serving-facts identity accepted by one current run.
pub struct CurrentServingProduct {
    /// Atomically selected analytics run.
    pub run_id: String,
    /// Accepted serving-facts product.
    pub product_id: String,
    /// Last complete UTC-hour boundary.
    pub complete_through_epoch: i64,
}

/// Exact count and accepted distinct products tied to one current run.
pub struct CurrentEventProducts {
    /// Accepted exact serving-facts product.
    pub serving_product_id: String,
    flexible_product_id: String,
    /// Shared last complete UTC-hour boundary.
    pub complete_through_epoch: i64,
}

/// Accepted exact publisher-ranking product tied to the current run.
pub struct CurrentPublisherProduct {
    /// Immutable ranking product identifier.
    pub product_id: String,
}

/// Supported calendar grouping for bounded zap-distinct unions.
#[derive(Clone, Copy)]
pub enum ZapPeriodGrain {
    /// One UTC day.
    Day,
    /// ISO week beginning Monday UTC.
    Week,
    /// Calendar month UTC.
    Month,
}

/// Supported presentation grouping for event-author sketch unions.
#[derive(Clone, Copy)]
pub enum EventDistinctGrain {
    /// UTC day.
    Day,
    /// ISO week beginning Monday UTC.
    Week,
    /// Calendar month UTC.
    Month,
    /// UTC hour-of-day in the range 0..=23.
    HourOfDay,
}

/// Distinct event authors for one presentation period.
pub struct EventPeriodDistinct {
    /// Epoch period start, or hour-of-day for `HourOfDay`.
    pub period_key: i64,
    /// Estimated unique event authors.
    pub unique_pubkeys: u64,
}

/// Distinct zap participants for one grouped UTC period.
pub struct ZapPeriodDistinct {
    /// UTC epoch at the beginning of the period.
    pub period_epoch: i64,
    /// Estimated unique validated senders.
    pub unique_senders: u64,
    /// Estimated unique validated recipients.
    pub unique_recipients: u64,
}

/// Resolve one current run's exact semantic and zap-distinct product pair.
pub async fn current_zap_products(client: &Client) -> anyhow::Result<CurrentZapProducts> {
    let row = client
        .query_opt(
            "SELECT semantic.product_id,distincts.product_id,
                    semantic.as_of_epoch,distincts.complete_through_epoch
               FROM pensieve_analytics.current_run_metadata current
               JOIN pensieve_analytics.semantic_products semantic
                 ON semantic.run_id=current.run_id
                AND semantic.evidence_sha256 =
                    current.validation ->> 'semantic_evidence_sha256'
               JOIN pensieve_analytics.semantic_zap_distinct_products distincts
                 ON distincts.semantic_product_id=semantic.product_id
                AND distincts.evidence_sha256 =
                    current.validation ->> 'zap_distinct_evidence_sha256'",
            &[],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("current semantic zap product is unavailable"))?;
    let as_of_epoch: i64 = row.get(2);
    let complete_through_epoch: i64 = row.get(3);
    if as_of_epoch < 0 || complete_through_epoch != as_of_epoch - as_of_epoch % SECONDS_PER_DAY {
        anyhow::bail!("current semantic zap products have inconsistent boundaries");
    }
    Ok(CurrentZapProducts {
        semantic_product_id: row.get(0),
        distinct_product_id: row.get(1),
        complete_through_epoch,
    })
}

/// Resolve one current run's semantic and flexible products for long-form stats.
pub async fn current_longform_products(client: &Client) -> anyhow::Result<CurrentLongformProducts> {
    let row = client
        .query_opt(
            "SELECT semantic.product_id,flexible.product_id,
                    semantic.as_of_epoch,flexible.complete_through_epoch
               FROM pensieve_analytics.current_run_metadata current
               JOIN pensieve_analytics.semantic_products semantic
                 ON semantic.run_id=current.run_id
                AND semantic.evidence_sha256 =
                    current.validation ->> 'semantic_evidence_sha256'
               JOIN pensieve_analytics.flexible_distinct_products flexible
                 ON flexible.run_id=current.run_id
                AND flexible.evidence_sha256 =
                    current.validation ->> 'flexible_distinct_evidence_sha256'
                AND flexible.validation_evidence_sha256 =
                    current.validation ->> 'flexible_distinct_validation_sha256'",
            &[],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("current longform products are unavailable"))?;
    let as_of_epoch: i64 = row.get(2);
    let flexible_complete_through_epoch: i64 = row.get(3);
    if as_of_epoch < 0
        || flexible_complete_through_epoch < 0
        || flexible_complete_through_epoch > as_of_epoch
        || flexible_complete_through_epoch % SECONDS_PER_HOUR != 0
    {
        anyhow::bail!("current longform products have inconsistent boundaries");
    }
    Ok(CurrentLongformProducts {
        semantic_product_id: row.get(0),
        flexible_product_id: row.get(1),
        complete_through_epoch: as_of_epoch - as_of_epoch % SECONDS_PER_DAY,
        flexible_complete_through_epoch,
    })
}

/// Resolve the exact serving-facts product accepted by the current run.
pub async fn current_serving_product(client: &Client) -> anyhow::Result<CurrentServingProduct> {
    let row = client
        .query_opt(
            "SELECT current.run_id,serving.product_id,
                    current.as_of_epoch,serving.complete_through_epoch
               FROM pensieve_analytics.current_run_metadata current
               JOIN pensieve_analytics.serving_fact_products serving
                 ON serving.run_id=current.run_id
                AND serving.evidence_sha256 =
                    current.validation ->> 'serving_facts_evidence_sha256'",
            &[],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("current serving-facts product is unavailable"))?;
    let as_of_epoch: i64 = row.get(2);
    let complete_through_epoch: i64 = row.get(3);
    if as_of_epoch < 0 || complete_through_epoch != as_of_epoch - as_of_epoch % SECONDS_PER_HOUR {
        anyhow::bail!("current serving-facts product has an inconsistent boundary");
    }
    Ok(CurrentServingProduct {
        run_id: row.get(0),
        product_id: row.get(1),
        complete_through_epoch,
    })
}

/// Resolve exact counts and accepted sketches from the same current run.
pub async fn current_event_products(client: &Client) -> anyhow::Result<CurrentEventProducts> {
    let row = client
        .query_opt(
            "SELECT serving.product_id,flexible.product_id,
                    current.as_of_epoch,serving.complete_through_epoch,
                    flexible.complete_through_epoch
               FROM pensieve_analytics.current_run_metadata current
               JOIN pensieve_analytics.serving_fact_products serving
                 ON serving.run_id=current.run_id
                AND serving.evidence_sha256 =
                    current.validation ->> 'serving_facts_evidence_sha256'
               JOIN pensieve_analytics.flexible_distinct_products flexible
                 ON flexible.run_id=current.run_id
                AND flexible.evidence_sha256 =
                    current.validation ->> 'flexible_distinct_evidence_sha256'
                AND flexible.validation_evidence_sha256 =
                    current.validation ->> 'flexible_distinct_validation_sha256'",
            &[],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("current event serving products are unavailable"))?;
    let as_of_epoch: i64 = row.get(2);
    let serving_complete: i64 = row.get(3);
    let flexible_complete: i64 = row.get(4);
    if as_of_epoch < 0
        || serving_complete != flexible_complete
        || serving_complete != as_of_epoch - as_of_epoch % SECONDS_PER_HOUR
    {
        anyhow::bail!("current event serving products have inconsistent boundaries");
    }
    Ok(CurrentEventProducts {
        serving_product_id: row.get(0),
        flexible_product_id: row.get(1),
        complete_through_epoch: serving_complete,
    })
}

/// Resolve the accepted exact predefined-window publisher ranking.
pub async fn current_publisher_product(client: &Client) -> anyhow::Result<CurrentPublisherProduct> {
    let row = client
        .query_opt(
            "SELECT products.product_id,products.windows_days,products.top_limit
               FROM pensieve_analytics.current_run_metadata current
               JOIN pensieve_analytics.publisher_ranking_products products
                 ON products.run_id=current.run_id
                AND products.evidence_sha256 =
                    current.validation ->> 'publisher_ranking_evidence_sha256'
              WHERE products.product_version='publisher-ranking-v1'",
            &[],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("current publisher-ranking product is unavailable"))?;
    let windows: Vec<i32> = row.get(1);
    let top_limit: i32 = row.get(2);
    if windows != [1, 7, 30, 90, 365] || top_limit != 1_000 {
        anyhow::bail!("current publisher-ranking product has an unsupported contract");
    }
    Ok(CurrentPublisherProduct {
        product_id: row.get(0),
    })
}

/// Estimate distinct authors over one bounded complete-hour event window.
pub async fn estimate_event_distinct(
    client: &Client,
    products: &CurrentEventProducts,
    since_epoch: i64,
    until_epoch: i64,
    kind: Option<u16>,
) -> anyhow::Result<u64> {
    estimate_flexible_distinct_product(
        client,
        &products.flexible_product_id,
        products.complete_through_epoch,
        since_epoch,
        until_epoch,
        kind,
    )
    .await
}

/// Estimate event authors for each requested presentation period.
pub async fn estimate_event_distinct_periods(
    client: &Client,
    products: &CurrentEventProducts,
    since_epoch: i64,
    until_epoch: i64,
    kind: Option<u16>,
    grain: EventDistinctGrain,
) -> anyhow::Result<Vec<EventPeriodDistinct>> {
    validate_window(since_epoch, until_epoch, SECONDS_PER_HOUR, MAX_WINDOW_HOURS)?;
    if until_epoch > products.complete_through_epoch {
        anyhow::bail!("event distinct window exceeds its complete-hour boundary");
    }
    let expression = event_period_expression(grain);
    let query = format!(
        "SELECT {expression} AS period_key,sketch
           FROM pensieve_analytics.flexible_distinct_leaves
          WHERE product_id=$1 AND hour_epoch >= $2 AND hour_epoch < $3
            AND ($4::integer IS NULL OR kind=$4)
          ORDER BY period_key,hour_epoch,kind"
    );
    let kind = kind.map(i32::from);
    let params: [&(dyn ToSql + Sync); 4] = [
        &products.flexible_product_id,
        &since_epoch,
        &until_epoch,
        &kind,
    ];
    let rows = client.query_raw(&query, params).await?;
    pin_mut!(rows);
    let mut output = Vec::new();
    let mut current_period = None;
    let mut union = DistinctSketchUnion::new();
    let mut leaf_count = 0_u64;
    while let Some(row) = rows.try_next().await? {
        let period_key: i64 = row.get(0);
        if current_period.is_some_and(|current| current != period_key) {
            output.push(EventPeriodDistinct {
                period_key: current_period.expect("period exists"),
                unique_pubkeys: union.finish().estimate(),
            });
            union = DistinctSketchUnion::new();
        }
        current_period = Some(period_key);
        let sketch: Vec<u8> = row.get(1);
        union
            .push_serialized(&sketch)
            .map_err(|error| anyhow::anyhow!("decode event distinct leaf: {error}"))?;
        leaf_count = leaf_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("event distinct leaf count overflow"))?;
    }
    if let Some(period_key) = current_period {
        output.push(EventPeriodDistinct {
            period_key,
            unique_pubkeys: union.finish().estimate(),
        });
    }
    if output.len() > usize::try_from(MAX_WINDOW_HOURS).expect("hour bound fits usize") {
        anyhow::bail!("event distinct period result exceeds its fixed bound");
    }
    metrics::histogram!("api_postgres_distinct_leaves_per_union").record(leaf_count as f64);
    Ok(output)
}

/// Estimate distinct senders and recipients over one bounded UTC-day window.
pub async fn estimate_zap_distinct_roles(
    client: &Client,
    products: &CurrentZapProducts,
    since_epoch: i64,
    until_epoch: i64,
) -> anyhow::Result<(u64, u64)> {
    validate_zap_window(products, since_epoch, until_epoch)?;
    let mut senders = zap_union()?;
    let mut recipients = zap_union()?;
    let params: [&(dyn ToSql + Sync); 3] =
        [&products.distinct_product_id, &since_epoch, &until_epoch];
    let rows = client
        .query_raw(
            "SELECT role,sketch
               FROM pensieve_analytics.semantic_zap_distinct_leaves
              WHERE product_id=$1 AND day_epoch >= $2 AND day_epoch < $3
              ORDER BY day_epoch,role",
            params,
        )
        .await?;
    pin_mut!(rows);
    let mut leaf_count = 0_u64;
    while let Some(row) = rows.try_next().await? {
        let role: i16 = row.get(0);
        let sketch: Vec<u8> = row.get(1);
        let union = match role {
            0 => &mut senders,
            1 => &mut recipients,
            _ => anyhow::bail!("published zap distinct role is invalid"),
        };
        union
            .push_serialized(&sketch)
            .map_err(|error| anyhow::anyhow!("decode zap distinct leaf: {error}"))?;
        leaf_count = leaf_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("zap distinct leaf count overflow"))?;
    }
    metrics::histogram!("api_postgres_distinct_leaves_per_union").record(leaf_count as f64);
    Ok((senders.finish().estimate(), recipients.finish().estimate()))
}

/// Estimate distinct senders and recipients for each bounded calendar period.
pub async fn estimate_zap_distinct_periods(
    client: &Client,
    products: &CurrentZapProducts,
    since_epoch: i64,
    until_epoch: i64,
    grain: ZapPeriodGrain,
) -> anyhow::Result<Vec<ZapPeriodDistinct>> {
    validate_zap_window(products, since_epoch, until_epoch)?;
    let period_expression = zap_period_expression(grain);
    let query = format!(
        "SELECT {period_expression} AS period_epoch,role,sketch
           FROM pensieve_analytics.semantic_zap_distinct_leaves
          WHERE product_id=$1 AND day_epoch >= $2 AND day_epoch < $3
          ORDER BY period_epoch DESC,day_epoch,role"
    );
    let params: [&(dyn ToSql + Sync); 3] =
        [&products.distinct_product_id, &since_epoch, &until_epoch];
    let rows = client.query_raw(&query, params).await?;
    pin_mut!(rows);
    let mut output = Vec::new();
    let mut current_period = None;
    let mut senders = zap_union()?;
    let mut recipients = zap_union()?;
    let mut leaf_count = 0_u64;
    while let Some(row) = rows.try_next().await? {
        let period_epoch: i64 = row.get(0);
        if current_period.is_some_and(|current| current != period_epoch) {
            output.push(finish_zap_period(
                current_period.expect("period exists"),
                senders,
                recipients,
            ));
            senders = zap_union()?;
            recipients = zap_union()?;
        }
        current_period = Some(period_epoch);
        let role: i16 = row.get(1);
        let sketch: Vec<u8> = row.get(2);
        let union = match role {
            0 => &mut senders,
            1 => &mut recipients,
            _ => anyhow::bail!("published zap distinct role is invalid"),
        };
        union
            .push_serialized(&sketch)
            .map_err(|error| anyhow::anyhow!("decode zap distinct leaf: {error}"))?;
        leaf_count = leaf_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("zap distinct leaf count overflow"))?;
    }
    if let Some(period_epoch) = current_period {
        output.push(finish_zap_period(period_epoch, senders, recipients));
    }
    if output.len() > usize::try_from(MAX_WINDOW_DAYS).expect("day bound fits usize") {
        anyhow::bail!("zap distinct period result exceeds its fixed bound");
    }
    metrics::histogram!("api_postgres_distinct_leaves_per_union").record(leaf_count as f64);
    Ok(output)
}

/// Estimate distinct event authors from one already-resolved flexible product.
pub async fn estimate_flexible_distinct(
    client: &Client,
    products: &CurrentLongformProducts,
    since_epoch: i64,
    until_epoch: i64,
    kind: Option<u16>,
) -> anyhow::Result<u64> {
    estimate_flexible_distinct_product(
        client,
        &products.flexible_product_id,
        products.flexible_complete_through_epoch,
        since_epoch,
        until_epoch,
        kind,
    )
    .await
}

async fn estimate_flexible_distinct_product(
    client: &Client,
    product_id: &str,
    complete_through_epoch: i64,
    since_epoch: i64,
    until_epoch: i64,
    kind: Option<u16>,
) -> anyhow::Result<u64> {
    validate_window(since_epoch, until_epoch, SECONDS_PER_HOUR, MAX_WINDOW_HOURS)?;
    if until_epoch > complete_through_epoch {
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

fn validate_zap_window(
    products: &CurrentZapProducts,
    since_epoch: i64,
    until_epoch: i64,
) -> anyhow::Result<()> {
    validate_window(since_epoch, until_epoch, SECONDS_PER_DAY, MAX_WINDOW_DAYS)?;
    if until_epoch > products.complete_through_epoch {
        anyhow::bail!("zap distinct window exceeds its complete-day boundary");
    }
    Ok(())
}

fn zap_union() -> anyhow::Result<DistinctSketchUnion> {
    DistinctSketchUnion::with_lg_k(ZAP_DISTINCT_SKETCH_LG_K)
        .map_err(|error| anyhow::anyhow!("create zap distinct union: {error}"))
}

fn finish_zap_period(
    period_epoch: i64,
    senders: DistinctSketchUnion,
    recipients: DistinctSketchUnion,
) -> ZapPeriodDistinct {
    ZapPeriodDistinct {
        period_epoch,
        unique_senders: senders.finish().estimate(),
        unique_recipients: recipients.finish().estimate(),
    }
}

/// Return a static SQL expression over the trusted `day_epoch` column.
pub fn zap_period_expression(grain: ZapPeriodGrain) -> &'static str {
    match grain {
        ZapPeriodGrain::Day => "day_epoch",
        ZapPeriodGrain::Week => "day_epoch - ((day_epoch / 86400 + 3) % 7) * 86400",
        ZapPeriodGrain::Month => {
            "EXTRACT(EPOCH FROM date_trunc('month', to_timestamp(day_epoch) AT TIME ZONE 'UTC'))::bigint"
        }
    }
}

/// Return a static SQL expression over a trusted `hour_epoch` column.
pub fn event_period_expression(grain: EventDistinctGrain) -> &'static str {
    match grain {
        EventDistinctGrain::Day => "hour_epoch - hour_epoch % 86400",
        EventDistinctGrain::Week => {
            "hour_epoch - hour_epoch % 86400 - (((hour_epoch / 86400) + 3) % 7) * 86400"
        }
        EventDistinctGrain::Month => {
            "EXTRACT(EPOCH FROM date_trunc('month', to_timestamp(hour_epoch) AT TIME ZONE 'UTC'))::bigint"
        }
        EventDistinctGrain::HourOfDay => "(hour_epoch / 3600) % 24",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EventDistinctGrain, SECONDS_PER_HOUR, ZapPeriodGrain, event_period_expression,
        validate_window, zap_period_expression,
    };

    #[test]
    fn distinct_windows_are_positive_aligned_and_bounded() {
        assert!(validate_window(0, SECONDS_PER_HOUR, SECONDS_PER_HOUR, 1).is_ok());
        assert!(validate_window(1, SECONDS_PER_HOUR, SECONDS_PER_HOUR, 1).is_err());
        assert!(validate_window(0, 0, SECONDS_PER_HOUR, 1).is_err());
        assert!(validate_window(0, 2 * SECONDS_PER_HOUR, SECONDS_PER_HOUR, 1).is_err());
    }

    #[test]
    fn zap_period_expressions_are_static_and_utc_anchored() {
        assert_eq!(zap_period_expression(ZapPeriodGrain::Day), "day_epoch");
        assert!(zap_period_expression(ZapPeriodGrain::Week).contains("+ 3"));
        assert!(zap_period_expression(ZapPeriodGrain::Month).contains("UTC"));
    }

    #[test]
    fn event_period_expressions_cover_calendar_and_hour_of_day_grains() {
        assert!(event_period_expression(EventDistinctGrain::Day).contains("86400"));
        assert!(event_period_expression(EventDistinctGrain::Week).contains("+ 3"));
        assert!(event_period_expression(EventDistinctGrain::Month).contains("UTC"));
        assert!(event_period_expression(EventDistinctGrain::HourOfDay).contains("% 24"));
    }
}
