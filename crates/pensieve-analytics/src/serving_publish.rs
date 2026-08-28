//! Dormant atomic Postgres publication for Slice 9.5 serving facts.

use std::io::Write;

use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, GenericClient};
use sha2::{Digest, Sha256};

use crate::schema::SCHEMA_SQL;
use crate::{
    BoundedServingFacts, Error, Result, SERVING_FACTS_VERSION, ServingHourlyRow, ServingKindRow,
    visit_serving_hourly_rows, visit_serving_kind_rows,
};

const PUBLICATION_LOCK_ID: i64 = 0x5045_4e53_4945_5645;

/// Result of publishing one versioned serving-facts product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServingFactsPublishOutcome {
    /// A new immutable product committed.
    Published { product_id: String },
    /// The identical committed product reconciled successfully.
    AlreadyPublished { product_id: String },
}

/// Proof that all immutable artifacts were validated before transaction entry.
pub(crate) struct ValidatedServingFactsPublication;

/// Publish serving facts without changing the current analytics pointer.
pub fn publish_serving_facts(
    client: &mut Client,
    baseline_run_id: &str,
    product: &BoundedServingFacts,
) -> Result<ServingFactsPublishOutcome> {
    let validated = validate_serving_facts_publication(product)?;
    client.batch_execute(SCHEMA_SQL)?;
    let mut transaction = client.transaction()?;
    transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&PUBLICATION_LOCK_ID])?;
    let current = transaction.query_one(
        "SELECT run_id,snapshot_id,as_of_epoch,
                validation ->> 'fixed_activity_evidence_sha256'
           FROM pensieve_analytics.current_run_metadata FOR SHARE",
        &[],
    )?;
    if current.get::<_, String>(0) != baseline_run_id
        || current.get::<_, String>(1) != product.evidence.snapshot_id
        || from_i64("serving current as-of", current.get(2))? != product.evidence.as_of_epoch
        || current.get::<_, Option<String>>(3).as_deref()
            != Some(product.evidence.activity_evidence_sha256.as_str())
    {
        return Err(Error::Validation(
            "current Postgres run is not the exact Slice 9.5 baseline".to_owned(),
        ));
    }
    let outcome = publish_serving_facts_in_transaction(
        &mut transaction,
        baseline_run_id,
        product,
        &validated,
    )?;
    transaction.commit()?;
    Ok(outcome)
}

/// Publish one prevalidated serving product inside a generation transaction.
pub(crate) fn publish_serving_facts_in_transaction(
    transaction: &mut impl GenericClient,
    run_id: &str,
    product: &BoundedServingFacts,
    _validated: &ValidatedServingFactsPublication,
) -> Result<ServingFactsPublishOutcome> {
    let product_id = product_id(run_id, product);
    if transaction
        .query_opt(
            "SELECT product_id FROM pensieve_analytics.serving_fact_products
              WHERE product_id=$1",
            &[&product_id],
        )?
        .is_some()
    {
        reconcile(transaction, &product_id, product)?;
        return Ok(ServingFactsPublishOutcome::AlreadyPublished { product_id });
    }
    transaction.execute(
        "INSERT INTO pensieve_analytics.serving_fact_products (
             product_id,run_id,snapshot_id,as_of_epoch,complete_through_epoch,
             product_version,evidence_sha256,activity_evidence_sha256,
             enriched_artifact_sha256,hourly_artifact_sha256,kind_artifact_sha256,
             logical_events,hourly_rows,kind_rows,complete_hour_events,
             eligible_kind_events,eligible_content_bytes,published_at
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,now())",
        &[
            &product_id,
            &run_id,
            &product.evidence.snapshot_id,
            &to_i64("serving as-of", product.evidence.as_of_epoch)?,
            &to_i64(
                "serving complete through",
                product.evidence.complete_through_epoch,
            )?,
            &SERVING_FACTS_VERSION,
            &product.evidence_sha256,
            &product.evidence.activity_evidence_sha256,
            &product.evidence.content_artifact.sha256,
            &product.evidence.hourly_artifact.sha256,
            &product.evidence.kind_artifact.sha256,
            &to_i64("serving logical events", product.evidence.logical_events)?,
            &to_i64(
                "serving hourly rows",
                product.evidence.hourly_artifact.row_count,
            )?,
            &to_i64(
                "serving kind rows",
                product.evidence.kind_artifact.row_count,
            )?,
            &to_i64(
                "serving complete-hour events",
                product.evidence.complete_hour_events,
            )?,
            &to_i64(
                "serving eligible kind events",
                product.evidence.eligible_kind_events,
            )?,
            &to_i64(
                "serving eligible content bytes",
                product.evidence.eligible_content_bytes,
            )?,
        ],
    )?;
    copy_hourly(transaction, &product_id, product)?;
    copy_kinds(transaction, &product_id, product)?;
    reconcile(transaction, &product_id, product)?;
    Ok(ServingFactsPublishOutcome::Published { product_id })
}

