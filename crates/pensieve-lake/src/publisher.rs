//! Immutable object publication backends.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::work_unit::sha256_file;
use crate::{Error, Result};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;

/// Confirmed properties of one durably published immutable object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedObject {
    /// Immutable key supplied by the caller.
    pub key: String,
    /// Confirmed byte size.
    pub byte_size: u64,
    /// Confirmed lowercase SHA-256.
    pub sha256: String,
}

/// Backend that durably publishes content-addressed immutable objects.
pub trait Publisher: Send + Sync {
    /// Publish `source` at `key`, or confirm an identical existing object.
    ///
    /// Implementations must never replace different bytes at an existing key.
    fn publish(
        &self,
        key: &str,
        source: &Path,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<PublishedObject>;
}

/// Filesystem object store used for local campaigns and fault testing.
#[derive(Clone, Debug)]
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    /// Create a local immutable object namespace rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Resolve an object key for diagnostics and tests.
    pub fn path_for_key(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }
}

impl Publisher for LocalObjectStore {
    fn publish(
        &self,
        key: &str,
        source: &Path,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<PublishedObject> {
        let destination = self.path_for_key(key)?;
        if destination.exists() {
            return confirm_existing(&destination, key, expected_bytes, expected_sha256);
        }

        let parent = destination
            .parent()
            .expect("validated object key has a parent");
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let mut source_file = File::open(source)?;
        let copied = std::io::copy(&mut source_file, temporary.as_file_mut())?;
        if copied != expected_bytes {
            return Err(Error::ArtifactMismatch {
                path: source.to_owned(),
            });
        }
        temporary.as_file_mut().flush()?;
        temporary.as_file_mut().sync_all()?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                return confirm_existing(&destination, key, expected_bytes, expected_sha256);
            }
            Err(error) => return Err(error.error.into()),
        }
        File::open(parent)?.sync_all()?;
        confirm_existing(&destination, key, expected_bytes, expected_sha256)
    }
}

/// Connection settings for an S3-compatible immutable object namespace.
#[derive(Clone, Debug)]
pub struct S3PublisherConfig {
    /// Destination bucket.
    pub bucket: String,
    /// Optional region override. The normal AWS resolution chain is used when absent.
    pub region: Option<String>,
    /// Optional endpoint for S3-compatible providers.
    pub endpoint_url: Option<String>,
    /// Use path-style bucket addressing.
    pub force_path_style: bool,
}

/// Immutable publisher backed by AWS S3 or an S3-compatible object store.
///
/// Credentials use the AWS SDK's normal environment/profile/instance resolution
/// chain. Each upload is conditional and therefore cannot replace an existing
/// object. A same-key retry succeeds only when both size and the stored SHA-256
/// metadata match.
pub struct S3Publisher {
    client: aws_sdk_s3::Client,
    bucket: String,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for S3Publisher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("S3Publisher")
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

impl S3Publisher {
    /// Build a publisher using the AWS SDK's standard credential chain.
    ///
    /// This constructor and [`Publisher::publish`] are blocking and should be
    /// called from a normal worker thread, not directly on an async executor.
    pub fn from_environment(config: S3PublisherConfig) -> Result<Self> {
        if config.bucket.trim().is_empty() {
            return Err(Error::ObjectStore(
                "S3 bucket must not be empty".to_string(),
            ));
        }

        let runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| Error::ObjectStore(error.to_string()))?,
        );
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = config.region {
            loader = loader.region(Region::new(region));
        }
        let shared_config = runtime.block_on(loader.load());
        let mut s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
            .force_path_style(config.force_path_style);
        if let Some(endpoint_url) = config.endpoint_url {
            s3_config = s3_config.endpoint_url(endpoint_url);
        }

