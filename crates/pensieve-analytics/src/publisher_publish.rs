//! Atomic Postgres publication for exact Slice 9 publisher rankings.

use std::io::Write;

use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, GenericClient};
use sha2::{Digest, Sha256};

use crate::schema::SCHEMA_SQL;
use crate::{
    BoundedPublisherRanking, Error, PUBLISHER_RANKING_VERSION, PublisherRankingRow, Result,
    visit_publisher_ranking_rows,
};

const PUBLICATION_LOCK_ID: i64 = 0x5045_4e53_4945_5645;

/// Proof that the full immutable product was checked before transaction entry.
pub(crate) struct ValidatedPublisherRankingPublication;

/// Result of publishing one versioned exact publisher product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublisherRankingPublishOutcome {
    /// A new product committed.
    Published { product_id: String },
    /// The identical product was already committed and reconciled.
    AlreadyPublished { product_id: String },
}

/// Publish one ranking product without changing the analytics run pointer.
pub fn publish_publisher_ranking(
    client: &mut Client,
    baseline_run_id: &str,
    product: &BoundedPublisherRanking,
) -> Result<PublisherRankingPublishOutcome> {
    let validated = validate_publisher_ranking_publication(product)?;
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
        || from_i64("publisher current as-of", current.get(2))? != product.evidence.as_of_epoch
        || current.get::<_, Option<String>>(3).as_deref()
            != Some(product.evidence.activity_evidence_sha256.as_str())
    {
        return Err(Error::Validation(
            "current Postgres run is not the exact Slice 9 baseline".to_owned(),
        ));
    }
    let outcome = publish_publisher_ranking_in_transaction(
        &mut transaction,
        baseline_run_id,
        product,
        &validated,
    )?;
    transaction.commit()?;
    Ok(outcome)
}

/// Publish a prevalidated ranking product inside its generation transaction.
pub(crate) fn publish_publisher_ranking_in_transaction(
    transaction: &mut impl GenericClient,
    run_id: &str,
    product: &BoundedPublisherRanking,
    _validated: &ValidatedPublisherRankingPublication,
) -> Result<PublisherRankingPublishOutcome> {
    let product_id = product_id(run_id, product);
    if transaction
        .query_opt(
            "SELECT product_id FROM pensieve_analytics.publisher_ranking_products
              WHERE product_id=$1",
            &[&product_id],
        )?
        .is_some()
    {
        reconcile(transaction, &product_id, product)?;
        return Ok(PublisherRankingPublishOutcome::AlreadyPublished { product_id });
    }
    let windows = product
        .evidence
        .windows_days
        .iter()
        .map(|value| i32::try_from(*value))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::Validation("publisher window exceeds i32".to_owned()))?;
    transaction.execute(
        "INSERT INTO pensieve_analytics.publisher_ranking_products (
             product_id,run_id,snapshot_id,as_of_epoch,product_version,
             evidence_sha256,activity_evidence_sha256,activity_artifact_sha256,
             ranking_artifact_sha256,windows_days,top_limit,source_records,
             ledger_rows,ranking_groups,ranking_rows,published_at
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,now())",
        &[
            &product_id,
            &run_id,
            &product.evidence.snapshot_id,
            &to_i64("publisher as-of", product.evidence.as_of_epoch)?,
            &PUBLISHER_RANKING_VERSION,
            &product.evidence_sha256,
            &product.evidence.activity_evidence_sha256,
            &product.evidence.activity_artifact_sha256,
            &product.evidence.ranking_artifact.sha256,
            &windows,
            &i32::try_from(product.evidence.top_limit)
                .map_err(|_| Error::Validation("publisher top limit exceeds i32".to_owned()))?,
            &to_i64("publisher source records", product.evidence.source_records)?,
            &to_i64("publisher ledger rows", product.evidence.ledger_rows)?,
            &to_i64("publisher groups", product.evidence.ranking_groups)?,
            &to_i64(
                "publisher ranking rows",
                product.evidence.ranking_artifact.row_count,
            )?,
        ],
    )?;
    copy_rows(transaction, &product_id, product)?;
    reconcile(transaction, &product_id, product)?;
    Ok(PublisherRankingPublishOutcome::Published { product_id })
}

