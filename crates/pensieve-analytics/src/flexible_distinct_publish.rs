//! Transactional publication and fixed-memory querying for Slice 6 HLL leaves.
//!
//! Publication is deliberately dormant: it binds a versioned leaf product to
//! the exact current B3 run, but never changes `current_run` and exposes no
//! current-product view. A separate API gate must authorize serving it.

use std::fs;
use std::io::Write;
use std::path::Path;

use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, GenericClient};
use sha2::{Digest, Sha256};

use crate::schema::SCHEMA_SQL;
use crate::{
    BoundedFlexibleDistinct, COHORT_RETENTION_QUERY_VERSION, DistinctSketchUnion, Error,
    FLEXIBLE_DISTINCT_VERSION, FlexibleDistinctValidationEvidence, Result,
    visit_flexible_distinct_leaves,
};

const PUBLICATION_LOCK_ID: i64 = 0x5045_4e53_4945_5645;
const VALIDATION_RUNNER: &str = "pensieve-analytics-flexible-distinct-validation-v1";
const SECONDS_PER_HOUR: u64 = 3_600;
const MAX_ACCEPTED_TOLERANCE_PPM: u64 = 20_000;

/// Result of atomically publishing one dormant Slice 6 product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlexibleDistinctPublishOutcome {
    /// A new versioned product and all of its leaves committed.
    Published { product_id: String },
    /// An identical previously committed product reconciled successfully.
    AlreadyPublished { product_id: String },
}

/// Fully checked tolerance evidence ready for one atomic publication.
pub(crate) struct ValidatedFlexibleDistinctPublication {
    validation_sha256: String,
}

/// Publish validated leaves without changing the analytics current-run pointer.
pub fn publish_flexible_distinct_leaves(
    client: &mut Client,
    baseline_run_id: &str,
    product: &BoundedFlexibleDistinct,
    validation_evidence_path: impl AsRef<Path>,
    expected_validation_sha256: &str,
) -> Result<FlexibleDistinctPublishOutcome> {
    let validated = validate_flexible_distinct_publication(
        product,
        validation_evidence_path,
        expected_validation_sha256,
    )?;

    client.batch_execute(SCHEMA_SQL)?;
    let mut transaction = client.transaction()?;
    transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&PUBLICATION_LOCK_ID])?;
    let current = transaction.query_one(
        "SELECT run_id, snapshot_id, query_version, as_of_epoch
           FROM pensieve_analytics.current_run_metadata FOR SHARE",
        &[],
    )?;
    let current_run_id: String = current.get(0);
    let current_snapshot: String = current.get(1);
    let current_query_version: String = current.get(2);
    let current_as_of: i64 = current.get(3);
    if current_run_id != baseline_run_id
        || current_snapshot != product.evidence.snapshot_id
        || current_query_version != COHORT_RETENTION_QUERY_VERSION
        || current_as_of != to_i64("flexible as_of_epoch", product.evidence.as_of_epoch)?
    {
        return Err(Error::Validation(
            "current Postgres run is not the exact corrected B3 Slice 6 baseline".to_owned(),
        ));
    }

    let outcome = publish_flexible_distinct_leaves_in_transaction(
        &mut transaction,
        baseline_run_id,
        product,
        &validated,
    )?;
    transaction.commit()?;
    Ok(outcome)
}

pub(crate) fn validate_flexible_distinct_publication(
    product: &BoundedFlexibleDistinct,
    validation_evidence_path: impl AsRef<Path>,
    expected_validation_sha256: &str,
) -> Result<ValidatedFlexibleDistinctPublication> {
    product
        .validate_for_publication(&product.evidence.snapshot_id, product.evidence.as_of_epoch)?;
    let validation_path = validation_evidence_path.as_ref();
    let validation: FlexibleDistinctValidationEvidence =
        serde_json::from_slice(&fs::read(validation_path)?).map_err(|error| {
            Error::Validation(format!(
                "decode flexible-distinct validation evidence: {error}"
            ))
        })?;
    let validation_sha256 = pensieve_lake::sha256_file(validation_path)?;
    if validation_sha256 != expected_validation_sha256 {
        return Err(Error::Validation(
            "flexible-distinct validation evidence SHA-256 differs from the authorized gate"
                .to_owned(),
        ));
    }
    validate_tolerance_evidence(product, &validation)?;
    Ok(ValidatedFlexibleDistinctPublication { validation_sha256 })
}

