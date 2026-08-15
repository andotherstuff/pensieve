//! Durable primitives for memory-bounded analytics execution.
//!
//! This module owns immutable run identity and checkpoint publication. Product
//! lanes may only reuse a completed artifact after its canonical checkpoint,
//! exact inputs, byte size, and SHA-256 have all been revalidated.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Schema version for bounded-run checkpoint evidence.
pub const BOUNDED_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Implementation identity for the bounded analytics runner.
pub const BOUNDED_RUNNER_VERSION: &str = "pensieve-analytics-bounded-v1";

/// Errors raised while publishing or validating bounded analytics evidence.
#[derive(Debug, thiserror::Error)]
pub enum BoundedExecutionError {
    /// A filesystem operation failed.
    #[error("bounded analytics I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Checkpoint JSON could not be encoded or decoded.
    #[error("bounded analytics checkpoint JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Immutable evidence or its identity failed validation.
    #[error("invalid bounded analytics evidence: {0}")]
    Invalid(String),
}

/// Result type for bounded analytics infrastructure.
pub type BoundedExecutionResult<T> = std::result::Result<T, BoundedExecutionError>;

/// Identity shared by every immutable run in one product build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunIdentity {
    /// Content ID of the frozen canonical catalog snapshot.
    pub snapshot_id: String,
    /// Fixed Unix timestamp used by time-windowed products.
    pub as_of: u64,
    /// Stable product lane name.
    pub product: String,
    /// Semantic version of the product computation.
    pub product_version: String,
    /// Stable description of the sorted key and record representation.
    pub key_space: String,
}

/// Exact identity and accounting for one input object or immutable run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputIdentity {
    /// Stable object key or content-derived run identity.
    pub identity: String,
    /// Exact input byte size.
    pub byte_size: u64,
    /// Exact logical input row count.
    pub row_count: u64,
    /// Lowercase hexadecimal SHA-256 of the input bytes.
    pub sha256: String,
}

/// Exact identity and accounting for one completed immutable artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactIdentity {
    /// Artifact path recorded by the runner.
    pub path: String,
    /// Exact completed byte size.
    pub byte_size: u64,
    /// Exact logical output row count.
    pub row_count: u64,
    /// Minimum encoded key, when the run is non-empty.
    pub min_key: Option<String>,
    /// Maximum encoded key, when the run is non-empty.
    pub max_key: Option<String>,
    /// Lowercase hexadecimal SHA-256 of the completed bytes.
    pub sha256: String,
}

/// Canonical immutable evidence for a completed bounded run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCheckpoint {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Runner implementation identity.
    pub runner_version: String,
    /// Frozen snapshot, time, product, and key-space identity.
    pub run: RunIdentity,
    /// Ordered exact identities consumed by this run.
    pub inputs: Vec<InputIdentity>,
    /// Completed output identity and accounting.
    pub artifact: ArtifactIdentity,
}

/// Atomically publish a completed `.partial` artifact and immutable checkpoint.
///
/// A retry after interruption may reuse an already-published artifact only if
/// its bytes match the partial candidate. An existing checkpoint is accepted
/// only when its canonical bytes and every requested identity match exactly.
#[allow(clippy::too_many_arguments)]
pub fn publish_run_checkpoint(
    partial_path: impl AsRef<Path>,
    completed_path: impl AsRef<Path>,
    checkpoint_path: impl AsRef<Path>,
    run: RunIdentity,
    inputs: Vec<InputIdentity>,
    row_count: u64,
    min_key: Option<String>,
    max_key: Option<String>,
) -> BoundedExecutionResult<RunCheckpoint> {
    let partial_path = partial_path.as_ref();
    let completed_path = completed_path.as_ref();
    let checkpoint_path = checkpoint_path.as_ref();
    require_partial_path(partial_path)?;
    validate_run_identity(&run)?;
    validate_inputs(&inputs)?;
    validate_key_range(row_count, min_key.as_deref(), max_key.as_deref())?;

    let partial_identity = file_identity(partial_path)?;
    publish_file_noclobber(partial_path, completed_path, &partial_identity)?;
    let artifact = ArtifactIdentity {
        path: completed_path.to_string_lossy().into_owned(),
        byte_size: partial_identity.0,
        row_count,
        min_key,
        max_key,
        sha256: partial_identity.1,
    };
    let checkpoint = RunCheckpoint {
        schema_version: BOUNDED_CHECKPOINT_SCHEMA_VERSION,
        runner_version: BOUNDED_RUNNER_VERSION.to_owned(),
        run,
        inputs,
        artifact,
    };
    validate_checkpoint_fields(&checkpoint)?;
    write_canonical_json_noclobber(checkpoint_path, &checkpoint)?;
    validate_run_checkpoint(checkpoint_path, completed_path, &checkpoint)
}