/// Query one bounded exact ranking page.
pub fn query_published_publisher_ranking(
    client: &mut impl GenericClient,
    product_id: &str,
    days: u32,
    kind: Option<u16>,
    limit: u64,
) -> Result<Vec<PublisherRankingRow>> {
    if limit == 0 || limit > 1_000 {
        return Err(Error::Validation(
            "publisher ranking limit must be between 1 and 1000".to_owned(),
        ));
    }
    let rows = client.query(
        "SELECT pubkey,event_count,kinds_count,first_event,last_event
           FROM pensieve_analytics.publisher_ranking_rows
          WHERE product_id=$1 AND days=$2 AND kind=$3
          ORDER BY event_count DESC,pubkey ASC LIMIT $4",
        &[
            &product_id,
            &i32::try_from(days)
                .map_err(|_| Error::Validation("publisher days exceed i32".to_owned()))?,
            &kind.map_or(-1_i32, i32::from),
            &to_i64("publisher query limit", limit)?,
        ],
    )?;
    rows.into_iter()
        .map(|row| decode_postgres_row(days, kind, &row))
        .collect()
}

fn copy_rows(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedPublisherRanking,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "COPY pensieve_analytics.publisher_ranking_rows
         (product_id,days,kind,pubkey,event_count,kinds_count,first_event,last_event)
         FROM STDIN WITH (FORMAT csv)",
    )?;
    visit_publisher_ranking_rows(product, |row| {
        writeln!(
            writer,
            "{product_id},{},{},\\x{},{},{},{},{}",
            row.days,
            row.kind.map_or(-1_i32, i32::from),
            hex::encode(row.pubkey),
            row.event_count,
            row.kinds_count,
            row.first_event,
            row.last_event,
        )?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    if inserted != product.evidence.ranking_artifact.row_count {
        return Err(Error::Validation(format!(
            "Postgres copied {inserted} publisher rows, expected {}",
            product.evidence.ranking_artifact.row_count
        )));
    }
    Ok(())
}

fn reconcile(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedPublisherRanking,
) -> Result<()> {
    let metadata = transaction.query_one(
        "SELECT snapshot_id,as_of_epoch,product_version,evidence_sha256,
                activity_evidence_sha256,activity_artifact_sha256,
                ranking_artifact_sha256,top_limit,source_records,ledger_rows,
                ranking_groups,ranking_rows
           FROM pensieve_analytics.publisher_ranking_products WHERE product_id=$1",
        &[&product_id],
    )?;
    if metadata.get::<_, String>(0) != product.evidence.snapshot_id
        || from_i64("published publisher as-of", metadata.get(1))? != product.evidence.as_of_epoch
        || metadata.get::<_, String>(2) != PUBLISHER_RANKING_VERSION
        || metadata.get::<_, String>(3) != product.evidence_sha256
        || metadata.get::<_, String>(4) != product.evidence.activity_evidence_sha256
        || metadata.get::<_, String>(5) != product.evidence.activity_artifact_sha256
        || metadata.get::<_, String>(6) != product.evidence.ranking_artifact.sha256
        || usize::try_from(metadata.get::<_, i32>(7)).ok() != Some(product.evidence.top_limit)
        || from_i64("published publisher source rows", metadata.get(8))?
            != product.evidence.source_records
        || from_i64("published publisher ledger rows", metadata.get(9))?
            != product.evidence.ledger_rows
        || from_i64("published publisher groups", metadata.get(10))?
            != product.evidence.ranking_groups
        || from_i64("published publisher rows", metadata.get(11))?
            != product.evidence.ranking_artifact.row_count
    {
        return Err(Error::Validation(
            "published publisher metadata does not reconcile".to_owned(),
        ));
    }
    let parameters: [&(dyn postgres::types::ToSql + Sync); 1] = [&product_id];
    let mut rows = transaction.query_raw(
        "SELECT days,kind,pubkey,event_count,kinds_count,first_event,last_event
           FROM pensieve_analytics.publisher_ranking_rows WHERE product_id=$1
          ORDER BY days ASC,kind ASC,event_count DESC,pubkey ASC",
        parameters,
    )?;
    visit_publisher_ranking_rows(product, |expected| {
        let row = rows
            .next()?
            .ok_or_else(|| Error::Validation("published publisher rows ended early".to_owned()))?;
        let actual = PublisherRankingRow {
            days: u32::try_from(row.get::<_, i32>(0)).map_err(|_| {
                Error::Validation("published publisher days are invalid".to_owned())
            })?,
            kind: decode_kind(row.get(1))?,
            pubkey: fixed_32(row.get(2), "published publisher pubkey")?,
            event_count: from_i64("published publisher count", row.get(3))?,
            kinds_count: from_i64("published publisher kinds", row.get(4))?,
            first_event: u32::try_from(row.get::<_, i64>(5)).map_err(|_| {
                Error::Validation("published publisher first event is invalid".to_owned())
            })?,
            last_event: u32::try_from(row.get::<_, i64>(6)).map_err(|_| {
                Error::Validation("published publisher last event is invalid".to_owned())
            })?,
        };
        if actual != expected {
            return Err(Error::Validation(
                "published publisher row differs from canonical artifact".to_owned(),
            ));
        }
        Ok(())
    })?;
    if rows.next()?.is_some() {
        return Err(Error::Validation(
            "published publisher relation has extra rows".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_publisher_ranking_publication(
    product: &BoundedPublisherRanking,
) -> Result<ValidatedPublisherRankingPublication> {
    if product.evidence.schema_version != 1
        || product.evidence.runner_version != "pensieve-analytics-publisher-ranking-v1"
        || product.evidence.status != "completed"
        || product.evidence.product_version != PUBLISHER_RANKING_VERSION
        || pensieve_lake::sha256_file(&product.evidence.ranking_artifact.path)?
            != product.evidence.ranking_artifact.sha256
    {
        return Err(Error::Validation(
            "publisher ranking failed immutable publication validation".to_owned(),
        ));
    }
    visit_publisher_ranking_rows(product, |_| Ok(()))?;
    Ok(ValidatedPublisherRankingPublication)
}

fn product_id(run_id: &str, product: &BoundedPublisherRanking) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-publisher-ranking-product-v1\0");
    digest.update(run_id.as_bytes());
    digest.update([0]);
    digest.update(product.evidence_sha256.as_bytes());
    hex::encode(digest.finalize())
}

fn decode_postgres_row(
    days: u32,
    kind: Option<u16>,
    row: &postgres::Row,
) -> Result<PublisherRankingRow> {
    Ok(PublisherRankingRow {
        days,
        kind,
        pubkey: fixed_32(row.get(0), "publisher pubkey")?,
        event_count: from_i64("publisher event count", row.get(1))?,
        kinds_count: from_i64("publisher kinds count", row.get(2))?,
        first_event: u32::try_from(row.get::<_, i64>(3))
            .map_err(|_| Error::Validation("publisher first event is invalid".to_owned()))?,
        last_event: u32::try_from(row.get::<_, i64>(4))
            .map_err(|_| Error::Validation("publisher last event is invalid".to_owned()))?,
    })
}

fn decode_kind(value: i32) -> Result<Option<u16>> {
    if value == -1 {
        Ok(None)
    } else {
        Ok(Some(u16::try_from(value).map_err(|_| {
            Error::Validation("published publisher kind is invalid".to_owned())
        })?))
    }
}

fn fixed_32(value: Vec<u8>, label: &str) -> Result<[u8; 32]> {
    value.try_into().map_err(|value: Vec<u8>| {
        Error::Validation(format!("{label} has {} bytes instead of 32", value.len()))
    })
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::NegativeLedgerValue { field, value })
}
