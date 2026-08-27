//! Dormant atomic Postgres publication for Slice 8 relay distribution.

use std::io::Write;

use postgres::{Client, GenericClient};
use sha2::{Digest, Sha256};

use crate::schema::SCHEMA_SQL;
use crate::{
    BoundedRelayDistribution, COHORT_RETENTION_QUERY_VERSION, Error, RELAY_DISTRIBUTION_VERSION,
    RelayDistributionRow, Result,
};

const PUBLICATION_LOCK_ID: i64 = 0x5045_4e53_4945_5645;

/// Result of atomically publishing one dormant Slice 8 product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayDistributionPublishOutcome {
    /// A new versioned product and all rows committed.
    Published { product_id: String },
    /// An identical committed product reconciled successfully.
    AlreadyPublished { product_id: String },
}

/// Publish exact current relay distribution without moving analytics pointers.
pub fn publish_relay_distribution(
    client: &mut Client,
    baseline_run_id: &str,
    product: &BoundedRelayDistribution,
) -> Result<RelayDistributionPublishOutcome> {
    validate_product(product)?;
    client.batch_execute(SCHEMA_SQL)?;
    let product_id = relay_product_id(baseline_run_id, product);
    let mut transaction = client.transaction()?;
    transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&PUBLICATION_LOCK_ID])?;
    let current = transaction.query_one(
        "SELECT run_id,snapshot_id,query_version,as_of_epoch
           FROM pensieve_analytics.current_run_metadata FOR SHARE",
        &[],
    )?;
    if current.get::<_, String>(0) != baseline_run_id
        || current.get::<_, String>(1) != product.evidence.snapshot_id
        || current.get::<_, String>(2) != COHORT_RETENTION_QUERY_VERSION
        || from_i64("relay current as-of", current.get(3))? != product.evidence.as_of_epoch
    {
        return Err(Error::Validation(
            "current Postgres run is not the exact corrected B3 Slice 8 baseline".to_owned(),
        ));
    }
    if transaction
        .query_opt(
            "SELECT product_id FROM pensieve_analytics.relay_distribution_products
              WHERE product_id=$1",
            &[&product_id],
        )?
        .is_some()
    {
        reconcile(&mut transaction, &product_id, product)?;
        transaction.commit()?;
        return Ok(RelayDistributionPublishOutcome::AlreadyPublished { product_id });
    }
    transaction.execute(
        "INSERT INTO pensieve_analytics.relay_distribution_products (
             product_id,run_id,snapshot_id,as_of_epoch,product_version,
             evidence_sha256,rows_sha256,candidate_events,winning_pubkeys,
             minimum_users,relay_rows,published_at
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,now())",
        &[
            &product_id,
            &baseline_run_id,
            &product.evidence.snapshot_id,
            &to_i64("relay as-of", product.evidence.as_of_epoch)?,
            &RELAY_DISTRIBUTION_VERSION,
            &product.evidence_sha256,
            &product.evidence.rows_sha256,
            &to_i64("relay candidate events", product.evidence.candidate_events)?,
            &to_i64("relay winning pubkeys", product.evidence.winning_pubkeys)?,
            &to_i64("relay minimum users", product.evidence.minimum_users)?,
            &to_i64("relay row count", product.evidence.rows.len() as u64)?,
        ],
    )?;
    copy_rows(&mut transaction, &product_id, product)?;
    reconcile(&mut transaction, &product_id, product)?;
    transaction.commit()?;
    Ok(RelayDistributionPublishOutcome::Published { product_id })
}

/// Load one deterministic serving page from a versioned dormant product.
pub fn query_published_relay_distribution(
    client: &mut impl GenericClient,
    product_id: &str,
    limit: u64,
) -> Result<Vec<RelayDistributionRow>> {
    if limit == 0 || limit > 1_000 {
        return Err(Error::Validation(
            "relay distribution limit must be between 1 and 1000".to_owned(),
        ));
    }
    let rows = client.query(
        "SELECT relay_url,user_count,read_count,write_count
           FROM pensieve_analytics.relay_distribution_rows
          WHERE product_id=$1
          ORDER BY user_count DESC,relay_url ASC LIMIT $2",
        &[&product_id, &to_i64("relay limit", limit)?],
    )?;
    rows.into_iter()
        .map(|row| {
            Ok(RelayDistributionRow {
                relay_url: row.get(0),
                user_count: from_i64("relay users", row.get(1))?,
                read_count: from_i64("relay reads", row.get(2))?,
                write_count: from_i64("relay writes", row.get(3))?,
            })
        })
        .collect()
}

