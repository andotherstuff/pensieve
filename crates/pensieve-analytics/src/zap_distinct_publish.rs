//! Dormant transactional publication and fixed-memory queries for zap sketches.

use std::io::Write;

use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, GenericClient};
use sha2::{Digest, Sha256};

use crate::schema::SCHEMA_SQL;
use crate::{
    BoundedSemanticFacts, BoundedZapDistinct, COHORT_RETENTION_QUERY_VERSION, DistinctSketchUnion,
    Error, Result, ZAP_DISTINCT_VERSION, ZapParticipantRole,
};

const PUBLICATION_LOCK_ID: i64 = 0x5045_4e53_4945_5645;
const SECONDS_PER_DAY: u64 = 86_400;

/// Result of atomically publishing one dormant zap-distinct product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ZapDistinctPublishOutcome {
    /// A new versioned product and all daily leaves committed.
    Published { product_id: String },
    /// An identical committed product reconciled successfully.
    AlreadyPublished { product_id: String },
}

/// Publish daily sender/recipient leaves without moving the analytics pointer.
pub fn publish_zap_distinct(
    client: &mut Client,
    semantic_product_id: &str,
    semantic: &BoundedSemanticFacts,
    product: &BoundedZapDistinct,
) -> Result<ZapDistinctPublishOutcome> {
    validate_source(semantic, product)?;
    client.batch_execute(SCHEMA_SQL)?;
    let product_id = zap_distinct_product_id(semantic_product_id, product);
    let complete_through = floor_day(product.evidence.as_of_epoch);
    let sketch_bytes = product
        .evidence
        .leaves
        .iter()
        .try_fold(0_u64, |sum, leaf| sum.checked_add(leaf.sketch.len() as u64))
        .ok_or_else(|| Error::Validation("zap distinct sketch bytes overflowed".to_owned()))?;

    let mut transaction = client.transaction()?;
    transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&PUBLICATION_LOCK_ID])?;
    let baseline = transaction.query_one(
        "SELECT products.run_id, products.snapshot_id, products.as_of_epoch,
                products.evidence_sha256, current.query_version
           FROM pensieve_analytics.semantic_products products
           JOIN pensieve_analytics.current_run_metadata current
             ON current.run_id = products.run_id
          WHERE products.product_id = $1
          FOR SHARE OF products",
        &[&semantic_product_id],
    )?;
    if baseline.get::<_, String>(0) != current_run_id(&mut transaction)?
        || baseline.get::<_, String>(1) != product.evidence.snapshot_id
        || from_i64("zap distinct baseline as-of", baseline.get(2))? != product.evidence.as_of_epoch
        || baseline.get::<_, String>(3) != product.evidence.semantic_evidence_sha256
        || baseline.get::<_, String>(4) != COHORT_RETENTION_QUERY_VERSION
    {
        return Err(Error::Validation(
            "semantic product is not the exact current corrected B3 baseline".to_owned(),
        ));
    }

    if transaction
        .query_opt(
            "SELECT product_id
               FROM pensieve_analytics.semantic_zap_distinct_products
              WHERE product_id = $1",
            &[&product_id],
        )?
        .is_some()
    {
        reconcile(&mut transaction, &product_id, product)?;
        transaction.commit()?;
        return Ok(ZapDistinctPublishOutcome::AlreadyPublished { product_id });
    }

    transaction.execute(
        "INSERT INTO pensieve_analytics.semantic_zap_distinct_products (
             product_id, semantic_product_id, complete_through_epoch,
             product_version, evidence_sha256, identity_artifact_sha256,
             physical_identities, logical_identities, duplicate_identities,
             leaf_rows, sketch_bytes, max_leaf_bytes, published_at
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,now())",
        &[
            &product_id,
            &semantic_product_id,
            &to_i64("zap distinct complete-through", complete_through)?,
            &ZAP_DISTINCT_VERSION,
            &product.evidence_sha256,
            &product.evidence.identity_artifact.sha256,
            &to_i64(
                "zap distinct physical identities",
                product.evidence.physical_identities,
            )?,
            &to_i64(
                "zap distinct logical identities",
                product.evidence.logical_identities,
            )?,
            &to_i64(
                "zap distinct duplicate identities",
                product.evidence.duplicate_identities,
            )?,
            &to_i64(
                "zap distinct leaf rows",
                product.evidence.leaves.len() as u64,
            )?,
            &to_i64("zap distinct sketch bytes", sketch_bytes)?,
            &to_i64(
                "zap distinct max leaf bytes",
                product.evidence.max_leaf_bytes as u64,
            )?,
        ],
    )?;
    copy_leaves(&mut transaction, &product_id, product)?;
    reconcile(&mut transaction, &product_id, product)?;
    transaction.commit()?;
    Ok(ZapDistinctPublishOutcome::Published { product_id })
}

