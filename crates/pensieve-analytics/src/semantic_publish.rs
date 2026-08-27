//! Dormant transactional publication for exact Slice 7 semantic products.

use std::io::Write;

use postgres::{Client, GenericClient};
use sha2::{Digest, Sha256};

use crate::schema::SCHEMA_SQL;
use crate::{
    BoundedSemanticFacts, COHORT_RETENTION_QUERY_VERSION, Error, Result,
    SEMANTIC_FACTS_RUNNER_VERSION, SEMANTIC_FACTS_VERSION,
};

const PUBLICATION_LOCK_ID: i64 = 0x5045_4e53_4945_5645;

/// Result of atomically publishing one dormant Slice 7 product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticPublishOutcome {
    /// A new versioned product and all rollups committed.
    Published { product_id: String },
    /// An identical committed product reconciled successfully.
    AlreadyPublished { product_id: String },
}

/// Publish exact semantic rollups without moving the analytics current pointer.
pub fn publish_semantic_facts(
    client: &mut Client,
    baseline_run_id: &str,
    product: &BoundedSemanticFacts,
) -> Result<SemanticPublishOutcome> {
    validate_product(product)?;
    client.batch_execute(SCHEMA_SQL)?;
    let product_id = semantic_product_id(baseline_run_id, product);
    let mut transaction = client.transaction()?;
    transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&PUBLICATION_LOCK_ID])?;
    let current = transaction.query_one(
        "SELECT run_id, snapshot_id, query_version, as_of_epoch
           FROM pensieve_analytics.current_run_metadata FOR SHARE",
        &[],
    )?;
    if current.get::<_, String>(0) != baseline_run_id
        || current.get::<_, String>(1) != product.evidence.snapshot_id
        || current.get::<_, String>(2) != COHORT_RETENTION_QUERY_VERSION
        || from_i64("semantic current as-of", current.get(3))? != product.evidence.as_of_epoch
    {
        return Err(Error::Validation(
            "current Postgres run is not the exact corrected B3 Slice 7 baseline".to_owned(),
        ));
    }
    if transaction
        .query_opt(
            "SELECT product_id FROM pensieve_analytics.semantic_products WHERE product_id=$1",
            &[&product_id],
        )?
        .is_some()
    {
        reconcile(&mut transaction, &product_id, product)?;
        transaction.commit()?;
        return Ok(SemanticPublishOutcome::AlreadyPublished { product_id });
    }

    transaction.execute(
        "INSERT INTO pensieve_analytics.semantic_products (
             product_id, run_id, snapshot_id, as_of_epoch, product_version,
             evidence_sha256, fact_artifact_sha256, rollup_sha256,
             logical_relevant_events, engagement_days, longform_days, zap_days, published_at
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,now())",
        &[
            &product_id,
            &baseline_run_id,
            &product.evidence.snapshot_id,
            &to_i64("semantic as-of", product.evidence.as_of_epoch)?,
            &SEMANTIC_FACTS_VERSION,
            &product.evidence_sha256,
            &product.evidence.final_artifact.sha256,
            &product.evidence.rollup_sha256,
            &to_i64(
                "semantic logical events",
                product.evidence.logical_relevant_events,
            )?,
            &to_i64(
                "semantic engagement days",
                product.evidence.rollups.engagement.len() as u64,
            )?,
            &to_i64(
                "semantic longform days",
                product.evidence.rollups.longform.len() as u64,
            )?,
            &to_i64(
                "semantic zap days",
                product.evidence.rollups.zaps.len() as u64,
            )?,
        ],
    )?;
    copy_rollups(&mut transaction, &product_id, product)?;
    reconcile(&mut transaction, &product_id, product)?;
    transaction.commit()?;
    Ok(SemanticPublishOutcome::Published { product_id })
}

