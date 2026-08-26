//! Exact bounded-memory weekly and monthly cohort retention.
//!
//! Slice 5 joins the sorted pubkey-first-seen artifact from Slice 3 with the
//! sorted fixed-activity artifact from Slice 4. The join holds one pubkey and
//! the last emitted week/month in memory. The compact matrix is explicitly
//! capped by configuration so memory cannot grow with event or pubkey
//! cardinality.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use pensieve_core::NOSTR_GENESIS_TIMESTAMP;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ArtifactIdentity, BoundedExecutionError, BoundedFixedActivity, BoundedPubkeyFirstSeen,
    FIXED_ACTIVITY_RECORD_BYTES, PUBKEY_FIRST_SEEN_BYTES, Result, publish_canonical_json,
};

const RUNNER_VERSION: &str = "pensieve-analytics-cohort-retention-v1";
const PROFILE_EXCLUDED_KIND: u16 = 445;
const WRAPPED_EVENT_EXCLUDED_KIND: u16 = 1059;

/// One exact cohort/activity-period count.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CohortRetentionPeriod {
    /// `week` or `month`.
    pub grain: String,
    /// ISO-8601 UTC cohort start.
    pub cohort_start: String,
    /// ISO-8601 UTC activity-period start.
    pub activity_period: String,
    /// Exact pubkeys active in this cohort and activity period.
    pub active_pubkeys: u64,
}

/// Immutable completion evidence for exact cohort retention.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CohortRetentionEvidence {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner identity.
    pub runner_version: String,
    /// Completion status.
    pub status: String,
    /// Frozen catalog identity shared by both inputs.
    pub snapshot_id: String,
    /// Fixed upper time boundary shared by both inputs.
    pub as_of_epoch: u64,
    /// SHA-256 of the validated first-seen evidence.
    pub identity_evidence_sha256: String,
    /// SHA-256 of the validated fixed-activity evidence.
    pub activity_evidence_sha256: String,
    /// First-seen artifact consumed by the streaming join.
    pub first_seen_artifact: ArtifactIdentity,
    /// Fixed-activity artifact consumed by the streaming join.
    pub activity_artifact: ArtifactIdentity,
    /// Compact exact serving matrix.
    pub periods: Vec<CohortRetentionPeriod>,
    /// Number of compact serving rows.
    pub period_rows: u64,
    /// Sum of exact active-pubkey counts across all matrix cells.
    pub active_pubkeys_sum: u64,
    /// SHA-256 of canonical compact serving rows.
    pub metric_sha256: String,
    /// Configured hard ceiling for compact matrix rows.
    pub matrix_row_limit: usize,
    /// Maximum per-pubkey activity-period identities held simultaneously.
    pub max_pubkey_periods_buffered: usize,
}

/// Completed exact cohort-retention product.
#[derive(Clone, Debug)]
pub struct BoundedCohortRetention {
    /// Validated completion evidence.
    pub evidence: CohortRetentionEvidence,
    /// SHA-256 of canonical evidence JSON.
    pub evidence_sha256: String,
}

/// Build exact cohort retention from matching Slice 3 and Slice 4 state.
pub fn build_bounded_cohort_retention(
    evidence_path: impl AsRef<Path>,
    identity: &BoundedPubkeyFirstSeen,
    activity: &BoundedFixedActivity,
    matrix_row_limit: usize,
) -> Result<BoundedCohortRetention> {
    validate_inputs(identity, activity, matrix_row_limit)?;
    let finalized = compute_retention(
        Path::new(&identity.evidence.final_artifact.path),
        &identity.evidence.final_artifact,
        Path::new(&activity.evidence.activity_artifact.path),
        &activity.evidence.activity_artifact,
        identity.evidence.as_of_epoch,
        matrix_row_limit,
    )?;
    let evidence = CohortRetentionEvidence {
        schema_version: 1,
        runner_version: RUNNER_VERSION.to_owned(),
        status: "completed".to_owned(),
        snapshot_id: identity.evidence.snapshot_id.clone(),
        as_of_epoch: identity.evidence.as_of_epoch,
        identity_evidence_sha256: identity.evidence_sha256.clone(),
        activity_evidence_sha256: activity.evidence_sha256.clone(),
        first_seen_artifact: identity.evidence.final_artifact.clone(),
        activity_artifact: activity.evidence.activity_artifact.clone(),
        period_rows: to_u64(finalized.periods.len())?,
        active_pubkeys_sum: finalized.active_pubkeys_sum,
        metric_sha256: metric_sha256(&finalized.periods)?,
        periods: finalized.periods,
        matrix_row_limit,
        max_pubkey_periods_buffered: finalized.max_pubkey_periods_buffered,
    };
    let evidence_path = evidence_path.as_ref();
    publish_canonical_json(evidence_path, &evidence)?;
    Ok(BoundedCohortRetention {
        evidence,
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
    })
}

