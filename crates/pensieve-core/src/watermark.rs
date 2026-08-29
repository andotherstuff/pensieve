//! Canonical ingestion-owned latest-event watermark.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Canonical evidence for the latest eligible event in durably sealed segments.
///
/// The timestamp follows the API's `u32` domain and excludes events dated after
/// their segment seal. All fields are cumulative so concurrent segment
/// completions cannot regress the published value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LatestEventWatermark {
    /// Watermark schema version.
    pub schema_version: u32,
    /// Publication status; currently always `published`.
    pub status: String,
    /// Greatest segment number observed by the publisher.
    pub max_sealed_segment_number: u64,
    /// Greatest eligible event timestamp across observed sealed segments.
    pub max_eligible_created_at: Option<u64>,
    /// Greatest seal timestamp across observed sealed segments.
    pub max_sealed_at_epoch: u64,
    /// Wall-clock timestamp at which this file was atomically published.
    pub published_at_epoch: u64,
}

impl LatestEventWatermark {
    /// Strictly validate the versioned watermark contract.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || self.status != "published" {
            return invalid("unsupported schema or status");
        }
        if self.published_at_epoch < self.max_sealed_at_epoch {
            return invalid("publication predates its latest segment seal");
        }
        if self.max_eligible_created_at.is_some_and(|created_at| {
            created_at > self.max_sealed_at_epoch || created_at > u64::from(u32::MAX)
        }) {
            return invalid("contains an ineligible event timestamp");
        }
        Ok(())
    }
}

/// Read, validate, and require canonical encoding for a latest-event watermark.
pub fn read_latest_event_watermark(path: impl AsRef<Path>) -> Result<LatestEventWatermark> {
    let bytes = fs::read(path)?;
    let watermark: LatestEventWatermark = serde_json::from_slice(&bytes)?;
    watermark.validate()?;
    let mut canonical = serde_json::to_vec_pretty(&watermark)?;
    canonical.push(b'\n');
    if bytes != canonical {
        return invalid("is not canonically encoded");
    }
    Ok(watermark)
}

fn invalid<T>(reason: impl Into<String>) -> Result<T> {
    Err(Error::InvalidField {
        field: "latest_event_watermark",
        reason: reason.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::LatestEventWatermark;

    fn valid() -> LatestEventWatermark {
        LatestEventWatermark {
            schema_version: 1,
            status: "published".to_owned(),
            max_sealed_segment_number: 7,
            max_eligible_created_at: Some(90),
            max_sealed_at_epoch: 100,
            published_at_epoch: 101,
        }
    }

    #[test]
    fn contract_rejects_future_ineligible_and_invalid_publication_state() {
        assert!(valid().validate().is_ok());
        let mut watermark = valid();
        watermark.max_eligible_created_at = Some(102);
        assert!(watermark.validate().is_err());
        let mut watermark = valid();
        watermark.published_at_epoch = 99;
        assert!(watermark.validate().is_err());
        let mut watermark = valid();
        watermark.status = "partial".to_owned();
        assert!(watermark.validate().is_err());
    }
}