fn copy_rollups(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedSemanticFacts,
) -> Result<()> {
    let mut engagement = transaction.copy_in(
        "COPY pensieve_analytics.semantic_engagement_daily
         (product_id,day_epoch,original_notes,replies,reactions) FROM STDIN WITH (FORMAT csv)",
    )?;
    for day in product.evidence.rollups.engagement.values() {
        writeln!(
            engagement,
            "{product_id},{},{},{},{}",
            day.day_epoch, day.original_notes, day.replies, day.reactions
        )?;
    }
    let inserted = engagement.finish()?;
    if inserted != product.evidence.rollups.engagement.len() as u64 {
        return Err(Error::Validation(
            "engagement COPY row count mismatch".to_owned(),
        ));
    }

    let mut longform = transaction.copy_in(
        "COPY pensieve_analytics.semantic_longform_daily
         (product_id,day_epoch,articles,content_bytes) FROM STDIN WITH (FORMAT csv)",
    )?;
    for day in product.evidence.rollups.longform.values() {
        writeln!(
            longform,
            "{product_id},{},{},{}",
            day.day_epoch, day.articles, day.content_bytes
        )?;
    }
    let inserted = longform.finish()?;
    if inserted != product.evidence.rollups.longform.len() as u64 {
        return Err(Error::Validation(
            "long-form COPY row count mismatch".to_owned(),
        ));
    }

    let mut zaps = transaction.copy_in(
        "COPY pensieve_analytics.semantic_zap_daily
         (product_id,day_epoch,accepted,amount_msats,validated_sender_facts,
          validated_recipient_facts) FROM STDIN WITH (FORMAT csv)",
    )?;
    for day in product.evidence.rollups.zaps.values() {
        writeln!(
            zaps,
            "{product_id},{},{},{},{},{}",
            day.day_epoch,
            day.accepted,
            day.amount_msats,
            day.validated_senders,
            day.validated_recipients
        )?;
    }
    let inserted = zaps.finish()?;
    if inserted != product.evidence.rollups.zaps.len() as u64 {
        return Err(Error::Validation("zap COPY row count mismatch".to_owned()));
    }

    let mut histogram = transaction.copy_in(
        "COPY pensieve_analytics.semantic_zap_histogram_daily
         (product_id,day_epoch,bucket,zap_count,amount_msats) FROM STDIN WITH (FORMAT csv)",
    )?;
    for day in product.evidence.rollups.zaps.values() {
        for bucket in 0..17 {
            writeln!(
                histogram,
                "{product_id},{},{bucket},{},{}",
                day.day_epoch, day.histogram[bucket], day.histogram_amount_msats[bucket]
            )?;
        }
    }
    let inserted = histogram.finish()?;
    let expected = (product.evidence.rollups.zaps.len() as u64)
        .checked_mul(17)
        .ok_or_else(|| Error::Validation("zap histogram rows overflowed".to_owned()))?;
    if inserted != expected {
        return Err(Error::Validation(
            "zap histogram COPY row count mismatch".to_owned(),
        ));
    }

    let mut rejected = transaction.copy_in(
        "COPY pensieve_analytics.semantic_zap_rejections_daily
         (product_id,day_epoch,reason,rejected_count) FROM STDIN WITH (FORMAT csv)",
    )?;
    for day in product.evidence.rollups.zaps.values() {
        for reason in 0..6 {
            writeln!(
                rejected,
                "{product_id},{},{reason},{}",
                day.day_epoch, day.rejected[reason]
            )?;
        }
    }
    let inserted = rejected.finish()?;
    let expected = (product.evidence.rollups.zaps.len() as u64)
        .checked_mul(6)
        .ok_or_else(|| Error::Validation("zap rejection rows overflowed".to_owned()))?;
    if inserted != expected {
        return Err(Error::Validation(
            "zap rejection COPY row count mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn reconcile(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedSemanticFacts,
) -> Result<()> {
    let metadata = transaction.query_one(
        "SELECT snapshot_id,as_of_epoch,product_version,evidence_sha256,
                fact_artifact_sha256,rollup_sha256,logical_relevant_events,
                engagement_days,longform_days,zap_days
           FROM pensieve_analytics.semantic_products WHERE product_id=$1",
        &[&product_id],
    )?;
    if metadata.get::<_, String>(0) != product.evidence.snapshot_id
        || from_i64("semantic metadata as-of", metadata.get(1))? != product.evidence.as_of_epoch
        || metadata.get::<_, String>(2) != SEMANTIC_FACTS_VERSION
        || metadata.get::<_, String>(3) != product.evidence_sha256
        || metadata.get::<_, String>(4) != product.evidence.final_artifact.sha256
        || metadata.get::<_, String>(5) != product.evidence.rollup_sha256
        || from_i64("semantic metadata events", metadata.get(6))?
            != product.evidence.logical_relevant_events
        || from_i64("semantic metadata engagement days", metadata.get(7))?
            != product.evidence.rollups.engagement.len() as u64
        || from_i64("semantic metadata longform days", metadata.get(8))?
            != product.evidence.rollups.longform.len() as u64
        || from_i64("semantic metadata zap days", metadata.get(9))?
            != product.evidence.rollups.zaps.len() as u64
    {
        return Err(Error::Validation(
            "semantic product metadata mismatch".to_owned(),
        ));
    }
    let engagement = transaction.query_one(
        "SELECT count(*)::BIGINT,coalesce(sum(original_notes),0)::BIGINT,
                coalesce(sum(replies),0)::BIGINT,coalesce(sum(reactions),0)::BIGINT
           FROM pensieve_analytics.semantic_engagement_daily WHERE product_id=$1",
        &[&product_id],
    )?;
    if from_i64("engagement rows", engagement.get(0))?
        != product.evidence.rollups.engagement.len() as u64
        || from_i64("original notes", engagement.get(1))?
            != product.evidence.domain_counts.original_notes
        || from_i64("replies", engagement.get(2))? != product.evidence.domain_counts.replies
        || from_i64("reactions", engagement.get(3))? != product.evidence.domain_counts.reactions
    {
        return Err(Error::Validation(
            "semantic engagement totals mismatch".to_owned(),
        ));
    }
    let longform = transaction.query_one(
        "SELECT count(*)::BIGINT,coalesce(sum(articles),0)::BIGINT
           FROM pensieve_analytics.semantic_longform_daily WHERE product_id=$1",
        &[&product_id],
    )?;
    if from_i64("longform rows", longform.get(0))? != product.evidence.rollups.longform.len() as u64
        || from_i64("longform articles", longform.get(1))?
            != product.evidence.domain_counts.longform_articles
    {
        return Err(Error::Validation(
            "semantic long-form totals mismatch".to_owned(),
        ));
    }
    let zaps = transaction.query_one(
        "SELECT count(*)::BIGINT,coalesce(sum(accepted),0)::BIGINT,
                coalesce(sum(amount_msats),0)::BIGINT
           FROM pensieve_analytics.semantic_zap_daily WHERE product_id=$1",
        &[&product_id],
    )?;
    let histogram = transaction.query_one(
        "SELECT count(*)::BIGINT,coalesce(sum(zap_count),0)::BIGINT,
                coalesce(sum(amount_msats),0)::BIGINT
           FROM pensieve_analytics.semantic_zap_histogram_daily WHERE product_id=$1",
        &[&product_id],
    )?;
    let rejected = transaction.query_one(
        "SELECT count(*)::BIGINT,coalesce(sum(rejected_count),0)::BIGINT
           FROM pensieve_analytics.semantic_zap_rejections_daily WHERE product_id=$1",
        &[&product_id],
    )?;
    let expected_amount = product
        .evidence
        .rollups
        .zaps
        .values()
        .try_fold(0_u64, |sum, day| sum.checked_add(day.amount_msats))
        .ok_or_else(|| Error::Validation("semantic zap amount overflowed".to_owned()))?;
    if from_i64("zap rows", zaps.get(0))? != product.evidence.rollups.zaps.len() as u64
        || from_i64("accepted zaps", zaps.get(1))? != product.evidence.domain_counts.accepted_zaps
        || from_i64("zap amounts", zaps.get(2))? != expected_amount
        || from_i64("histogram rows", histogram.get(0))?
            != product.evidence.rollups.zaps.len() as u64 * 17
        || from_i64("histogram count", histogram.get(1))?
            != product.evidence.domain_counts.accepted_zaps
        || from_i64("histogram amount", histogram.get(2))? != expected_amount
        || from_i64("rejection rows", rejected.get(0))?
            != product.evidence.rollups.zaps.len() as u64 * 6
        || from_i64("rejected zaps", rejected.get(1))?
            != product.evidence.domain_counts.rejected_zaps
    {
        return Err(Error::Validation("semantic zap totals mismatch".to_owned()));
    }
    Ok(())
}

fn validate_product(product: &BoundedSemanticFacts) -> Result<()> {
    if product.evidence.status != "completed"
        || product.evidence.runner_version != SEMANTIC_FACTS_RUNNER_VERSION
        || product.evidence.final_artifact.row_count != product.evidence.retained_relevant_events
        || pensieve_lake::sha256_file(&product.artifact_path)?
            != product.evidence.final_artifact.sha256
    {
        return Err(Error::Validation(
            "semantic product failed immutable publication validation".to_owned(),
        ));
    }
    Ok(())
}

fn semantic_product_id(baseline_run_id: &str, product: &BoundedSemanticFacts) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-semantic-product-v2\0");
    digest.update(baseline_run_id.as_bytes());
    digest.update([0]);
    digest.update(product.evidence_sha256.as_bytes());
    hex::encode(digest.finalize())
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::NegativeLedgerValue { field, value })
}