/// Estimate unique validated participants for one complete UTC-day window.
pub fn estimate_published_zap_distinct(
    client: &mut impl GenericClient,
    product_id: &str,
    since_epoch: u64,
    until_epoch: u64,
    role: ZapParticipantRole,
) -> Result<u64> {
    let complete_through = from_i64(
        "zap distinct complete-through",
        client
            .query_one(
                "SELECT complete_through_epoch
                   FROM pensieve_analytics.semantic_zap_distinct_products
                  WHERE product_id = $1",
                &[&product_id],
            )?
            .get(0),
    )?;
    validate_window(since_epoch, until_epoch, complete_through)?;
    let role = role_code(role);
    let mut union = DistinctSketchUnion::new();
    let mut rows = client.query_raw(
        "SELECT sketch
           FROM pensieve_analytics.semantic_zap_distinct_leaves
          WHERE product_id = $1 AND role = $2
            AND day_epoch >= $3 AND day_epoch < $4
          ORDER BY day_epoch",
        [
            &product_id as &(dyn postgres::types::ToSql + Sync),
            &role,
            &to_i64("zap distinct since", since_epoch)?,
            &to_i64("zap distinct until", until_epoch)?,
        ],
    )?;
    while let Some(row) = rows.next()? {
        let sketch: Vec<u8> = row.get(0);
        union.push_serialized(&sketch).map_err(|error| {
            Error::Validation(format!("decode published zap distinct leaf: {error}"))
        })?;
    }
    Ok(union.finish().estimate())
}

fn copy_leaves(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedZapDistinct,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "COPY pensieve_analytics.semantic_zap_distinct_leaves
         (product_id,day_epoch,role,exact_identities,estimated_identities,
          relative_error_ppm,sketch) FROM STDIN WITH (FORMAT csv)",
    )?;
    for leaf in &product.evidence.leaves {
        writeln!(
            writer,
            "{product_id},{},{},{},{},{},\\x{}",
            leaf.day_epoch,
            role_code(leaf.role),
            leaf.exact_identities,
            leaf.estimated_identities,
            leaf.relative_error_ppm,
            hex::encode(&leaf.sketch),
        )?;
    }
    let inserted = writer.finish()?;
    if inserted != product.evidence.leaves.len() as u64 {
        return Err(Error::Validation(format!(
            "Postgres copied {inserted} zap distinct leaves, expected {}",
            product.evidence.leaves.len()
        )));
    }
    Ok(())
}