/// Query a bounded half-open range of exact hourly counts.
pub fn query_published_serving_hourly(
    client: &mut impl GenericClient,
    product_id: &str,
    start_hour: u32,
    end_hour: u32,
    kind: Option<u16>,
) -> Result<Vec<ServingHourlyRow>> {
    if start_hour >= end_hour || end_hour - start_hour > 24 * 366 {
        return Err(Error::Validation(
            "serving hourly query must cover 1 through 8784 hours".to_owned(),
        ));
    }
    let rows = client.query(
        "SELECT hour_epoch,event_count
           FROM pensieve_analytics.serving_hourly_counts
          WHERE product_id=$1 AND kind=$2 AND hour_epoch >= $3 AND hour_epoch < $4
          ORDER BY hour_epoch ASC",
        &[
            &product_id,
            &kind.map_or(-1_i32, i32::from),
            &i64::from(start_hour),
            &i64::from(end_hour),
        ],
    )?;
    rows.into_iter()
        .map(|row| {
            Ok(ServingHourlyRow {
                hour_epoch: u32::try_from(row.get::<_, i64>(0)).map_err(|_| {
                    Error::Validation("published serving hour is invalid".to_owned())
                })?,
                kind,
                event_count: from_i64("published serving hourly count", row.get(1))?,
            })
        })
        .collect()
}

/// Query one exact all-time kind summary.
pub fn query_published_serving_kind(
    client: &mut impl GenericClient,
    product_id: &str,
    kind: u16,
) -> Result<Option<ServingKindRow>> {
    client
        .query_opt(
            "SELECT event_count,unique_pubkeys,first_seen,last_seen,content_bytes,content_rows
               FROM pensieve_analytics.serving_kind_summaries
              WHERE product_id=$1 AND kind=$2",
            &[&product_id, &i32::from(kind)],
        )?
        .map(|row| decode_kind_row(kind, &row, 0))
        .transpose()
}

