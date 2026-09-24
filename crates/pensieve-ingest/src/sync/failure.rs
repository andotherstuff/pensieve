//! Bounded, untrusted worker failure diagnostics; never archive or completion proof.

use serde::{Deserialize, Serialize};

/// Maximum diagnostic event IDs retained per attempt.
pub const MAX_FAILURE_SAMPLE: usize = 128;
/// Maximum missing count reported by the bounded worker.
pub const MAX_FAILURE_MISSING: u64 = 50_000;

/// Stable failure classes with no free-form relay-controlled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Relay or reconciliation lifecycle failure.
    Relay,
    /// Attempt volume exceeded a configured bound.
    Volume,
    /// An event exceeded the permitted frame size.
    EventSize,
    /// Advertised event IDs remain outstanding; not proof they are undeliverable.
    Unavailable,
    /// The worker was cancelled.
    Cancelled,
}

/// Diagnostic hints only: a sample does not promise full missing-ID coverage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureDiagnostic {
    /// Failure class used by parent retry policy.
    pub kind: FailureKind,
    /// Outstanding IDs observed by the worker, including not-yet-requested IDs.
    /// Not a completeness assertion or authority to skip events on retry.
    pub missing_count: u64,
    /// Sorted unique sample, present only for unavailable events.
    pub sample: Vec<[u8; 32]>,
}

impl FailureDiagnostic {
    /// Check semantic bounds after bounded frame decoding and before persistence.
    pub fn validate(&self) -> bool {
        self.sample.len() <= MAX_FAILURE_SAMPLE
            && self.missing_count <= MAX_FAILURE_MISSING
            && self.missing_count >= self.sample.len() as u64
            && self.sample.windows(2).all(|ids| ids[0] < ids[1])
            && (self.kind == FailureKind::Unavailable
                || (self.missing_count == 0 && self.sample.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_reject_unbounded_ambiguous_and_unknown_data() {
        let mut report = FailureDiagnostic {
            kind: FailureKind::Unavailable,
            missing_count: 2,
            sample: vec![[1; 32], [2; 32]],
        };
        assert!(report.validate());
        report.sample.reverse();
        assert!(!report.validate());
        report.sample = vec![[1; 32]; 2];
        assert!(!report.validate());
        report.sample = vec![[1; 32]];
        report.missing_count = 0;
        assert!(!report.validate());
        report.missing_count = MAX_FAILURE_MISSING + 1;
        assert!(!report.validate());
        report.missing_count = 1;
        report.kind = FailureKind::Relay;
        assert!(!report.validate());
        report.sample.clear();
        report.missing_count = 0;
        assert!(report.validate());
        assert!(
            serde_json::from_str::<FailureDiagnostic>(
                r#"{"kind":"relay","missing_count":0,"sample":[],"extra":true}"#
            )
            .is_err()
        );
        report.kind = FailureKind::Unavailable;
        report.missing_count = 129;
        report.sample = (0..129).map(|n| [n; 32]).collect();
        assert!(!report.validate());
    }
}