/// Read a checkpoint, requiring canonical JSON and valid evidence fields.
pub fn read_run_checkpoint(path: impl AsRef<Path>) -> BoundedExecutionResult<RunCheckpoint> {
    let bytes = fs::read(path)?;
    let checkpoint: RunCheckpoint = serde_json::from_slice(&bytes)?;
    validate_checkpoint_fields(&checkpoint)?;
    if bytes != canonical_json(&checkpoint)? {
        return Err(BoundedExecutionError::Invalid(
            "checkpoint JSON is not canonically encoded".to_owned(),
        ));
    }
    Ok(checkpoint)
}

/// Validate a checkpoint, its exact expected identity, and completed artifact.
pub fn validate_run_checkpoint(
    checkpoint_path: impl AsRef<Path>,
    completed_path: impl AsRef<Path>,
    expected: &RunCheckpoint,
) -> BoundedExecutionResult<RunCheckpoint> {
    let checkpoint = read_run_checkpoint(checkpoint_path)?;
    if checkpoint != *expected {
        return Err(BoundedExecutionError::Invalid(
            "checkpoint identity does not exactly match the requested run".to_owned(),
        ));
    }
    let completed_path = completed_path.as_ref();
    if checkpoint.artifact.path != completed_path.to_string_lossy() {
        return Err(BoundedExecutionError::Invalid(
            "checkpoint artifact path does not match the requested completed path".to_owned(),
        ));
    }
    let (byte_size, sha256) = file_identity(completed_path)?;
    if byte_size != checkpoint.artifact.byte_size || sha256 != checkpoint.artifact.sha256 {
        return Err(BoundedExecutionError::Invalid(format!(
            "completed artifact {} does not match checkpoint bytes and SHA-256",
            completed_path.display()
        )));
    }
    Ok(checkpoint)
}

fn validate_checkpoint_fields(checkpoint: &RunCheckpoint) -> BoundedExecutionResult<()> {
    if checkpoint.schema_version != BOUNDED_CHECKPOINT_SCHEMA_VERSION {
        return Err(BoundedExecutionError::Invalid(format!(
            "unsupported checkpoint schema version {}",
            checkpoint.schema_version
        )));
    }
    if checkpoint.runner_version != BOUNDED_RUNNER_VERSION {
        return Err(BoundedExecutionError::Invalid(format!(
            "unsupported runner version {:?}",
            checkpoint.runner_version
        )));
    }
    validate_run_identity(&checkpoint.run)?;
    validate_inputs(&checkpoint.inputs)?;
    validate_sha256("artifact SHA-256", &checkpoint.artifact.sha256)?;
    validate_key_range(
        checkpoint.artifact.row_count,
        checkpoint.artifact.min_key.as_deref(),
        checkpoint.artifact.max_key.as_deref(),
    )
}

fn validate_run_identity(run: &RunIdentity) -> BoundedExecutionResult<()> {
    validate_sha256_id("snapshot ID", &run.snapshot_id)?;
    for (label, value) in [
        ("product", run.product.as_str()),
        ("product version", run.product_version.as_str()),
        ("key space", run.key_space.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(BoundedExecutionError::Invalid(format!(
                "{label} must not be empty"
            )));
        }
    }
    Ok(())
}

