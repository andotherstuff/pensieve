//! Versioned, deterministic distinct-identity sketches for flexible windows.
//!
//! Slice 6 uses Apache DataSketches HLL with one Pensieve envelope. Builders
//! feed sorted 32-byte identities into each leaf sketch and sort serialized
//! leaves before union. Those two rules make artifact bytes reproducible while
//! keeping both leaf construction and union memory bounded by fixed sketch
//! state rather than input cardinality.

use datasketches::hash::value::raw_bytes;
use datasketches::hll::{HllSketch, HllType, HllUnion};

/// Pensieve envelope version for serialized distinct sketches.
pub const DISTINCT_SKETCH_FORMAT_VERSION: u8 = 1;

/// Apache DataSketches HLL serialization version accepted by this envelope.
pub const DATASKETCHES_HLL_SERIALIZATION_VERSION: u8 = 1;

/// HLL precision: 4096 registers and roughly 1.6% nominal relative error.
pub const DISTINCT_SKETCH_LG_K: u8 = 12;

/// Maximum accepted relative error for dense Slice 6 validation fixtures.
pub const DISTINCT_SKETCH_RELATIVE_TOLERANCE: f64 = 0.02;

const MAGIC: &[u8; 8] = b"PNSHLL\0\0";
const TARGET_HLL_TYPE: HllType = HllType::Hll8;
const TARGET_HLL_TYPE_CODE: u8 = 8;
const HEADER_BYTES: usize = MAGIC.len() + 1 + 1 + 1 + 4;

/// Invalid deterministic input or serialized sketch state.
#[derive(Debug, thiserror::Error)]
pub enum DistinctSketchError {
    /// Leaf inputs must be sorted so repeated builds produce identical bytes.
    #[error("distinct sketch identities are not in ascending byte order")]
    UnsortedIdentities,
    /// The Pensieve envelope is absent or truncated.
    #[error("distinct sketch envelope is truncated")]
    TruncatedEnvelope,
    /// The serialized state belongs to another envelope version or product.
    #[error("unsupported distinct sketch envelope: {0}")]
    UnsupportedEnvelope(String),
    /// The embedded Apache DataSketches payload is invalid.
    #[error("invalid Apache DataSketches HLL payload: {0}")]
    InvalidPayload(#[source] datasketches::error::Error),
}

/// One fixed-memory, mergeable distinct-identity sketch.
#[derive(Debug, Clone, PartialEq)]
pub struct DistinctSketch {
    inner: HllSketch,
}

/// Streaming deterministic leaf builder retaining only one prior identity.
pub struct DistinctSketchBuilder {
    inner: HllSketch,
    previous: Option<[u8; 32]>,
}

/// Fixed-memory union for already canonical leaf order.
pub struct DistinctSketchUnion {
    inner: HllUnion,
}

impl Default for DistinctSketchUnion {
    fn default() -> Self {
        Self::new()
    }
}

impl DistinctSketchUnion {
    /// Create an empty union using the fixed Slice 6 precision.
    pub fn new() -> Self {
        Self {
            inner: HllUnion::new(DISTINCT_SKETCH_LG_K),
        }
    }

    /// Merge one validated serialized leaf into the union.
    pub fn push_serialized(&mut self, bytes: &[u8]) -> Result<(), DistinctSketchError> {
        let sketch = DistinctSketch::deserialize(bytes)?;
        self.inner.update(&sketch.inner);
        Ok(())
    }

    /// Finish the union as a canonical HLL8 sketch.
    pub fn finish(self) -> DistinctSketch {
        DistinctSketch {
            inner: self.inner.to_sketch(TARGET_HLL_TYPE),
        }
    }
}

impl Default for DistinctSketchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DistinctSketchBuilder {
    /// Create an empty builder using the fixed Slice 6 sketch configuration.
    pub fn new() -> Self {
        Self {
            inner: HllSketch::new(DISTINCT_SKETCH_LG_K, TARGET_HLL_TYPE),
            previous: None,
        }
    }

