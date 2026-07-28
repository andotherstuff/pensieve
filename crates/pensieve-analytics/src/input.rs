//! Resolve immutable catalog object keys to DuckDB-readable locations.

use std::path::{Path, PathBuf};

use pensieve_lake::{ActiveRawSnapshot, read_catalog_snapshot};

use crate::{Error, Result};

/// One object location understood by DuckDB.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObjectLocation {
    /// A local file below an operator-provided immutable lake root.
    Local(PathBuf),
    /// An object read directly through DuckDB's S3 support.
    S3(String),
}

impl ObjectLocation {
    /// Return a URI/path suitable for `read_parquet`.
    pub fn duckdb_path(&self) -> String {
        match self {
            Self::Local(path) => path.to_string_lossy().into_owned(),
            Self::S3(uri) => uri.clone(),
        }
    }
}

/// A validated active-file snapshot plus resolved object locations.
#[derive(Clone, Debug)]
pub struct ResolvedSnapshot {
    /// Validated immutable active-file snapshot.
    pub catalog: ActiveRawSnapshot,
    /// Locations in the same canonical order as `catalog.objects()`.
    pub locations: Vec<ObjectLocation>,
    /// S3 endpoint hostname when locations are remote.
    pub s3_endpoint: Option<String>,
    /// S3 bucket when locations are remote.
    pub s3_bucket: Option<String>,
}

/// Read a canonical snapshot and resolve all of its object keys.
///
/// Supplying `local_object_root` is the deterministic test/offline mode. When
/// it is absent, `store_id` must identify an S3-compatible HTTPS namespace.
pub fn resolve_snapshot(
    catalog_path: impl AsRef<Path>,
    local_object_root: Option<&Path>,
) -> Result<ResolvedSnapshot> {
    let catalog = read_catalog_snapshot(catalog_path)?;
    if let Some(root) = local_object_root {
        let mut locations = Vec::with_capacity(catalog.objects().len());
        for object in catalog.objects() {
            let path = root.join(&object.object_key);
            if !path.is_file() {
                return Err(Error::MissingLocalObject(path));
            }
            locations.push(ObjectLocation::Local(path));
        }
        return Ok(ResolvedSnapshot {
            catalog,
            locations,
            s3_endpoint: None,
            s3_bucket: None,
        });
    }

    let (endpoint, bucket) = parse_s3_store_id(catalog.store_id())?;
    let locations = catalog
        .objects()
        .iter()
        .map(|object| ObjectLocation::S3(format!("s3://{bucket}/{}", object.object_key)))
        .collect();
    Ok(ResolvedSnapshot {
        catalog,
        locations,
        s3_endpoint: Some(endpoint),
        s3_bucket: Some(bucket),
    })
}

fn parse_s3_store_id(store_id: &str) -> Result<(String, String)> {
    let namespace = store_id
        .strip_prefix("s3+https://")
        .ok_or_else(|| Error::UnsupportedStoreId(store_id.to_owned()))?;
    let (endpoint, bucket) = namespace
        .split_once('/')
        .ok_or_else(|| Error::UnsupportedStoreId(store_id.to_owned()))?;
    if endpoint.is_empty()
        || bucket.is_empty()
        || bucket.contains('/')
        || endpoint.contains(['?', '#'])
        || bucket.contains(['?', '#'])
    {
        return Err(Error::UnsupportedStoreId(store_id.to_owned()));
    }
    Ok((endpoint.to_owned(), bucket.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::parse_s3_store_id;

    #[test]
    fn parses_full_s3_namespace_identity() {
        assert_eq!(
            parse_s3_store_id("s3+https://hel1.your-objectstorage.com/pensieve-parquet")
                .expect("valid namespace"),
            (
                "hel1.your-objectstorage.com".to_owned(),
                "pensieve-parquet".to_owned()
            )
        );
    }

    #[test]
    fn rejects_ambiguous_store_identity() {
        assert!(parse_s3_store_id("pensieve-parquet").is_err());
        assert!(parse_s3_store_id("s3+https://host/bucket/prefix").is_err());
    }
}