/// Load and fully revalidate completed cohort-retention evidence.
pub fn load_bounded_cohort_retention(
    evidence_path: impl AsRef<Path>,
) -> Result<BoundedCohortRetention> {
    let evidence_path = evidence_path.as_ref();
    let evidence: CohortRetentionEvidence = serde_json::from_slice(&fs::read(evidence_path)?)
        .map_err(|error| {
            BoundedExecutionError::Invalid(format!("decode cohort-retention evidence: {error}"))
        })?;
    let completed = BoundedCohortRetention {
        evidence_sha256: pensieve_lake::sha256_file(evidence_path)?,
        evidence,
    };
    completed.validate_for_publication()?;
    Ok(completed)
}

impl BoundedCohortRetention {
    /// Re-read immutable inputs and prove the compact matrix before publication.
    pub fn validate_for_publication(&self) -> Result<()> {
        let evidence = &self.evidence;
        if evidence.schema_version != 1
            || evidence.runner_version != RUNNER_VERSION
            || evidence.status != "completed"
            || evidence.matrix_row_limit == 0
            || evidence.period_rows != to_u64(evidence.periods.len())?
            || evidence.periods.len() > evidence.matrix_row_limit
        {
            return Err(BoundedExecutionError::Invalid(
                "cohort-retention evidence is not a completed bounded product".to_owned(),
            )
            .into());
        }
        let finalized = compute_retention(
            Path::new(&evidence.first_seen_artifact.path),
            &evidence.first_seen_artifact,
            Path::new(&evidence.activity_artifact.path),
            &evidence.activity_artifact,
            evidence.as_of_epoch,
            evidence.matrix_row_limit,
        )?;
        if finalized.periods != evidence.periods
            || finalized.active_pubkeys_sum != evidence.active_pubkeys_sum
            || finalized.max_pubkey_periods_buffered != evidence.max_pubkey_periods_buffered
            || metric_sha256(&finalized.periods)? != evidence.metric_sha256
        {
            return Err(BoundedExecutionError::Invalid(
                "cohort-retention metrics do not match immutable inputs".to_owned(),
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FinalizedRetention {
    periods: Vec<CohortRetentionPeriod>,
    active_pubkeys_sum: u64,
    max_pubkey_periods_buffered: usize,
}

fn compute_retention(
    first_seen_path: &Path,
    first_seen_artifact: &ArtifactIdentity,
    activity_path: &Path,
    activity_artifact: &ArtifactIdentity,
    as_of_epoch: u64,
    matrix_row_limit: usize,
) -> Result<FinalizedRetention> {
    validate_artifact(
        first_seen_path,
        first_seen_artifact,
        PUBKEY_FIRST_SEEN_BYTES,
    )?;
    validate_artifact(
        activity_path,
        activity_artifact,
        FIXED_ACTIVITY_RECORD_BYTES,
    )?;
    if matrix_row_limit == 0 {
        return Err(BoundedExecutionError::Invalid(
            "cohort matrix row limit must be positive".to_owned(),
        )
        .into());
    }

    let mut first_seen = FirstSeenReader::open(first_seen_path)?;
    let mut activity = ActivityReader::open(activity_path)?;
    let mut next_activity = activity.next()?;
    let mut matrix = BTreeMap::<(u8, u32, u32), u64>::new();
    let mut active_pubkeys_sum = 0_u64;
    let mut max_pubkey_periods_buffered = 0_usize;

    while let Some(first) = first_seen.next()? {
        while next_activity
            .as_ref()
            .is_some_and(|row| row.pubkey < first.pubkey)
        {
            consume_unmatched_activity_pubkey(&mut activity, &mut next_activity, as_of_epoch)?;
        }
        if first.first_seen < u64::from(NOSTR_GENESIS_TIMESTAMP) || first.first_seen > as_of_epoch {
            while next_activity
                .as_ref()
                .is_some_and(|row| row.pubkey == first.pubkey)
            {
                next_activity = activity.next()?;
            }
            continue;
        }
        let first_day = day_from_epoch(first.first_seen)?;
        let cohort_week = week_start(first_day);
        let cohort_month = month_start(first_day)?;
        let mut last_week = None;
        let mut last_month = None;
        while let Some(row) = next_activity.as_ref() {
            if row.pubkey != first.pubkey {
                break;
            }
            let row = *row;
            next_activity = activity.next()?;
            if u64::from(row.created_at) < first.first_seen
                || u64::from(row.created_at) > as_of_epoch
                || matches!(
                    row.kind,
                    PROFILE_EXCLUDED_KIND | WRAPPED_EVENT_EXCLUDED_KIND
                )
            {
                continue;
            }
            let day = day_from_epoch(u64::from(row.created_at))?;
            let week = week_start(day);
            if last_week != Some(week) {
                increment_matrix(&mut matrix, (0, cohort_week, week), matrix_row_limit)?;
                active_pubkeys_sum = checked_add(active_pubkeys_sum, 1, "cohort active sum")?;
                last_week = Some(week);
            }
            let month = month_start(day)?;
            if last_month != Some(month) {
                increment_matrix(&mut matrix, (1, cohort_month, month), matrix_row_limit)?;
                active_pubkeys_sum = checked_add(active_pubkeys_sum, 1, "cohort active sum")?;
                last_month = Some(month);
            }
            max_pubkey_periods_buffered = max_pubkey_periods_buffered.max(2);
        }
    }
    while next_activity.is_some() {
        consume_unmatched_activity_pubkey(&mut activity, &mut next_activity, as_of_epoch)?;
    }
    let periods = matrix
        .into_iter()
        .map(|((grain, cohort, period), active_pubkeys)| {
            Ok(CohortRetentionPeriod {
                grain: match grain {
                    0 => "week",
                    1 => "month",
                    _ => unreachable!("matrix only stores supported grains"),
                }
                .to_owned(),
                cohort_start: day_string(cohort)?,
                activity_period: day_string(period)?,
                active_pubkeys,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(FinalizedRetention {
        periods,
        active_pubkeys_sum,
        max_pubkey_periods_buffered,
    })
}

fn consume_unmatched_activity_pubkey(
    activity: &mut ActivityReader,
    next_activity: &mut Option<ActivityRecord>,
    as_of_epoch: u64,
) -> Result<()> {
    let pubkey = next_activity
        .as_ref()
        .expect("caller only consumes present activity")
        .pubkey;
    while let Some(row) = next_activity.as_ref() {
        if row.pubkey != pubkey {
            break;
        }
        if u64::from(row.created_at) <= as_of_epoch
            && !matches!(
                row.kind,
                PROFILE_EXCLUDED_KIND | WRAPPED_EVENT_EXCLUDED_KIND
            )
        {
            return Err(BoundedExecutionError::Invalid(
                "eligible activity pubkey is absent from first-seen state".to_owned(),
            )
            .into());
        }
        *next_activity = activity.next()?;
    }
    Ok(())
}

fn validate_inputs(
    identity: &BoundedPubkeyFirstSeen,
    activity: &BoundedFixedActivity,
    matrix_row_limit: usize,
) -> Result<()> {
    if identity.evidence.snapshot_id != activity.evidence.snapshot_id
        || identity.evidence.as_of_epoch != activity.evidence.as_of_epoch
        || matrix_row_limit == 0
    {
        return Err(BoundedExecutionError::Invalid(
            "cohort retention requires matching identity/activity inputs and a positive row limit"
                .to_owned(),
        )
        .into());
    }
    Ok(())
}

fn increment_matrix(
    matrix: &mut BTreeMap<(u8, u32, u32), u64>,
    key: (u8, u32, u32),
    matrix_row_limit: usize,
) -> Result<()> {
    if let Some(value) = matrix.get_mut(&key) {
        *value = checked_add(*value, 1, "cohort matrix cell")?;
        return Ok(());
    }
    if matrix.len() == matrix_row_limit {
        return Err(BoundedExecutionError::Invalid(format!(
            "cohort matrix exceeds configured {matrix_row_limit}-row bound"
        ))
        .into());
    }
    matrix.insert(key, 1);
    Ok(())
}

fn validate_artifact(path: &Path, artifact: &ArtifactIdentity, record_bytes: usize) -> Result<()> {
    let metadata = path.metadata()?;
    if !metadata.is_file()
        || metadata.len() != artifact.byte_size
        || artifact.byte_size != artifact.row_count.saturating_mul(record_bytes as u64)
        || pensieve_lake::sha256_file(path)? != artifact.sha256
    {
        return Err(BoundedExecutionError::Invalid(
            "cohort-retention input artifact identity mismatch".to_owned(),
        )
        .into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct FirstSeenRecord {
    pubkey: [u8; 32],
    first_seen: u64,
}

struct FirstSeenReader {
    reader: BufReader<File>,
}

impl FirstSeenReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
        })
    }

    fn next(&mut self) -> Result<Option<FirstSeenRecord>> {
        let mut bytes = [0_u8; PUBKEY_FIRST_SEEN_BYTES];
        if !read_exact_or_eof(&mut self.reader, &mut bytes)? {
            return Ok(None);
        }
        Ok(Some(FirstSeenRecord {
            pubkey: bytes[..32].try_into().expect("fixed first-seen pubkey"),
            first_seen: u64::from_be_bytes(bytes[32..].try_into().expect("fixed timestamp")),
        }))
    }
}

#[derive(Clone, Copy)]
struct ActivityRecord {
    pubkey: [u8; 32],
    created_at: u32,
    kind: u16,
}

struct ActivityReader {
    reader: BufReader<File>,
}

impl ActivityReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
        })
    }

    fn next(&mut self) -> Result<Option<ActivityRecord>> {
        let mut bytes = [0_u8; FIXED_ACTIVITY_RECORD_BYTES];
        if !read_exact_or_eof(&mut self.reader, &mut bytes)? {
            return Ok(None);
        }
        Ok(Some(ActivityRecord {
            pubkey: bytes[..32].try_into().expect("fixed activity pubkey"),
            created_at: u32::from_be_bytes(bytes[32..36].try_into().expect("fixed timestamp")),
            kind: u16::from_be_bytes(bytes[36..38].try_into().expect("fixed kind")),
        }))
    }
}

fn read_exact_or_eof(reader: &mut impl Read, bytes: &mut [u8]) -> Result<bool> {
    let mut offset = 0;
    while offset < bytes.len() {
        let read = reader.read(&mut bytes[offset..])?;
        if read == 0 {
            if offset == 0 {
                return Ok(false);
            }
            return Err(BoundedExecutionError::Invalid(
                "cohort-retention input ends with a truncated record".to_owned(),
            )
            .into());
        }
        offset += read;
    }
    Ok(true)
}

fn day_from_epoch(epoch: u64) -> Result<u32> {
    u32::try_from(epoch / 86_400).map_err(|_| {
        BoundedExecutionError::Invalid("cohort timestamp exceeds supported day range".to_owned())
            .into()
    })
}

fn week_start(day: u32) -> u32 {
    day - ((day + 3) % 7)
}

fn month_start(day: u32) -> Result<u32> {
    let date = day_date(day)?;
    let first = NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
        .ok_or_else(|| BoundedExecutionError::Invalid("invalid cohort month".to_owned()))?;
    u32::try_from(
        first
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| BoundedExecutionError::Invalid("invalid cohort timestamp".to_owned()))?
            .and_utc()
            .timestamp()
            / 86_400,
    )
    .map_err(|_| BoundedExecutionError::Invalid("cohort day exceeds u32".to_owned()).into())
}

fn day_date(day: u32) -> Result<NaiveDate> {
    DateTime::<Utc>::from_timestamp(i64::from(day) * 86_400, 0)
        .ok_or_else(|| BoundedExecutionError::Invalid("invalid UTC cohort day".to_owned()).into())
        .map(|value| value.date_naive())
}

fn day_string(day: u32) -> Result<String> {
    Ok(day_date(day)?.to_string())
}

fn metric_sha256(periods: &[CohortRetentionPeriod]) -> Result<String> {
    let mut bytes = serde_json::to_vec_pretty(periods).map_err(|error| {
        BoundedExecutionError::Invalid(format!("serialize cohort-retention metrics: {error}"))
    })?;
    bytes.push(b'\n');
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| BoundedExecutionError::Invalid(format!("{label} overflowed u64")).into())
}

fn to_u64(value: usize) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BoundedExecutionError::Invalid("count exceeds u64".to_owned()).into())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn streaming_join_counts_each_pubkey_once_per_period() {
        let directory = tempdir().expect("temporary directory");
        let first_seen_path = directory.path().join("first-seen.run");
        let activity_path = directory.path().join("activity.run");
        let monday = 1_699_833_600_u64;
        write_first_seen(&first_seen_path, &[(1, monday + 1), (2, monday + 2)]);
        write_activity(
            &activity_path,
            &[
                (1, monday + 1, 1),
                (1, monday + 2, 1),
                (1, monday + 7 * 86_400 + 1, 1),
                (2, monday + 2, 1),
                (2, monday + 3, 445),
            ],
        );
        let first_seen = artifact(&first_seen_path, 2);
        let activity = artifact(&activity_path, 5);
        let result = compute_retention(
            &first_seen_path,
            &first_seen,
            &activity_path,
            &activity,
            monday + 8 * 86_400,
            32,
        )
        .expect("compute retention");
        let weekly = result
            .periods
            .iter()
            .filter(|row| row.grain == "week")
            .collect::<Vec<_>>();
        assert_eq!(weekly.len(), 2);
        assert_eq!(weekly[0].active_pubkeys, 2);
        assert_eq!(weekly[1].active_pubkeys, 1);
        assert_eq!(result.max_pubkey_periods_buffered, 2);
    }

