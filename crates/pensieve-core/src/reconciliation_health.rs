//! Non-sensitive reconciliation progress health, independent of archive admission.
//!
//! Persisted ledger observations establish progress; process liveness alone does
//! not. A stale observation fails closed even when an owner task is still alive.

use std::sync::{Mutex, OnceLock};

/// Progress thresholds deliberately exceed a single nine-minute worker session.
const OBSERVATION_TIMEOUT: i64 = 15 * 60;
const COMPLETION_TIMEOUT: i64 = 60 * 60;

/// Durable facts sampled by the sole ledger owner, without relay URLs or tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressObservation {
    /// Execution-time Unix seconds of the observation.
    pub observed_at: i64,
    /// Earliest persisted creation time of pending work; None means idle.
    pub pending_since: Option<i64>,
    /// Most recent archive-confirmed completion, preserved across restarts.
    pub last_success: Option<i64>,
    /// Blocked or repeatedly failing obligations remain unresolved.
    pub persistent_failure: bool,
    /// Job, ledger-byte or disk admission capacity is exhausted.
    pub capacity_exhausted: bool,
}

#[derive(Debug, Default)]
struct State {
    enabled: bool,
    fault: bool,
    observation: Option<ProgressObservation>,
}

/// One process-wide health state; no database is opened by HTTP handlers.
#[derive(Debug, Default)]
pub struct ReconciliationHealth(Mutex<State>);

impl ReconciliationHealth {
    /// Mark explicit activation; readiness still requires an owner observation.
    pub fn enable(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.enabled = true;
        }
    }

    /// Latch an owner/runtime failure. Restart must still recover persisted work.
    pub fn fail(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.fault = true;
        }
    }

    /// Publish durable facts, without clearing a separately latched runtime fault.
    pub fn observe(&self, observation: ProgressObservation) {
        if let Ok(mut state) = self.0.lock() {
            state.observation = Some(observation);
        }
    }

    /// Return a fixed classification. Disabled is explicit, not a claim of health.
    /// Operators must pause the dedicated monitor when intentionally disabled.
    pub fn status(&self, now: i64) -> (bool, &'static str) {
        let Ok(state) = self.0.lock() else {
            return (false, "state_unavailable");
        };
        if !state.enabled {
            return (false, "disabled");
        }
        if state.fault {
            return (false, "runtime_failed");
        }
        let Some(o) = state.observation else {
            return (false, "starting");
        };
        if now < o.observed_at || now.saturating_sub(o.observed_at) > OBSERVATION_TIMEOUT {
            return (false, "owner_observation_stale");
        }
        if o.capacity_exhausted {
            return (false, "capacity_exhausted");
        }
        if o.persistent_failure {
            return (false, "unresolved_failures");
        }
        if let Some(pending) = o.pending_since {
            let progress = pending.max(o.last_success.unwrap_or(pending));
            if progress > now || now.saturating_sub(progress) >= COMPLETION_TIMEOUT {
                return (false, "durable_progress_stalled");
            }
            return (true, "progress_within_deadline");
        }
        (true, "idle")
    }
}

/// Shared by the HTTP server and the isolated runtime, not by a second ledger owner.
pub fn reconciliation_health() -> &'static ReconciliationHealth {
    static HEALTH: OnceLock<ReconciliationHealth> = OnceLock::new();
    HEALTH.get_or_init(ReconciliationHealth::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(now: i64) -> ProgressObservation {
        ProgressObservation {
            observed_at: now,
            pending_since: Some(100),
            last_success: None,
            persistent_failure: false,
            capacity_exhausted: false,
        }
    }

    #[test]
    fn restart_does_not_reset_persisted_progress_deadline() {
        let health = ReconciliationHealth::default();
        assert_eq!(health.status(4000), (false, "disabled"));
        health.enable();
        health.observe(observation(4000));
        assert_eq!(health.status(4000), (false, "durable_progress_stalled"));
        let mut recovered = observation(4001);
        recovered.last_success = Some(4000);
        health.observe(recovered);
        assert!(health.status(4001).0);
    }

    #[test]
    fn stale_owner_failure_and_pressure_are_never_healthy() {
        let health = ReconciliationHealth::default();
        health.enable();
        assert_eq!(health.status(100), (false, "starting"));
        health.observe(observation(100));
        assert!(health.status(100).0);
        assert_eq!(health.status(1001), (false, "owner_observation_stale"));
        assert!(!health.status(99).0);
        let mut o = observation(101);
        o.persistent_failure = true;
        health.observe(o);
        assert_eq!(health.status(101), (false, "unresolved_failures"));
        o.capacity_exhausted = true;
        health.observe(o);
        assert_eq!(health.status(101), (false, "capacity_exhausted"));
        health.fail();
        health.observe(observation(102));
        assert_eq!(health.status(102), (false, "runtime_failed"));
    }

    #[test]
    fn idle_is_distinct_from_missing_observations() {
        let health = ReconciliationHealth::default();
        health.enable();
        let mut o = observation(100);
        o.pending_since = None;
        health.observe(o);
        assert_eq!(health.status(100), (true, "idle"));
        assert!(!health.status(1001).0);
    }

    #[test]
    fn fresh_post_replay_observation_covers_a_full_worker_session() {
        let health = ReconciliationHealth::default();
        health.enable();
        health.observe(observation(100));
        // Ten minutes of replay followed by nine minutes of worker work would
        // expire the pre-replay sample even though both phases are progressing.
        assert_eq!(health.status(1240), (false, "owner_observation_stale"));
        health.observe(observation(700));
        assert!(health.status(1240).0);
        // A new sample is not a fabricated durable completion.
        health.observe(observation(4000));
        assert_eq!(health.status(4000), (false, "durable_progress_stalled"));
    }
}