fn copy_hourly(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedServingFacts,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "COPY pensieve_analytics.serving_hourly_counts
         (product_id,hour_epoch,kind,event_count) FROM STDIN WITH (FORMAT csv)",
    )?;
    visit_serving_hourly_rows(product, |row| {
        writeln!(
            writer,
            "{product_id},{},{},{}",
            row.hour_epoch,
            row.kind.map_or(-1_i32, i32::from),
            row.event_count,
        )?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    if inserted != product.evidence.hourly_artifact.row_count {
        return Err(Error::Validation(format!(
            "Postgres copied {inserted} serving hourly rows, expected {}",
            product.evidence.hourly_artifact.row_count
        )));
    }
    Ok(())
}

fn copy_kinds(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedServingFacts,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "COPY pensieve_analytics.serving_kind_summaries
         (product_id,kind,event_count,unique_pubkeys,first_seen,last_seen,content_bytes,content_rows)
         FROM STDIN WITH (FORMAT csv)",
    )?;
    visit_serving_kind_rows(product, |row| {
        writeln!(
            writer,
            "{product_id},{},{},{},{},{},{},{}",
            row.kind,
            row.event_count,
            row.unique_pubkeys,
            row.first_seen,
            row.last_seen,
            row.content_bytes,
            row.content_rows,
        )?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    if inserted != product.evidence.kind_artifact.row_count {
        return Err(Error::Validation(format!(
            "Postgres copied {inserted} serving kind rows, expected {}",
            product.evidence.kind_artifact.row_count
        )));
    }
    Ok(())
}

fn reconcile(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedServingFacts,
) -> Result<()> {
    let metadata = transaction.query_one(
        "SELECT snapshot_id,as_of_epoch,complete_through_epoch,product_version,
                evidence_sha256,activity_evidence_sha256,enriched_artifact_sha256,
                hourly_artifact_sha256,kind_artifact_sha256,logical_events,
                hourly_rows,kind_rows,complete_hour_events,eligible_kind_events,
                eligible_content_bytes
           FROM pensieve_analytics.serving_fact_products WHERE product_id=$1",
        &[&product_id],
    )?;
    if metadata.get::<_, String>(0) != product.evidence.snapshot_id
        || from_i64("published serving as-of", metadata.get(1))? != product.evidence.as_of_epoch
        || from_i64("published serving boundary", metadata.get(2))?
            != product.evidence.complete_through_epoch
        || metadata.get::<_, String>(3) != SERVING_FACTS_VERSION
        || metadata.get::<_, String>(4) != product.evidence_sha256
        || metadata.get::<_, String>(5) != product.evidence.activity_evidence_sha256
        || metadata.get::<_, String>(6) != product.evidence.content_artifact.sha256
        || metadata.get::<_, String>(7) != product.evidence.hourly_artifact.sha256
        || metadata.get::<_, String>(8) != product.evidence.kind_artifact.sha256
        || from_i64("published serving logical", metadata.get(9))?
            != product.evidence.logical_events
        || from_i64("published serving hourly rows", metadata.get(10))?
            != product.evidence.hourly_artifact.row_count
        || from_i64("published serving kind rows", metadata.get(11))?
            != product.evidence.kind_artifact.row_count
        || from_i64("published complete-hour events", metadata.get(12))?
            != product.evidence.complete_hour_events
        || from_i64("published eligible kind events", metadata.get(13))?
            != product.evidence.eligible_kind_events
        || from_i64("published content bytes", metadata.get(14))?
            != product.evidence.eligible_content_bytes
    {
        return Err(Error::Validation(
            "published serving product metadata does not reconcile".to_owned(),
        ));
    }
    reconcile_hourly(transaction, product_id, product)?;
    reconcile_kinds(transaction, product_id, product)
}

fn reconcile_hourly(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedServingFacts,
) -> Result<()> {
    let parameters: [&(dyn postgres::types::ToSql + Sync); 1] = [&product_id];
    let mut rows = transaction.query_raw(
        "SELECT hour_epoch,kind,event_count
           FROM pensieve_analytics.serving_hourly_counts WHERE product_id=$1
          ORDER BY hour_epoch ASC,kind ASC",
        parameters,
    )?;
    visit_serving_hourly_rows(product, |expected| {
        let row = rows
            .next()?
            .ok_or_else(|| Error::Validation("published hourly rows ended early".to_owned()))?;
        let actual = ServingHourlyRow {
            hour_epoch: u32::try_from(row.get::<_, i64>(0))
                .map_err(|_| Error::Validation("published hour is invalid".to_owned()))?,
            kind: decode_kind(row.get(1))?,
            event_count: from_i64("published hourly event count", row.get(2))?,
        };
        if actual != expected {
            return Err(Error::Validation(
                "published hourly row differs from canonical artifact".to_owned(),
            ));
        }
        Ok(())
    })?;
    if rows.next()?.is_some() {
        return Err(Error::Validation(
            "published hourly relation has extra rows".to_owned(),
        ));
    }
    Ok(())
}

fn reconcile_kinds(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedServingFacts,
) -> Result<()> {
    let parameters: [&(dyn postgres::types::ToSql + Sync); 1] = [&product_id];
    let mut rows = transaction.query_raw(
        "SELECT kind,event_count,unique_pubkeys,first_seen,last_seen,content_bytes,content_rows
           FROM pensieve_analytics.serving_kind_summaries WHERE product_id=$1 ORDER BY kind ASC",
        parameters,
    )?;
    visit_serving_kind_rows(product, |expected| {
        let row = rows
            .next()?
            .ok_or_else(|| Error::Validation("published kind rows ended early".to_owned()))?;
        let kind = u16::try_from(row.get::<_, i32>(0))
            .map_err(|_| Error::Validation("published kind is invalid".to_owned()))?;
        let actual = decode_kind_row(kind, &row, 1)?;
        if actual != expected {
            return Err(Error::Validation(
                "published kind row differs from canonical artifact".to_owned(),
            ));
        }
        Ok(())
    })?;
    if rows.next()?.is_some() {
        return Err(Error::Validation(
            "published kind relation has extra rows".to_owned(),
        ));
    }
    Ok(())
}

fn decode_kind_row(kind: u16, row: &postgres::Row, offset: usize) -> Result<ServingKindRow> {
    Ok(ServingKindRow {
        kind,
        event_count: from_i64("published kind events", row.get(offset))?,
        unique_pubkeys: from_i64("published kind pubkeys", row.get(offset + 1))?,
        first_seen: u32::try_from(row.get::<_, i64>(offset + 2))
            .map_err(|_| Error::Validation("published first seen is invalid".to_owned()))?,
        last_seen: u32::try_from(row.get::<_, i64>(offset + 3))
            .map_err(|_| Error::Validation("published last seen is invalid".to_owned()))?,
        content_bytes: from_i64("published content bytes", row.get(offset + 4))?,
        content_rows: from_i64("published content rows", row.get(offset + 5))?,
    })
}

pub(crate) fn validate_serving_facts_publication(
    product: &BoundedServingFacts,
) -> Result<ValidatedServingFactsPublication> {
    product.validate_for_publication(
        &product.evidence.snapshot_id,
        product.evidence.as_of_epoch,
        &product.evidence.activity_evidence_sha256,
    )?;
    Ok(ValidatedServingFactsPublication)
}

fn product_id(run_id: &str, product: &BoundedServingFacts) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-serving-facts-product-v1\0");
    digest.update(run_id.as_bytes());
    digest.update([0]);
    digest.update(product.evidence_sha256.as_bytes());
    hex::encode(digest.finalize())
}

fn decode_kind(value: i32) -> Result<Option<u16>> {
    if value == -1 {
        Ok(None)
    } else {
        Ok(Some(u16::try_from(value).map_err(|_| {
            Error::Validation("published serving kind is invalid".to_owned())
        })?))
    }
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::NegativeLedgerValue { field, value })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::str::FromStr;

    use pensieve_core::NOSTR_GENESIS_TIMESTAMP;

    use super::*;
    use crate::{
        ArtifactIdentity, CONTENT_FACT_BYTES, EVENT_FACT_BYTES, FIXED_ACTIVITY_RECORD_BYTES,
        HOURLY_COUNT_BYTES, KIND_SUMMARY_BYTES, ServingFactsEvidence, ServingFactsMemoryEvidence,
    };

    #[test]
    fn postgres_publication_rolls_back_retries_and_never_moves_current() {
        let Ok(url) = std::env::var("PENSIEVE_TEST_POSTGRES_URL") else {
            return;
        };
        let directory = tempfile::tempdir().expect("create serving publication fixture");
        let product = synthetic_product(directory.path());
        let baseline = "serving-b3-baseline";
        let mut client = postgres::Config::from_str(&url)
            .expect("parse test Postgres URL")
            .connect(postgres::NoTls)
            .expect("connect test Postgres");
        client
            .batch_execute("DROP SCHEMA IF EXISTS pensieve_analytics CASCADE")
            .expect("reset isolated database");
        client.batch_execute(SCHEMA_SQL).expect("apply schema");
        client
            .execute(
                "INSERT INTO pensieve_analytics.runs (
                     run_id,snapshot_id,previous_run_id,run_kind,query_version,
                     code_version,as_of_epoch,started_at,completed_at,published_at,
                     physical_rows,logical_events,duplicate_rows,api_representable_events,
                     event_daily_rows,event_daily_kind_rows,kind_all_time_rows,validation
                 ) VALUES ($1,$2,NULL,'incremental','slice-b3-v2','test',$3,
                           now(),now(),now(),0,0,0,0,0,0,0,
                           jsonb_build_object('fixed_activity_evidence_sha256',$4))",
                &[
                    &baseline,
                    &product.evidence.snapshot_id,
                    &(product.evidence.as_of_epoch as i64),
                    &product.evidence.activity_evidence_sha256,
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
                "CREATE FUNCTION pensieve_analytics.reject_serving_hourly()
                   RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN RAISE EXCEPTION 'injected serving failure'; END $$;
                 CREATE TRIGGER reject_serving_hourly
                   BEFORE INSERT ON pensieve_analytics.serving_hourly_counts
                   FOR EACH ROW EXECUTE FUNCTION pensieve_analytics.reject_serving_hourly();",
            )
            .expect("install failure injection");
        assert!(publish_serving_facts(&mut client, baseline, &product).is_err());
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*)::BIGINT FROM pensieve_analytics.serving_fact_products",
                    &[],
                )
                .expect("count rolled-back serving products")
                .get::<_, i64>(0),
            0
        );
        client
            .batch_execute(
                "DROP TRIGGER reject_serving_hourly
                   ON pensieve_analytics.serving_hourly_counts;
                 DROP FUNCTION pensieve_analytics.reject_serving_hourly();",
            )
            .expect("remove failure injection");
        let product_id = match publish_serving_facts(&mut client, baseline, &product)
            .expect("publish serving product")
        {
            ServingFactsPublishOutcome::Published { product_id } => product_id,
            outcome => panic!("unexpected publication: {outcome:?}"),
        };
        let hour = u32::try_from(product.evidence.complete_through_epoch / 3_600 - 1)
            .expect("fixture hour");
        assert_eq!(
            query_published_serving_hourly(&mut client, &product_id, hour, hour + 1, None)
                .expect("query published hourly facts"),
            vec![ServingHourlyRow {
                hour_epoch: hour,
                kind: None,
                event_count: 1,
            }]
        );
        assert_eq!(
            query_published_serving_kind(&mut client, &product_id, 1)
                .expect("query published kind facts"),
            Some(ServingKindRow {
                kind: 1,
                event_count: 1,
                unique_pubkeys: 1,
                first_seen: hour * 3_600 + 1,
                last_seen: hour * 3_600 + 1,
                content_bytes: 3,
                content_rows: 1,
            })
        );
        assert_eq!(
            publish_serving_facts(&mut client, baseline, &product).expect("retry serving product"),
            ServingFactsPublishOutcome::AlreadyPublished {
                product_id: product_id.clone(),
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

    fn synthetic_product(root: &Path) -> BoundedServingFacts {
        let complete_through = ((u64::from(NOSTR_GENESIS_TIMESTAMP) / 3_600) + 2) * 3_600;
        let created_at = u32::try_from(complete_through - 3_600 + 1).expect("created at");
        let as_of_epoch = complete_through + 1;
        let event_id = [1_u8; 32];

        let events = root.join("events.run");
        let mut event_bytes = Vec::with_capacity(EVENT_FACT_BYTES);
        event_bytes.extend_from_slice(&event_id);
        event_bytes.extend_from_slice(&u64::from(created_at).to_be_bytes());
        event_bytes.extend_from_slice(&1_u16.to_be_bytes());
        fs::write(&events, event_bytes).expect("write event facts");

        let content = root.join("content.run");
        let mut content_bytes = Vec::with_capacity(CONTENT_FACT_BYTES);
        content_bytes.extend_from_slice(&event_id);
        content_bytes.extend_from_slice(&u64::from(created_at).to_be_bytes());
        content_bytes.extend_from_slice(&1_u16.to_be_bytes());
        content_bytes.extend_from_slice(&3_u64.to_be_bytes());
        fs::write(&content, content_bytes).expect("write content facts");

        let activity = root.join("activity.run");
        let mut activity_bytes = Vec::with_capacity(FIXED_ACTIVITY_RECORD_BYTES);
        activity_bytes.extend_from_slice(&[2_u8; 32]);
        activity_bytes.extend_from_slice(&created_at.to_be_bytes());
        activity_bytes.extend_from_slice(&1_u16.to_be_bytes());
        activity_bytes.extend_from_slice(&event_id);
        fs::write(&activity, activity_bytes).expect("write activity facts");

        let hour = created_at / 3_600;
        let hourly = root.join("hourly.run");
        let mut hourly_bytes = Vec::with_capacity(2 * HOURLY_COUNT_BYTES);
        for kind_key in [0_u32, 2] {
            hourly_bytes.extend_from_slice(&hour.to_be_bytes());
            hourly_bytes.extend_from_slice(&kind_key.to_be_bytes());
            hourly_bytes.extend_from_slice(&1_u64.to_be_bytes());
        }
        fs::write(&hourly, hourly_bytes).expect("write hourly facts");

        let kinds = root.join("kinds.run");
        let mut kind_bytes = Vec::with_capacity(KIND_SUMMARY_BYTES);
        kind_bytes.extend_from_slice(&1_u16.to_be_bytes());
        kind_bytes.extend_from_slice(&1_u64.to_be_bytes());
        kind_bytes.extend_from_slice(&1_u64.to_be_bytes());
        kind_bytes.extend_from_slice(&created_at.to_be_bytes());
        kind_bytes.extend_from_slice(&created_at.to_be_bytes());
        kind_bytes.extend_from_slice(&3_u64.to_be_bytes());
        kind_bytes.extend_from_slice(&1_u64.to_be_bytes());
        fs::write(&kinds, kind_bytes).expect("write kind facts");

        BoundedServingFacts {
            evidence: ServingFactsEvidence {
                schema_version: 1,
                runner_version: crate::SERVING_FACTS_RUNNER_VERSION.to_owned(),
                status: "completed".to_owned(),
                snapshot_id: "sha256:serving-publication-test".to_owned(),
                as_of_epoch,
                complete_through_epoch: complete_through,
                object_count: 1,
                physical_rows: 1,
                delta_object_count: 1,
                baseline_evidence_sha256: None,
                logical_events: 1,
                duplicate_rows: 0,
                initial_event_facts_evidence_sha256: "a".repeat(64),
                initial_event_facts_artifact: artifact(&events, 1),
                activity_evidence_sha256: "b".repeat(64),
                activity_artifact: artifact(&activity, 1),
                content_artifact: artifact(&content, 1),
                hourly_artifact: artifact(&hourly, 2),
                kind_artifact: artifact(&kinds, 1),
                complete_hour_events: 1,
                eligible_kind_events: 1,
                eligible_content_bytes: 3,
                batch_count: 0,
                merge_count: 0,
                estimated_run_bytes: 0,
                disk_reserve_bytes: 0,
                memory: ServingFactsMemoryEvidence {
                    max_batch_bytes: 0,
                    max_batch_rows: 0,
                    max_merge_buffered_bytes: 0,
                    hourly_keys: 2,
                    kind_counter_slots: 65_536,
                },
                batch_checkpoints: Vec::new(),
                merge_checkpoints: Vec::new(),
            },
            evidence_sha256: "c".repeat(64),
        }
    }

    fn artifact(path: &Path, row_count: u64) -> ArtifactIdentity {
        ArtifactIdentity {
            path: path.to_string_lossy().into_owned(),
            byte_size: path.metadata().expect("artifact metadata").len(),
            row_count,
            min_key: None,
            max_key: None,
            sha256: pensieve_lake::sha256_file(path).expect("hash artifact"),
        }
    }
}