    #[test]
    fn late_first_seen_moves_the_cohort_without_unbounded_state() {
        let directory = tempdir().expect("temporary directory");
        let first_seen_path = directory.path().join("first-seen.run");
        let activity_path = directory.path().join("activity.run");
        let monday = 1_699_833_600_u64;
        write_first_seen(&first_seen_path, &[(1, monday - 7 * 86_400 + 1)]);
        write_activity(
            &activity_path,
            &[(1, monday - 7 * 86_400 + 1, 1), (1, monday + 1, 1)],
        );
        let result = compute_retention(
            &first_seen_path,
            &artifact(&first_seen_path, 1),
            &activity_path,
            &artifact(&activity_path, 2),
            monday + 1,
            8,
        )
        .expect("compute retention");
        let weekly = result
            .periods
            .iter()
            .filter(|row| row.grain == "week")
            .collect::<Vec<_>>();
        assert_eq!(weekly.len(), 2);
        assert!(weekly.iter().all(|row| row.cohort_start == "2023-11-06"));
    }

    #[test]
    fn compact_matrix_fails_at_the_configured_bound() {
        let directory = tempdir().expect("temporary directory");
        let first_seen_path = directory.path().join("first-seen.run");
        let activity_path = directory.path().join("activity.run");
        let monday = 1_699_833_600_u64;
        write_first_seen(&first_seen_path, &[(1, monday + 1)]);
        write_activity(&activity_path, &[(1, monday + 1, 1)]);
        let error = compute_retention(
            &first_seen_path,
            &artifact(&first_seen_path, 1),
            &activity_path,
            &artifact(&activity_path, 1),
            monday + 1,
            1,
        )
        .expect_err("week and month rows exceed one-cell cap");
        assert!(error.to_string().contains("matrix exceeds"));
    }