fn copy_rows(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedRelayDistribution,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "COPY pensieve_analytics.relay_distribution_rows
         (product_id,relay_url,user_count,read_count,write_count)
         FROM STDIN WITH (FORMAT csv)",
    )?;
    for row in &product.evidence.rows {
        writeln!(
            writer,
            "{product_id},{},{},{},{}",
            csv_field(&row.relay_url),
            row.user_count,
            row.read_count,
            row.write_count,
        )?;
    }
    let inserted = writer.finish()?;
    if inserted != product.evidence.rows.len() as u64 {
        return Err(Error::Validation(format!(
            "Postgres copied {inserted} relay rows, expected {}",
            product.evidence.rows.len()
        )));
    }
    Ok(())
}

fn reconcile(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedRelayDistribution,
) -> Result<()> {
    let metadata = transaction.query_one(
        "SELECT snapshot_id,as_of_epoch,product_version,evidence_sha256,
                rows_sha256,candidate_events,winning_pubkeys,minimum_users,relay_rows
           FROM pensieve_analytics.relay_distribution_products WHERE product_id=$1",
        &[&product_id],
    )?;
    if metadata.get::<_, String>(0) != product.evidence.snapshot_id
        || from_i64("published relay as-of", metadata.get(1))? != product.evidence.as_of_epoch
        || metadata.get::<_, String>(2) != RELAY_DISTRIBUTION_VERSION
        || metadata.get::<_, String>(3) != product.evidence_sha256
        || metadata.get::<_, String>(4) != product.evidence.rows_sha256
        || from_i64("published relay candidates", metadata.get(5))?
            != product.evidence.candidate_events
        || from_i64("published relay winners", metadata.get(6))? != product.evidence.winning_pubkeys
        || from_i64("published relay minimum", metadata.get(7))? != product.evidence.minimum_users
        || from_i64("published relay row metadata", metadata.get(8))?
            != product.evidence.rows.len() as u64
    {
        return Err(Error::Validation(
            "published relay product metadata does not reconcile".to_owned(),
        ));
    }
    let rows = transaction.query(
        "SELECT relay_url,user_count,read_count,write_count
           FROM pensieve_analytics.relay_distribution_rows
          WHERE product_id=$1 ORDER BY user_count DESC,relay_url ASC",
        &[&product_id],
    )?;
    let published = rows
        .into_iter()
        .map(|row| {
            Ok(RelayDistributionRow {
                relay_url: row.get(0),
                user_count: from_i64("published relay users", row.get(1))?,
                read_count: from_i64("published relay reads", row.get(2))?,
                write_count: from_i64("published relay writes", row.get(3))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if published != product.evidence.rows {
        return Err(Error::Validation(
            "published relay rows differ from canonical evidence".to_owned(),
        ));
    }
    Ok(())
}

fn validate_product(product: &BoundedRelayDistribution) -> Result<()> {
    let rows_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&product.evidence.rows)
            .map_err(|error| Error::Validation(format!("encode relay rows: {error}")))?,
    ));
    if product.evidence.status != "completed"
        || product.evidence.product_version != RELAY_DISTRIBUTION_VERSION
        || product.evidence.rows_sha256 != rows_sha256
        || product.evidence.rows.iter().any(|row| {
            row.user_count < product.evidence.minimum_users
                || row.read_count > row.user_count
                || row.write_count > row.user_count
        })
    {
        return Err(Error::Validation(
            "relay distribution failed immutable publication validation".to_owned(),
        ));
    }
    Ok(())
}

fn relay_product_id(baseline_run_id: &str, product: &BoundedRelayDistribution) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-relay-distribution-product-v1\0");
    digest.update(baseline_run_id.as_bytes());
    digest.update([0]);
    digest.update(product.evidence_sha256.as_bytes());
    hex::encode(digest.finalize())
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::NegativeLedgerValue { field, value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::RelayDistributionEvidence;

    #[test]
    fn csv_quotes_urls() {
        assert_eq!(
            csv_field("wss://relay.example/a,b"),
            "\"wss://relay.example/a,b\""
        );
    }

    #[test]
    fn postgres_publication_rolls_back_retries_and_never_moves_current() {
        let Ok(url) = std::env::var("PENSIEVE_TEST_POSTGRES_URL") else {
            return;
        };
        let mut client = postgres::Config::from_str(&url)
            .expect("parse test Postgres URL")
            .connect(postgres::NoTls)
            .expect("connect test Postgres");
        client
            .batch_execute("DROP SCHEMA IF EXISTS pensieve_analytics CASCADE")
            .expect("reset isolated database");
        client.batch_execute(SCHEMA_SQL).expect("apply schema");
        let product = synthetic_product();
        let baseline = "relay-b3-baseline";
        client
            .execute(
                "INSERT INTO pensieve_analytics.runs (
                     run_id,snapshot_id,previous_run_id,run_kind,query_version,
                     code_version,as_of_epoch,started_at,completed_at,published_at,
                     physical_rows,logical_events,duplicate_rows,api_representable_events,
                     event_daily_rows,event_daily_kind_rows,kind_all_time_rows,validation
                 ) VALUES ($1,$2,NULL,'incremental',$3,'test',$4,
                           now(),now(),now(),0,0,0,0,0,0,0,'{}'::jsonb)",
                &[
                    &baseline,
                    &product.evidence.snapshot_id,
                    &COHORT_RETENTION_QUERY_VERSION,
                    &(product.evidence.as_of_epoch as i64),
                ],
            )
            .expect("insert B3 baseline");
        client
            .execute(
                "INSERT INTO pensieve_analytics.current_run(singleton,run_id)
                 VALUES(true,$1)",
                &[&baseline],
            )
            .expect("set current B3 baseline");
        client
            .batch_execute(
                "CREATE FUNCTION pensieve_analytics.reject_relay_row()
                   RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN RAISE EXCEPTION 'injected relay failure'; END $$;
                 CREATE TRIGGER reject_relay_row
                   BEFORE INSERT ON pensieve_analytics.relay_distribution_rows
                   FOR EACH ROW EXECUTE FUNCTION pensieve_analytics.reject_relay_row();",
            )
            .expect("install failure injection");
        assert!(publish_relay_distribution(&mut client, baseline, &product).is_err());
        let rolled_back: i64 = client
            .query_one(
                "SELECT count(*)::BIGINT
                   FROM pensieve_analytics.relay_distribution_products",
                &[],
            )
            .expect("count rolled back products")
            .get(0);
        assert_eq!(rolled_back, 0);
        client
            .batch_execute(
                "DROP TRIGGER reject_relay_row
                   ON pensieve_analytics.relay_distribution_rows;
                 DROP FUNCTION pensieve_analytics.reject_relay_row();",
            )
            .expect("remove failure injection");
        let published = publish_relay_distribution(&mut client, baseline, &product)
            .expect("publish relay product");
        let product_id = match published {
            RelayDistributionPublishOutcome::Published { product_id } => product_id,
            outcome => panic!("unexpected publication: {outcome:?}"),
        };
        assert_eq!(
            query_published_relay_distribution(&mut client, &product_id, 1)
                .expect("query published relay rows"),
            product.evidence.rows[..1]
        );
        assert_eq!(
            publish_relay_distribution(&mut client, baseline, &product)
                .expect("retry relay product"),
            RelayDistributionPublishOutcome::AlreadyPublished {
                product_id: product_id.clone()
            }
        );
        let current: String = client
            .query_one(
                "SELECT run_id FROM pensieve_analytics.current_run WHERE singleton",
                &[],
            )
            .expect("read current pointer")
            .get(0);
        assert_eq!(current, baseline);
    }

    fn synthetic_product() -> BoundedRelayDistribution {
        let rows = vec![
            RelayDistributionRow {
                relay_url: "wss://first.example".to_owned(),
                user_count: 20,
                read_count: 15,
                write_count: 10,
            },
            RelayDistributionRow {
                relay_url: "wss://second.example".to_owned(),
                user_count: 10,
                read_count: 10,
                write_count: 10,
            },
        ];
        let rows_sha256 = hex::encode(Sha256::digest(
            serde_json::to_vec(&rows).expect("encode rows"),
        ));
        BoundedRelayDistribution {
            evidence: RelayDistributionEvidence {
                schema_version: 1,
                runner_version: "pensieve-analytics-relay-distribution-v1".to_owned(),
                status: "completed".to_owned(),
                product_version: RELAY_DISTRIBUTION_VERSION.to_owned(),
                snapshot_id: "sha256:relay-test".to_owned(),
                as_of_epoch: 1_700_000_000,
                object_count: 0,
                applied_objects: 0,
                physical_rows_scanned: 0,
                physical_relay_events: 0,
                candidate_events: 30,
                eligible_candidate_events: 30,
                winning_pubkeys: 25,
                candidate_memberships: 30,
                raw_relay_tags: 30,
                invalid_relay_tags: 0,
                duplicate_relay_tags: 0,
                minimum_users: 10,
                rows,
                rows_sha256,
                inputs: Vec::new(),
                max_state_bytes: 1,
                sqlite_cache_bytes: 1,
                disk_reserve_bytes: 0,
            },
            evidence_sha256: "a".repeat(64),
        }
    }
}