    /// Add the next ascending raw identity, ignoring adjacent duplicates.
    pub fn push(&mut self, identity: [u8; 32]) -> Result<(), DistinctSketchError> {
        if let Some(previous) = self.previous {
            if identity < previous {
                return Err(DistinctSketchError::UnsortedIdentities);
            }
            if identity == previous {
                return Ok(());
            }
        }
        self.inner.update(raw_bytes::from_slice(&identity));
        self.previous = Some(identity);
        Ok(())
    }

    /// Finish the immutable sketch leaf.
    pub fn finish(self) -> DistinctSketch {
        DistinctSketch { inner: self.inner }
    }
}

impl DistinctSketch {
    /// Build a deterministic leaf from sorted 32-byte identities.
    ///
    /// Equal adjacent identities are ignored. A descending identity fails
    /// closed instead of silently producing an order-dependent artifact.
    pub fn from_sorted_identities(
        identities: impl IntoIterator<Item = [u8; 32]>,
    ) -> Result<Self, DistinctSketchError> {
        let mut builder = DistinctSketchBuilder::new();
        for identity in identities {
            builder.push(identity)?;
        }
        Ok(builder.finish())
    }

    /// Merge serialized leaves in canonical byte order.
    ///
    /// Sorting a bounded list of fixed-size leaf states makes a repeated merge
    /// byte-identical even when callers discover checkpoints in a different
    /// filesystem order.
    pub fn merge_serialized<'a>(
        sketches: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Self, DistinctSketchError> {
        let mut canonical = sketches.into_iter().collect::<Vec<_>>();
        canonical.sort_unstable();

        let mut union = DistinctSketchUnion::new();
        for bytes in canonical {
            union.push_serialized(bytes)?;
        }
        Ok(union.finish())
    }

    /// Rounded cardinality estimate published to serving relations.
    pub fn estimate(&self) -> u64 {
        self.inner.estimate().round() as u64
    }