fn reconcile(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedZapDistinct,
) -> Result<()> {
    let row = transaction.query_one(
        "SELECT products.product_version, products.evidence_sha256,
                products.identity_artifact_sha256, products.physical_identities,
                products.logical_identities, products.duplicate_identities,
                products.leaf_rows, products.sketch_bytes, products.max_leaf_bytes,
                count(leaves.day_epoch)::BIGINT,
                coalesce(sum(octet_length(leaves.sketch)),0)::BIGINT,
                coalesce(max(octet_length(leaves.sketch)),0)::BIGINT,
                coalesce(sum(leaves.exact_identities),0)::BIGINT
           FROM pensieve_analytics.semantic_zap_distinct_products products
           LEFT JOIN pensieve_analytics.semantic_zap_distinct_leaves leaves USING (product_id)
          WHERE products.product_id = $1
          GROUP BY products.product_id",
        &[&product_id],
    )?;
    let sketch_bytes = product
        .evidence
        .leaves
        .iter()
        .try_fold(0_u64, |sum, leaf| sum.checked_add(leaf.sketch.len() as u64))
        .ok_or_else(|| Error::Validation("zap distinct sketch bytes overflowed".to_owned()))?;
    if row.get::<_, String>(0) != ZAP_DISTINCT_VERSION
        || row.get::<_, String>(1) != product.evidence_sha256
        || row.get::<_, String>(2) != product.evidence.identity_artifact.sha256
        || from_i64("published zap physical", row.get(3))? != product.evidence.physical_identities
        || from_i64("published zap logical", row.get(4))? != product.evidence.logical_identities
        || from_i64("published zap duplicates", row.get(5))?
            != product.evidence.duplicate_identities
        || from_i64("published zap leaf metadata", row.get(6))?
            != product.evidence.leaves.len() as u64
        || from_i64("published zap sketch metadata", row.get(7))? != sketch_bytes
        || from_i64("published zap max metadata", row.get(8))?
            != product.evidence.max_leaf_bytes as u64
        || from_i64("published zap leaf rows", row.get(9))? != product.evidence.leaves.len() as u64
        || from_i64("published zap sketch bytes", row.get(10))? != sketch_bytes
        || from_i64("published zap max bytes", row.get(11))?
            != product.evidence.max_leaf_bytes as u64
        || from_i64("published zap exact identities", row.get(12))?
            != product.evidence.logical_identities
    {
        return Err(Error::Validation(
            "published zap distinct product does not reconcile".to_owned(),
        ));
    }
    Ok(())
}

fn validate_source(semantic: &BoundedSemanticFacts, product: &BoundedZapDistinct) -> Result<()> {
    product.validate_for_publication(semantic)?;
    if product.evidence.status != "completed"
        || product.evidence.snapshot_id != semantic.evidence.snapshot_id
        || product.evidence.as_of_epoch != semantic.evidence.as_of_epoch
        || product.evidence.semantic_evidence_sha256 != semantic.evidence_sha256
        || product.evidence.semantic_artifact_sha256 != semantic.evidence.final_artifact.sha256
    {
        return Err(Error::Validation(
            "zap distinct product does not belong to semantic source".to_owned(),
        ));
    }
    Ok(())
}

fn current_run_id(transaction: &mut impl GenericClient) -> Result<String> {
    Ok(transaction
        .query_one(
            "SELECT run_id FROM pensieve_analytics.current_run WHERE singleton=true",
            &[],
        )?
        .get(0))
}

fn zap_distinct_product_id(semantic_product_id: &str, product: &BoundedZapDistinct) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pensieve-zap-distinct-product-v1\0");
    digest.update(semantic_product_id.as_bytes());
    digest.update([0]);
    digest.update(product.evidence_sha256.as_bytes());
    hex::encode(digest.finalize())
}

fn validate_window(since: u64, until: u64, complete_through: u64) -> Result<()> {
    if !since.is_multiple_of(SECONDS_PER_DAY)
        || !until.is_multiple_of(SECONDS_PER_DAY)
        || since >= until
        || until > complete_through
    {
        return Err(Error::Validation(
            "zap distinct window must be non-empty, complete, and UTC-day aligned".to_owned(),
        ));
    }
    Ok(())
}

fn floor_day(epoch: u64) -> u64 {
    epoch - epoch % SECONDS_PER_DAY
}

fn role_code(role: ZapParticipantRole) -> i16 {
    match role {
        ZapParticipantRole::Sender => 0,
        ZapParticipantRole::Recipient => 1,
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
    use super::*;

    #[test]
    fn published_windows_are_complete_day_aligned_and_nonempty() {
        assert!(validate_window(86_400, 172_800, 259_200).is_ok());
        assert!(validate_window(1, 172_800, 259_200).is_err());
        assert!(validate_window(86_400, 86_400, 259_200).is_err());
        assert!(validate_window(172_800, 86_400, 259_200).is_err());
        assert!(validate_window(86_400, 345_600, 259_200).is_err());
    }
}