fn validate_inputs(inputs: &[InputIdentity]) -> BoundedExecutionResult<()> {
    if inputs.is_empty() {
        return Err(BoundedExecutionError::Invalid(
            "a completed run must record at least one input identity".to_owned(),
        ));
    }
    for input in inputs {
        if input.identity.trim().is_empty() {
            return Err(BoundedExecutionError::Invalid(
                "input identity must not be empty".to_owned(),
            ));
        }
        validate_sha256("input SHA-256", &input.sha256)?;
    }
    Ok(())
}

fn validate_key_range(
    row_count: u64,
    min_key: Option<&str>,
    max_key: Option<&str>,
) -> BoundedExecutionResult<()> {
    match (row_count, min_key, max_key) {
        (0, None, None) => Ok(()),
        (0, _, _) => Err(BoundedExecutionError::Invalid(
            "an empty artifact must not record a key range".to_owned(),
        )),
        (_, Some(minimum), Some(maximum)) if minimum <= maximum => Ok(()),
        (_, Some(_), Some(_)) => Err(BoundedExecutionError::Invalid(
            "artifact minimum key exceeds maximum key".to_owned(),
        )),
        _ => Err(BoundedExecutionError::Invalid(
            "a non-empty artifact must record both minimum and maximum keys".to_owned(),
        )),
    }
}

fn require_partial_path(path: &Path) -> BoundedExecutionResult<()> {
    if path.extension().and_then(|value| value.to_str()) != Some("partial") {
        return Err(BoundedExecutionError::Invalid(format!(
            "temporary artifact {} must use a .partial suffix",
            path.display()
        )));
    }
    Ok(())
}

fn validate_sha256_id(label: &str, value: &str) -> BoundedExecutionResult<()> {
    let digest = value.strip_prefix("sha256:").ok_or_else(|| {
        BoundedExecutionError::Invalid(format!("{label} is not a SHA-256 content ID"))
    })?;
    validate_sha256(label, digest)
}

fn validate_sha256(label: &str, value: &str) -> BoundedExecutionResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BoundedExecutionError::Invalid(format!(
            "{label} is not lowercase hexadecimal SHA-256"
        )));
    }
    Ok(())
}