    /// Serialize using the versioned Pensieve envelope.
    pub fn serialize(&self) -> Vec<u8> {
        let payload = self.inner.serialize();
        let payload_len = u32::try_from(payload.len()).expect("HLL payload length fits u32");
        let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
        bytes.extend_from_slice(MAGIC);
        bytes.push(DISTINCT_SKETCH_FORMAT_VERSION);
        bytes.push(DISTINCT_SKETCH_LG_K);
        bytes.push(TARGET_HLL_TYPE_CODE);
        bytes.extend_from_slice(&payload_len.to_be_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    /// Deserialize and validate both the Pensieve envelope and HLL settings.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, DistinctSketchError> {
        if bytes.len() < HEADER_BYTES {
            return Err(DistinctSketchError::TruncatedEnvelope);
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(DistinctSketchError::UnsupportedEnvelope(
                "magic does not identify Pensieve HLL".to_owned(),
            ));
        }
        let version = bytes[MAGIC.len()];
        let lg_k = bytes[MAGIC.len() + 1];
        let hll_type = bytes[MAGIC.len() + 2];
        if version != DISTINCT_SKETCH_FORMAT_VERSION
            || lg_k != DISTINCT_SKETCH_LG_K
            || hll_type != TARGET_HLL_TYPE_CODE
        {
            return Err(DistinctSketchError::UnsupportedEnvelope(format!(
                "version={version}, lg_k={lg_k}, hll_type={hll_type}"
            )));
        }

        let payload_len_offset = MAGIC.len() + 3;
        let payload_len = u32::from_be_bytes(
            bytes[payload_len_offset..payload_len_offset + 4]
                .try_into()
                .expect("validated envelope header length"),
        ) as usize;
        let expected_len = HEADER_BYTES.checked_add(payload_len).ok_or_else(|| {
            DistinctSketchError::UnsupportedEnvelope("payload length overflow".to_owned())
        })?;
        if bytes.len() != expected_len {
            return Err(DistinctSketchError::UnsupportedEnvelope(format!(
                "declared payload bytes={payload_len}, actual={}",
                bytes.len() - HEADER_BYTES
            )));
        }

        let payload = &bytes[HEADER_BYTES..];
        if payload.get(1).copied() != Some(DATASKETCHES_HLL_SERIALIZATION_VERSION) {
            return Err(DistinctSketchError::UnsupportedEnvelope(format!(
                "Apache HLL serialization version={:?}",
                payload.get(1)
            )));
        }
        let inner = HllSketch::deserialize(payload).map_err(DistinctSketchError::InvalidPayload)?;
        if inner.lg_config_k() != DISTINCT_SKETCH_LG_K || inner.target_type() != TARGET_HLL_TYPE {
            return Err(DistinctSketchError::UnsupportedEnvelope(format!(
                "payload lg_k={}, hll_type={:?}",
                inner.lg_config_k(),
                inner.target_type()
            )));
        }
        Ok(Self { inner })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(value: u64) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        bytes[24..].copy_from_slice(&value.to_be_bytes());
        bytes
    }

    #[test]
    fn sparse_build_is_exact_deterministic_and_duplicate_safe() {
        let first = DistinctSketch::from_sorted_identities([
            identity(1),
            identity(1),
            identity(2),
            identity(3),
        ])
        .expect("build sparse sketch");
        let second =
            DistinctSketch::from_sorted_identities([identity(1), identity(2), identity(3)])
                .expect("rebuild sparse sketch");

        assert_eq!(first.estimate(), 3);
        assert_eq!(first.serialize(), second.serialize());
        assert_eq!(
            DistinctSketch::deserialize(&first.serialize())
                .expect("deserialize")
                .serialize(),
            first.serialize()
        );
    }

    #[test]
    fn unsorted_leaf_fails_closed() {
        let error = DistinctSketch::from_sorted_identities([identity(2), identity(1)])
            .expect_err("unsorted identities must fail");
        assert!(matches!(error, DistinctSketchError::UnsortedIdentities));
    }

    #[test]
    fn union_is_deterministic_across_checkpoint_discovery_order() {
        let left = DistinctSketch::from_sorted_identities((0..10_000).map(identity))
            .expect("build left")
            .serialize();
        let right = DistinctSketch::from_sorted_identities((5_000..15_000).map(identity))
            .expect("build right")
            .serialize();

        let left_first = DistinctSketch::merge_serialized([left.as_slice(), right.as_slice()])
            .expect("merge left first");
        let right_first = DistinctSketch::merge_serialized([right.as_slice(), left.as_slice()])
            .expect("merge right first");
        assert_eq!(left_first.serialize(), right_first.serialize());
        assert_relative_error(left_first.estimate(), 15_000);
    }

    #[test]
    fn dense_and_adversarial_fixtures_stay_inside_error_gate() {
        let dense = DistinctSketch::from_sorted_identities((0..100_000).map(identity))
            .expect("build dense");
        assert_relative_error(dense.estimate(), 100_000);

        let clustered = DistinctSketch::from_sorted_identities((0_u64..100_000).map(|value| {
            let mut bytes = [0xA5; 32];
            bytes[24..].copy_from_slice(&value.to_be_bytes());
            bytes
        }))
        .expect("build clustered");
        assert_relative_error(clustered.estimate(), 100_000);
    }

    #[test]
    fn envelope_rejects_version_and_length_changes() {
        let sketch = DistinctSketch::from_sorted_identities([identity(1)]).expect("build");
        let mut wrong_version = sketch.serialize();
        wrong_version[MAGIC.len()] += 1;
        assert!(matches!(
            DistinctSketch::deserialize(&wrong_version),
            Err(DistinctSketchError::UnsupportedEnvelope(_))
        ));

        let mut truncated = sketch.serialize();
        truncated.pop();
        assert!(matches!(
            DistinctSketch::deserialize(&truncated),
            Err(DistinctSketchError::UnsupportedEnvelope(_))
        ));
    }

    fn assert_relative_error(estimate: u64, exact: u64) {
        let relative = estimate.abs_diff(exact) as f64 / exact as f64;
        assert!(
            relative <= DISTINCT_SKETCH_RELATIVE_TOLERANCE,
            "estimate {estimate} differs from exact {exact} by {relative:.6}"
        );
    }
}