        Ok(Self {
            client: aws_sdk_s3::Client::from_conf(s3_config.build()),
            bucket: config.bucket,
            runtime,
        })
    }

    fn inspect(
        &self,
        key: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<Option<PublishedObject>> {
        let response = self.runtime.block_on(
            self.client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send(),
        );
        match response {
            Ok(output) => {
                let byte_size = output
                    .content_length()
                    .and_then(|size| u64::try_from(size).ok());
                let sha256 = output
                    .metadata()
                    .and_then(|metadata| metadata.get("sha256"));
                if byte_size != Some(expected_bytes)
                    || sha256.map(String::as_str) != Some(expected_sha256)
                {
                    return Err(Error::ObjectCollision {
                        key: key.to_owned(),
                    });
                }
                Ok(Some(PublishedObject {
                    key: key.to_owned(),
                    byte_size: expected_bytes,
                    sha256: expected_sha256.to_owned(),
                }))
            }
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_s3::operation::head_object::HeadObjectError::is_not_found,
                ) =>
            {
                Ok(None)
            }
            Err(error) => {
                let detail = error.as_service_error().map_or_else(
                    || error.to_string(),
                    |service| {
                        format!(
                            "code={}, message={}",
                            service.code().unwrap_or("unknown"),
                            service.message().unwrap_or("none")
                        )
                    },
                );
                Err(Error::ObjectStore(format!("HeadObject {key}: {detail}")))
            }
        }
    }
}

impl Publisher for S3Publisher {
    fn publish(
        &self,
        key: &str,
        source: &Path,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<PublishedObject> {
        validate_key(key)?;
        let actual_bytes = source.metadata()?.len();
        let actual_sha256 = sha256_file(source)?;
        if actual_bytes != expected_bytes || actual_sha256 != expected_sha256 {
            return Err(Error::ArtifactMismatch {
                path: source.to_owned(),
            });
        }
        if let Some(existing) = self.inspect(key, expected_bytes, expected_sha256)? {
            return Ok(existing);
        }

        let content_length =
            i64::try_from(expected_bytes).map_err(|_| Error::NumericOutOfRange {
                field: "object byte size",
            })?;
        let body = self
            .runtime
            .block_on(ByteStream::from_path(source))
            .map_err(|error| Error::ObjectStore(error.to_string()))?;
        let result = self.runtime.block_on(
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(body)
                .content_length(content_length)
                .metadata("sha256", expected_sha256)
                .metadata("pensieve-format", "nostr-event-archive-v1")
                .if_none_match("*")
                .send(),
        );
        if let Err(error) = result {
            let raced_with_identical_writer = error
                .as_service_error()
                .and_then(ProvideErrorMetadata::code)
                .is_some_and(|code| {
                    code == "PreconditionFailed" || code == "ConditionalRequestConflict"
                });
            if !raced_with_identical_writer {
                let detail = error.as_service_error().map_or_else(
                    || error.to_string(),
                    |service| {
                        format!(
                            "code={}, message={}",
                            service.code().unwrap_or("unknown"),
                            service.message().unwrap_or("none")
                        )
                    },
                );
                return Err(Error::ObjectStore(format!("PutObject {key}: {detail}")));
            }
        }

        self.inspect(key, expected_bytes, expected_sha256)?
            .ok_or_else(|| {
                Error::ObjectStore(format!(
                    "S3 object {key} was not visible after a successful upload"
                ))
            })
    }
}

fn confirm_existing(
    path: &Path,
    key: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<PublishedObject> {
    let byte_size = path.metadata()?.len();
    let sha256 = sha256_file(path)?;
    if byte_size != expected_bytes || sha256 != expected_sha256 {
        return Err(Error::ObjectCollision {
            key: key.to_owned(),
        });
    }
    Ok(PublishedObject {
        key: key.to_owned(),
        byte_size,
        sha256,
    })
}

fn validate_key(key: &str) -> Result<()> {
    let path = Path::new(key);
    let valid = !key.is_empty()
        && !path.is_absolute()
        && key.split('/').all(|component| !component.is_empty())
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(Error::ObjectCollision {
            key: key.to_owned(),
        })
    }
}