fn publish_file_noclobber(
    partial_path: &Path,
    completed_path: &Path,
    expected: &(u64, String),
) -> BoundedExecutionResult<()> {
    let partial = OpenOptions::new()
        .read(true)
        .write(true)
        .open(partial_path)?;
    partial.sync_all()?;
    let parent = parent_directory(completed_path);
    fs::create_dir_all(parent)?;
    match fs::hard_link(partial_path, completed_path) {
        Ok(()) => File::open(parent)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if file_identity(completed_path)? != *expected {
                return Err(BoundedExecutionError::Invalid(format!(
                    "refusing to replace immutable artifact {}",
                    completed_path.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(partial_path)?;
    File::open(parent_directory(partial_path))?.sync_all()?;
    Ok(())
}

fn write_canonical_json_noclobber(
    path: &Path,
    value: &impl Serialize,
) -> BoundedExecutionResult<()> {
    let bytes = canonical_json(value)?;
    if path.exists() {
        if fs::read(path)? == bytes {
            return Ok(());
        }
        return Err(BoundedExecutionError::Invalid(format!(
            "refusing to replace immutable checkpoint {}",
            path.display()
        )));
    }
    let parent = parent_directory(path);
    fs::create_dir_all(parent)?;
    let partial = partial_checkpoint_path(path)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
    {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(&partial)? != bytes {
                return Err(BoundedExecutionError::Invalid(format!(
                    "checkpoint partial {} has conflicting content",
                    partial.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    match fs::hard_link(&partial, path) {
        Ok(()) => File::open(parent)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(path)? != bytes {
                return Err(BoundedExecutionError::Invalid(format!(
                    "refusing to replace immutable checkpoint {}",
                    path.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(&partial)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn partial_checkpoint_path(path: &Path) -> BoundedExecutionResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        BoundedExecutionError::Invalid("checkpoint path must have a file name".to_owned())
    })?;
    Ok(path.with_file_name(format!("{}.partial", file_name.to_string_lossy())))
}

fn canonical_json(value: &impl Serialize) -> BoundedExecutionResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn file_identity(path: &Path) -> BoundedExecutionResult<(u64, String)> {
    let mut file = File::open(path)?;
    let byte_size = file.metadata()?.len();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok((byte_size, hex::encode(digest.finalize())))
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_identity() -> RunIdentity {
        RunIdentity {
            snapshot_id: format!("sha256:{}", "1".repeat(64)),
            as_of: 1_786_024_407,
            product: "canonical-events".to_owned(),
            product_version: "v1".to_owned(),
            key_space: "event-id-v1".to_owned(),
        }
    }

    fn input_identity() -> InputIdentity {
        InputIdentity {
            identity: "raw/object.parquet".to_owned(),
            byte_size: 100,
            row_count: 2,
            sha256: "2".repeat(64),
        }
    }

    fn publish(root: &Path, bytes: &[u8]) -> BoundedExecutionResult<RunCheckpoint> {
        let partial = root.join("run.bin.partial");
        fs::write(&partial, bytes)?;
        publish_run_checkpoint(
            &partial,
            root.join("run.bin"),
            root.join("run.json"),
            run_identity(),
            vec![input_identity()],
            2,
            Some("00".to_owned()),
            Some("ff".to_owned()),
        )
    }

    #[test]
    fn publishes_and_revalidates_canonical_immutable_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = publish(root.path(), b"sorted run").expect("publish");
        assert!(!root.path().join("run.bin.partial").exists());
        let validated = validate_run_checkpoint(
            root.path().join("run.json"),
            root.path().join("run.bin"),
            &checkpoint,
        )
        .expect("validate");
        assert_eq!(validated, checkpoint);
        let bytes = fs::read(root.path().join("run.json")).expect("checkpoint bytes");
        assert_eq!(bytes, canonical_json(&checkpoint).expect("canonical JSON"));
    }

    #[test]
    fn partial_artifact_never_satisfies_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("run.bin.partial"), b"incomplete").expect("partial");
        assert!(read_run_checkpoint(root.path().join("run.json")).is_err());
        assert!(!root.path().join("run.bin").exists());
    }

    #[test]
    fn resumes_after_artifact_publication_before_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let completed = root.path().join("run.bin");
        fs::write(&completed, b"sorted run").expect("completed");
        let checkpoint = publish(root.path(), b"sorted run").expect("resume");
        assert_eq!(checkpoint.artifact.byte_size, 10);
        assert!(root.path().join("run.json").is_file());
    }

    #[test]
    fn exact_retry_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = publish(root.path(), b"sorted run").expect("first publish");
        let second = publish(root.path(), b"sorted run").expect("retry");
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_tampered_artifact_and_identity_reuse() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = publish(root.path(), b"sorted run").expect("publish");
        fs::write(root.path().join("run.bin"), b"tampered").expect("tamper");
        assert!(
            validate_run_checkpoint(
                root.path().join("run.json"),
                root.path().join("run.bin"),
                &checkpoint,
            )
            .is_err()
        );

        fs::write(root.path().join("run.bin.partial"), b"different").expect("partial");
        assert!(
            publish_run_checkpoint(
                root.path().join("run.bin.partial"),
                root.path().join("run.bin"),
                root.path().join("run.json"),
                run_identity(),
                vec![input_identity()],
                2,
                Some("00".to_owned()),
                Some("ff".to_owned()),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_noncanonical_or_mismatched_checkpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = publish(root.path(), b"sorted run").expect("publish");
        let path = root.path().join("run.json");
        let compact = serde_json::to_vec(&checkpoint).expect("compact JSON");
        fs::write(&path, compact).expect("replace checkpoint in test");
        assert!(read_run_checkpoint(&path).is_err());
    }
}