    #[test]
    fn excluded_only_activity_does_not_create_a_cohort() {
        let directory = tempdir().expect("temporary directory");
        let first_seen_path = directory.path().join("first-seen.run");
        let activity_path = directory.path().join("activity.run");
        let monday = 1_699_833_600_u64;
        write_first_seen(&first_seen_path, &[(2, monday + 2)]);
        write_activity(&activity_path, &[(1, monday + 1, 445), (2, monday + 2, 1)]);
        let result = compute_retention(
            &first_seen_path,
            &artifact(&first_seen_path, 1),
            &activity_path,
            &artifact(&activity_path, 2),
            monday + 2,
            8,
        )
        .expect("compute retention");
        assert_eq!(result.periods.len(), 2);
        assert!(result.periods.iter().all(|row| row.active_pubkeys == 1));
    }

    fn write_first_seen(path: &Path, rows: &[(u8, u64)]) {
        let mut file = File::create(path).expect("create first-seen run");
        for (pubkey, first_seen) in rows {
            file.write_all(&[*pubkey; 32]).expect("write pubkey");
            file.write_all(&first_seen.to_be_bytes())
                .expect("write timestamp");
        }
    }

    fn write_activity(path: &Path, rows: &[(u8, u64, u16)]) {
        let mut file = File::create(path).expect("create activity run");
        for (index, (pubkey, created_at, kind)) in rows.iter().enumerate() {
            file.write_all(&[*pubkey; 32]).expect("write pubkey");
            file.write_all(
                &u32::try_from(*created_at)
                    .expect("u32 timestamp")
                    .to_be_bytes(),
            )
            .expect("write timestamp");
            file.write_all(&kind.to_be_bytes()).expect("write kind");
            file.write_all(&[u8::try_from(index).expect("small index"); 32])
                .expect("write event ID");
        }
    }

    fn artifact(path: &Path, rows: u64) -> ArtifactIdentity {
        ArtifactIdentity {
            path: PathBuf::from(path).to_string_lossy().into_owned(),
            byte_size: fs::metadata(path).expect("artifact metadata").len(),
            row_count: rows,
            min_key: None,
            max_key: None,
            sha256: pensieve_lake::sha256_file(path).expect("artifact SHA-256"),
        }
    }
}