pub(crate) fn publish_flexible_distinct_leaves_in_transaction(
    transaction: &mut impl GenericClient,
    baseline_run_id: &str,
    product: &BoundedFlexibleDistinct,
    validated: &ValidatedFlexibleDistinctPublication,
) -> Result<FlexibleDistinctPublishOutcome> {
    let product_id = flexible_product_id(baseline_run_id, product, &validated.validation_sha256);
    if transaction
        .query_opt(
            "SELECT product_id FROM pensieve_analytics.flexible_distinct_products
              WHERE product_id = $1",
            &[&product_id],
        )?
        .is_some()
    {
        reconcile_flexible_product(transaction, &product_id, product)?;
        return Ok(FlexibleDistinctPublishOutcome::AlreadyPublished { product_id });
    }

    let sketch_bytes = product
        .evidence
        .leaf_artifact
        .byte_size
        .checked_sub(product.evidence.leaf_artifact.row_count.saturating_mul(10))
        .ok_or_else(|| Error::Validation("flexible leaf byte accounting underflow".to_owned()))?;
    transaction.execute(
        "INSERT INTO pensieve_analytics.flexible_distinct_products (
             product_id, run_id, snapshot_id, as_of_epoch, complete_through_epoch,
             product_version, evidence_sha256, validation_evidence_sha256,
             leaf_artifact_sha256, leaf_rows, sketch_bytes, max_leaf_bytes, published_at
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,now())",
        &[
            &product_id,
            &baseline_run_id,
            &product.evidence.snapshot_id,
            &to_i64("flexible as_of_epoch", product.evidence.as_of_epoch)?,
            &to_i64(
                "flexible complete_through_epoch",
                product.evidence.complete_through_epoch,
            )?,
            &FLEXIBLE_DISTINCT_VERSION,
            &product.evidence_sha256,
            &validated.validation_sha256,
            &product.evidence.leaf_artifact.sha256,
            &to_i64(
                "flexible leaf rows",
                product.evidence.leaf_artifact.row_count,
            )?,
            &to_i64("flexible sketch bytes", sketch_bytes)?,
            &to_i64(
                "flexible max leaf bytes",
                product.evidence.max_leaf_bytes as u64,
            )?,
        ],
    )?;
    copy_flexible_leaves(transaction, &product_id, product)?;
    reconcile_flexible_product(transaction, &product_id, product)?;
    Ok(FlexibleDistinctPublishOutcome::Published { product_id })
}

/// Estimate one aligned window from a published product with fixed memory.
pub fn estimate_published_flexible_distinct(
    client: &mut impl GenericClient,
    product_id: &str,
    since_epoch: u64,
    until_epoch: u64,
    kind: Option<u16>,
) -> Result<u64> {
    let metadata = client.query_one(
        "SELECT complete_through_epoch
           FROM pensieve_analytics.flexible_distinct_products
          WHERE product_id = $1",
        &[&product_id],
    )?;
    let complete_through = from_i64("published complete-through", metadata.get(0))?;
    validate_window(since_epoch, until_epoch, complete_through)?;
    let kind_i32 = kind.map(i32::from);
    let mut union = DistinctSketchUnion::new();
    let mut rows = client.query_raw(
        "SELECT sketch
           FROM pensieve_analytics.flexible_distinct_leaves
          WHERE product_id = $1 AND hour_epoch >= $2 AND hour_epoch < $3
            AND ($4::INTEGER IS NULL OR kind = $4)
          ORDER BY hour_epoch, kind",
        [
            &product_id as &(dyn postgres::types::ToSql + Sync),
            &to_i64("flexible since_epoch", since_epoch)?,
            &to_i64("flexible until_epoch", until_epoch)?,
            &kind_i32,
        ],
    )?;
    while let Some(row) = rows.next()? {
        let sketch: Vec<u8> = row.get(0);
        union.push_serialized(&sketch).map_err(|error| {
            Error::Validation(format!("decode published flexible leaf: {error}"))
        })?;
    }
    Ok(union.finish().estimate())
}

fn copy_flexible_leaves(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedFlexibleDistinct,
) -> Result<()> {
    let mut writer = transaction.copy_in(
        "COPY pensieve_analytics.flexible_distinct_leaves
             (product_id, hour_epoch, kind, sketch)
         FROM STDIN WITH (FORMAT csv)",
    )?;
    let visited = visit_flexible_distinct_leaves(product, |hour, kind, sketch| {
        let hour_epoch = u64::from(hour)
            .checked_mul(SECONDS_PER_HOUR)
            .ok_or_else(|| Error::Validation("flexible leaf hour overflow".to_owned()))?;
        writeln!(
            writer,
            "{product_id},{hour_epoch},{kind},\\x{}",
            hex::encode(sketch)
        )?;
        Ok(())
    })?;
    let inserted = writer.finish()?;
    if inserted != visited || inserted != product.evidence.leaf_artifact.row_count {
        return Err(Error::Validation(format!(
            "Postgres copied {inserted} flexible leaves, expected {}",
            product.evidence.leaf_artifact.row_count
        )));
    }
    Ok(())
}

fn reconcile_flexible_product(
    transaction: &mut impl GenericClient,
    product_id: &str,
    product: &BoundedFlexibleDistinct,
) -> Result<()> {
    let row = transaction.query_one(
        "SELECT products.snapshot_id, products.as_of_epoch,
                products.complete_through_epoch, products.product_version,
                products.evidence_sha256, products.leaf_artifact_sha256,
                products.leaf_rows, products.sketch_bytes, products.max_leaf_bytes,
                count(leaves.hour_epoch)::BIGINT,
                coalesce(sum(octet_length(leaves.sketch)), 0)::BIGINT,
                coalesce(max(octet_length(leaves.sketch)), 0)::BIGINT
           FROM pensieve_analytics.flexible_distinct_products products
           LEFT JOIN pensieve_analytics.flexible_distinct_leaves leaves USING (product_id)
          WHERE products.product_id = $1
          GROUP BY products.product_id",
        &[&product_id],
    )?;
    let expected_sketch_bytes = product
        .evidence
        .leaf_artifact
        .byte_size
        .checked_sub(product.evidence.leaf_artifact.row_count.saturating_mul(10))
        .ok_or_else(|| Error::Validation("flexible leaf byte accounting underflow".to_owned()))?;
    if row.get::<_, String>(0) != product.evidence.snapshot_id
        || from_i64("published flexible as-of", row.get(1))? != product.evidence.as_of_epoch
        || from_i64("published flexible complete-through", row.get(2))?
            != product.evidence.complete_through_epoch
        || row.get::<_, String>(3) != FLEXIBLE_DISTINCT_VERSION
        || row.get::<_, String>(4) != product.evidence_sha256
        || row.get::<_, String>(5) != product.evidence.leaf_artifact.sha256
        || from_i64("published flexible metadata rows", row.get(6))?
            != product.evidence.leaf_artifact.row_count
        || from_i64("published flexible metadata bytes", row.get(7))? != expected_sketch_bytes
        || from_i64("published flexible metadata max leaf", row.get(8))?
            != product.evidence.max_leaf_bytes as u64
        || from_i64("published flexible leaf rows", row.get(9))?
            != product.evidence.leaf_artifact.row_count
        || from_i64("published flexible leaf bytes", row.get(10))? != expected_sketch_bytes
        || from_i64("published flexible max leaf", row.get(11))?
            != product.evidence.max_leaf_bytes as u64
    {
        return Err(Error::Validation(
            "published flexible-distinct product does not reconcile to immutable evidence"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_tolerance_evidence(
    product: &BoundedFlexibleDistinct,
    validation: &FlexibleDistinctValidationEvidence,
) -> Result<()> {
    let samples_match = validation.sample_count == validation.samples.len() as u64
        && validation.sample_count > 0
        && validation
            .samples
            .iter()
            .all(|sample| sample.accepted && sample.relative_error_ppm <= validation.tolerance_ppm)
        && validation.max_relative_error_ppm
            == validation
                .samples
                .iter()
                .map(|sample| sample.relative_error_ppm)
                .max()
                .unwrap_or(0);
    if validation.schema_version != 1
        || validation.runner_version != VALIDATION_RUNNER
        || validation.status != "passed"
        || validation.snapshot_id != product.evidence.snapshot_id
        || validation.as_of_epoch != product.evidence.as_of_epoch
        || validation.complete_through_epoch != product.evidence.complete_through_epoch
        || !is_sha256(&validation.activity_evidence_sha256)
        || validation.flexible_evidence_sha256 != product.evidence_sha256
        || validation.tolerance_ppm > MAX_ACCEPTED_TOLERANCE_PPM
        || validation.max_relative_error_ppm > validation.tolerance_ppm
        || !samples_match
    {
        return Err(Error::Validation(
            "flexible-distinct tolerance evidence is not a matching passed gate".to_owned(),
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn flexible_product_id(
    baseline_run_id: &str,
    product: &BoundedFlexibleDistinct,
    validation_sha256: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(baseline_run_id.as_bytes());
    digest.update([0]);
    digest.update(FLEXIBLE_DISTINCT_VERSION.as_bytes());
    digest.update([0]);
    digest.update(product.evidence_sha256.as_bytes());
    digest.update([0]);
    digest.update(validation_sha256.as_bytes());
    hex::encode(digest.finalize())
}

fn validate_window(since: u64, until: u64, complete_through: u64) -> Result<()> {
    if !since.is_multiple_of(SECONDS_PER_HOUR)
        || !until.is_multiple_of(SECONDS_PER_HOUR)
        || since > until
        || until > complete_through
    {
        return Err(Error::Validation(
            "published flexible distinct window is not a valid complete-hour interval".to_owned(),
        ));
    }
    Ok(())
}

fn to_i64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::NumericOverflow { field, value })
}

fn from_i64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Validation(format!("{field} is negative: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::{ArtifactIdentity, DistinctSketchBuilder, FlexibleDistinctEvidence};
    use tempfile::TempDir;

    #[test]
    fn published_windows_are_complete_hour_aligned_and_bounded() {
        assert!(validate_window(3_600, 7_200, 10_800).is_ok());
        assert!(validate_window(1, 7_200, 10_800).is_err());
        assert!(validate_window(7_200, 3_600, 10_800).is_err());
        assert!(validate_window(3_600, 14_400, 10_800).is_err());
    }

    #[test]
    fn sha256_identity_is_lowercase_and_exact_length() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"A".repeat(64)));
        assert!(!is_sha256(&"a".repeat(63)));
        assert!(!is_sha256(&"g".repeat(64)));
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
            .expect("reset isolated test database");
        client.batch_execute(SCHEMA_SQL).expect("apply schema");

        let temp = TempDir::new().expect("temporary artifacts");
        let product = synthetic_product(&temp);
        let baseline = "baseline-b3";
        client
            .execute(
                "INSERT INTO pensieve_analytics.runs (
                     run_id, snapshot_id, previous_run_id, run_kind, query_version,
                     code_version, as_of_epoch, started_at, completed_at, published_at,
                     physical_rows, logical_events, duplicate_rows,
                     api_representable_events, event_daily_rows, event_daily_kind_rows,
                     kind_all_time_rows, validation
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
                "INSERT INTO pensieve_analytics.current_run (singleton, run_id)
                 VALUES (true, $1)",
                &[&baseline],
            )
            .expect("set current B3 baseline");

        let validation_path = temp.path().join("validation.json");
        let validation = serde_json::json!({
            "schema_version": 1,
            "runner_version": VALIDATION_RUNNER,
            "status": "passed",
            "snapshot_id": product.evidence.snapshot_id,
            "as_of_epoch": product.evidence.as_of_epoch,
            "complete_through_epoch": product.evidence.complete_through_epoch,
            "activity_evidence_sha256": "a".repeat(64),
            "flexible_evidence_sha256": product.evidence_sha256,
            "tolerance_ppm": 20_000,
            "sample_count": 1,
            "max_absolute_error": 0,
            "max_relative_error_ppm": 0,
            "samples": [{"accepted": true, "relative_error_ppm": 0}]
        });
        fs::write(
            &validation_path,
            serde_json::to_vec(&validation).expect("encode validation"),
        )
        .expect("write validation");
        let validation_sha = pensieve_lake::sha256_file(&validation_path).expect("hash validation");

        client
            .batch_execute(
                "CREATE FUNCTION pensieve_analytics.reject_flexible_leaf()
                   RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN RAISE EXCEPTION 'injected leaf failure'; END $$;
                 CREATE TRIGGER reject_flexible_leaf
                   BEFORE INSERT ON pensieve_analytics.flexible_distinct_leaves
                   FOR EACH ROW EXECUTE FUNCTION pensieve_analytics.reject_flexible_leaf();",
            )
            .expect("install failure injection");
        assert!(
            publish_flexible_distinct_leaves(
                &mut client,
                baseline,
                &product,
                &validation_path,
                &validation_sha,
            )
            .is_err()
        );
        let product_rows: i64 = client
            .query_one(
                "SELECT count(*)::BIGINT
                   FROM pensieve_analytics.flexible_distinct_products",
                &[],
            )
            .expect("count rolled-back products")
            .get(0);
        assert_eq!(product_rows, 0);
        client
            .batch_execute(
                "DROP TRIGGER reject_flexible_leaf
                   ON pensieve_analytics.flexible_distinct_leaves;
                 DROP FUNCTION pensieve_analytics.reject_flexible_leaf();",
            )
            .expect("remove failure injection");

        let published = publish_flexible_distinct_leaves(
            &mut client,
            baseline,
            &product,
            &validation_path,
            &validation_sha,
        )
        .expect("publish leaves");
        let product_id = match published {
            FlexibleDistinctPublishOutcome::Published { product_id } => product_id,
            outcome => panic!("unexpected initial outcome: {outcome:?}"),
        };
        assert_eq!(
            estimate_published_flexible_distinct(&mut client, &product_id, 3_600, 7_200, None)
                .expect("estimate published window"),
            2
        );
        assert_eq!(
            publish_flexible_distinct_leaves(
                &mut client,
                baseline,
                &product,
                &validation_path,
                &validation_sha,
            )
            .expect("retry publication"),
            FlexibleDistinctPublishOutcome::AlreadyPublished {
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

    fn synthetic_product(temp: &TempDir) -> BoundedFlexibleDistinct {
        let activity = temp.path().join("activity.run");
        fs::write(&activity, []).expect("write empty activity");
        let identities = temp.path().join("identities.run");
        let mut identity_bytes = Vec::new();
        for pubkey in [[1_u8; 32], [2_u8; 32]] {
            identity_bytes.extend_from_slice(&1_u32.to_be_bytes());
            identity_bytes.extend_from_slice(&1_u16.to_be_bytes());
            identity_bytes.extend_from_slice(&pubkey);
        }
        fs::write(&identities, &identity_bytes).expect("write identities");
        let mut builder = DistinctSketchBuilder::new();
        builder.push([1_u8; 32]).expect("first pubkey");
        builder.push([2_u8; 32]).expect("second pubkey");
        let sketch = builder.finish().serialize();
        let leaves = temp.path().join("leaves.run");
        let mut leaf_file = fs::File::create(&leaves).expect("create leaves");
        leaf_file.write_all(&1_u32.to_be_bytes()).expect("hour");
        leaf_file.write_all(&1_u16.to_be_bytes()).expect("kind");
        leaf_file
            .write_all(&(sketch.len() as u32).to_be_bytes())
            .expect("length");
        leaf_file.write_all(&sketch).expect("sketch");
        leaf_file.sync_all().expect("sync leaves");
        let artifact = |path: &Path, row_count: u64, min_key, max_key| ArtifactIdentity {
            path: path.to_string_lossy().into_owned(),
            byte_size: path.metadata().expect("artifact metadata").len(),
            row_count,
            min_key,
            max_key,
            sha256: pensieve_lake::sha256_file(path).expect("artifact SHA"),
        };
        let activity_artifact = artifact(&activity, 0, None, None);
        let identity_artifact = artifact(
            &identities,
            2,
            Some(hex::encode(&identity_bytes[..38])),
            Some(hex::encode(&identity_bytes[38..76])),
        );
        let leaf_artifact = artifact(
            &leaves,
            1,
            Some(hex::encode([0_u8, 0, 0, 1, 0, 1])),
            Some(hex::encode([0_u8, 0, 0, 1, 0, 1])),
        );
        BoundedFlexibleDistinct {
            evidence: FlexibleDistinctEvidence {
                schema_version: 1,
                runner_version: "pensieve-analytics-flexible-distinct-v1".to_owned(),
                status: "completed".to_owned(),
                snapshot_id: "sha256:test-snapshot".to_owned(),
                as_of_epoch: 7_201,
                complete_through_epoch: 7_200,
                activity_evidence_sha256: "b".repeat(64),
                baseline_evidence_sha256: None,
                baseline_complete_through_epoch: None,
                incremental_activity_checkpoints: Vec::new(),
                activity_artifact,
                source_activity_rows: 0,
                batch_count: 0,
                merge_count: 0,
                identity_artifact,
                leaf_artifact,
                max_batch_buffered_bytes: 0,
                max_merge_buffered_bytes: 0,
                max_leaf_bytes: sketch.len(),
                estimated_run_bytes: 0,
                disk_reserve_bytes: 0,
                batch_checkpoints: Vec::new(),
                merge_checkpoints: Vec::new(),
                leaf_checkpoint: "synthetic".to_owned(),
            },
            evidence_sha256: "c".repeat(64),
        }
    }
}
