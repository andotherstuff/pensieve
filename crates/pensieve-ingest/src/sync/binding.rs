//! Fail-closed Linux worker identity inspection. Never launches a worker or
//! grants a lease. A snapshot is not a cached admission permission.

use std::time::Duration;

#[cfg(any(target_os = "linux", test))]
use serde::Deserialize;
#[cfg(any(target_os = "linux", test))]
use serde_json::Value;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::WorkerCandidate;
#[cfg(target_os = "linux")]
mod query;

#[cfg(any(target_os = "linux", test))]
const UNIT: &str = "pensieve-negentropy-worker.service";
#[cfg(any(target_os = "linux", test))]
const WORKER_CGROUP: &str = "/system.slice/pensieve-negentropy-worker.service";
#[cfg(target_os = "linux")]
const PARENT_CGROUP: &str = "/system.slice/pensieve-ingest.service";
#[cfg(any(target_os = "linux", test))]
const JOB_US: u64 = 540_000_000;
// Match the inventory/fetch progress allowance as a minimum useful opportunity,
// not a promise that the five-minute comparison allowance fits. A parent-cut
// attempt remains incomplete and consumes paced retry budget; slow replays
// must be watched through the admitted session_seconds log during rollout.
#[cfg(any(target_os = "linux", test))]
const MIN_JOB_US: u64 = 120_000_000;
#[cfg(any(target_os = "linux", test))]
const MARGIN_US: u64 = 10_000_000;
#[cfg(any(target_os = "linux", test))]
const OUTPUT_LIMIT: usize = 16 * 1024;
#[cfg(target_os = "linux")]
const QUERY_TIME: Duration = Duration::from_secs(2);

/// Fail-closed binding failure; subprocess output is deliberately not exposed.
#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    /// Platform/kernel does not provide the required proof.
    #[error("worker binding capability unavailable")]
    Unavailable,
    /// Another bounded inspection is still running or cleaning up.
    #[error("worker inspection busy")]
    Busy,
    /// Inspection deadline or caller cancellation.
    #[error("worker inspection cancelled or timed out")]
    Deadline,
    /// Untrusted/inconsistent identity or malformed manager response.
    #[error("worker identity mismatch")]
    Identity,
    /// Minimum useful session plus safety margin no longer fits.
    #[error("worker service lifetime insufficient")]
    Lifetime,
    /// Bounded output limit was exceeded.
    #[error("worker inspection output exceeded limit")]
    Limit,
    /// Local I/O failed; no worker fault classification is implied.
    #[error("worker inspection I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Non-Linux builds cannot authenticate a systemd worker.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct WorkerCandidate;

#[cfg(all(unix, not(target_os = "linux")))]
impl WorkerCandidate {
    /// Always fails closed on this platform; does not contact the peer.
    pub async fn inspect(
        socket: &tokio::net::UnixStream,
        worker_uid: u32,
    ) -> Result<Self, BindingError> {
        let _ = (socket, worker_uid);
        Err(BindingError::Unavailable)
    }

    /// Always fails closed on this platform.
    pub async fn recheck(&self) -> Result<Duration, BindingError> {
        Err(BindingError::Unavailable)
    }
}

/// How the isolated runtime proves an accepted socket is the dedicated worker
/// and how long it may still serve. Production always uses [`WorkerCandidate`];
/// the trait exists so runtime tests can bind a same-UID peer on every
/// platform without systemd, while production checks stay unconditional.
#[cfg(unix)]
pub(crate) trait WorkerBinding: Sized + Send + Sync + 'static {
    /// Authenticate the accepted socket before exporting any inventory or lease.
    fn inspect(
        socket: &tokio::net::UnixStream,
        worker_uid: u32,
    ) -> impl Future<Output = Result<Self, BindingError>> + Send;

    /// Revalidate immediately before assignment; returns the usable session time.
    fn recheck(&self) -> impl Future<Output = Result<Duration, BindingError>> + Send;
}

#[cfg(unix)]
impl WorkerBinding for WorkerCandidate {
    fn inspect(
        socket: &tokio::net::UnixStream,
        worker_uid: u32,
    ) -> impl Future<Output = Result<Self, BindingError>> + Send {
        WorkerCandidate::inspect(socket, worker_uid)
    }

    fn recheck(&self) -> impl Future<Output = Result<Duration, BindingError>> + Send {
        WorkerCandidate::recheck(self)
    }
}

/// Test-only binding: the peer's UID must equal the configured worker UID (as
/// the real check requires first), and the service lifetime is a fixed full
/// session. It replaces only the systemd unit, cgroup and pidfd proof, which a
/// CI runner cannot provide; it is never compiled into a release binary.
#[cfg(all(unix, test))]
#[derive(Debug)]
pub(crate) struct SameUidPeer;

#[cfg(all(unix, test))]
impl WorkerBinding for SameUidPeer {
    async fn inspect(
        socket: &tokio::net::UnixStream,
        worker_uid: u32,
    ) -> Result<Self, BindingError> {
        if socket.peer_cred()?.uid() == worker_uid {
            Ok(Self)
        } else {
            Err(BindingError::Identity)
        }
    }

    async fn recheck(&self) -> Result<Duration, BindingError> {
        Ok(Duration::from_micros(JOB_US))
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    invocation: [u8; 16],
    pid: u32,
    active_us: u64,
    runtime_us: u64,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Property {
    #[serde(rename = "type")]
    signature: String,
    data: Value,
}

#[cfg(any(target_os = "linux", test))]
fn properties(bytes: &[u8], signatures: &[&str]) -> Result<Vec<Value>, BindingError> {
    if bytes.len() > OUTPUT_LIMIT {
        return Err(BindingError::Limit);
    }
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<Property>();
    let mut values = Vec::with_capacity(signatures.len());
    for expected in signatures {
        let property = stream
            .next()
            .ok_or(BindingError::Identity)?
            .map_err(|_| BindingError::Identity)?;
        if property.signature != *expected {
            return Err(BindingError::Identity);
        }
        values.push(property.data);
    }
    if stream.next().is_some() {
        return Err(BindingError::Identity);
    }
    Ok(values)
}

#[cfg(any(target_os = "linux", test))]
fn parse(unit: &[u8], service: &[u8]) -> Result<Snapshot, BindingError> {
    let unit = properties(unit, &["s", "s", "s", "ay", "t"])?;
    let service = properties(service, &["s", "u", "s", "t"])?;
    if unit[0].as_str() != Some(UNIT)
        || unit[1].as_str() != Some("active")
        || unit[2].as_str() != Some("running")
        || service[0].as_str() != Some("simple")
        || service[2].as_str() != Some(WORKER_CGROUP)
    {
        return Err(BindingError::Identity);
    }
    let id = unit[3].as_array().ok_or(BindingError::Identity)?;
    if id.len() != 16 {
        return Err(BindingError::Identity);
    }
    let mut invocation = [0; 16];
    for (output, input) in invocation.iter_mut().zip(id) {
        *output = input
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .ok_or(BindingError::Identity)?;
    }
    let snapshot = Snapshot {
        invocation,
        pid: service[1]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(BindingError::Identity)?,
        active_us: unit[4].as_u64().ok_or(BindingError::Identity)?,
        runtime_us: service[3].as_u64().ok_or(BindingError::Identity)?,
    };
    if snapshot.invocation == [0; 16]
        || snapshot.pid == 0
        || snapshot.active_us == 0
        || snapshot.runtime_us != 600_000_000
    {
        return Err(BindingError::Identity);
    }
    Ok(snapshot)
}

#[cfg(any(target_os = "linux", test))]
impl Snapshot {
    fn check(&self, expected: &Self, pid: u32, now_us: u64) -> Result<Duration, BindingError> {
        if self != expected || self.pid != pid || now_us < self.active_us {
            return Err(BindingError::Identity);
        }
        let deadline = self
            .active_us
            .checked_add(self.runtime_us)
            .ok_or(BindingError::Lifetime)?;
        let remaining = deadline
            .checked_sub(now_us)
            .and_then(|remaining| remaining.checked_sub(MARGIN_US))
            .ok_or(BindingError::Lifetime)?;
        if remaining < MIN_JOB_US {
            return Err(BindingError::Lifetime);
        }
        Ok(Duration::from_micros(remaining.min(JOB_US)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs() -> (Vec<u8>, Vec<u8>) {
        let unit = format!(
            "{{\"type\":\"s\",\"data\":\"{UNIT}\"}}\n{{\"type\":\"s\",\"data\":\"active\"}}\n{{\"type\":\"s\",\"data\":\"running\"}}\n{{\"type\":\"ay\",\"data\":[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}}\n{{\"type\":\"t\",\"data\":1000000}}"
        );
        let service = format!(
            "{{\"type\":\"s\",\"data\":\"simple\"}}\n{{\"type\":\"u\",\"data\":123}}\n{{\"type\":\"s\",\"data\":\"{WORKER_CGROUP}\"}}\n{{\"type\":\"t\",\"data\":600000000}}"
        );
        (unit.into_bytes(), service.into_bytes())
    }

    #[test]
    fn ordered_property_parser_rejects_missing_extra_wrong_type_and_oversize() {
        let (unit, service) = outputs();
        assert_eq!(parse(&unit, &service).unwrap().pid, 123);
        assert!(parse(&unit[..unit.len() - 1], &service).is_err());
        assert!(parse(&[unit.as_slice(), b"{}"].concat(), &service).is_err());
        assert!(parse(&service, &unit).is_err());
        assert!(parse(&vec![b' '; OUTPUT_LIMIT + 1], &service).is_err());
        let wrong_cgroup = String::from_utf8(service.clone())
            .unwrap()
            .replace(WORKER_CGROUP, "/system.slice/other.service");
        assert!(parse(&unit, wrong_cgroup.as_bytes()).is_err());
        for (from, to) in [
            ("active", "inactive"),
            ("running", "exited"),
            ("[1,", "[0,"),
        ] {
            let bad = String::from_utf8(unit.clone()).unwrap().replace(from, to);
            assert!(parse(bad.as_bytes(), &service).is_err());
        }
    }

    #[test]
    fn replacement_pid_invocation_and_lifetime_fail_closed() {
        let (unit, service) = outputs();
        let snapshot = parse(&unit, &service).unwrap();
        assert!(snapshot.check(&snapshot, 123, 51_000_000).is_ok());
        assert!(matches!(
            snapshot.check(&snapshot, 123, 471_000_001),
            Err(BindingError::Lifetime)
        ));
        assert!(snapshot.check(&snapshot, 124, 1_000_000).is_err());
        assert!(snapshot.check(&snapshot, 123, 999_999).is_err());
        let mut replacement = snapshot.clone();
        replacement.invocation[1] = 1;
        assert!(replacement.check(&snapshot, 123, 1_000_000).is_err());
        replacement = snapshot.clone();
        replacement.active_us = u64::MAX;
        assert!(replacement.check(&replacement, 123, u64::MAX).is_err());
    }

    #[test]
    fn replay_time_reduces_session_budget_without_rejecting_healthy_worker() {
        let (unit, service) = outputs();
        let snapshot = parse(&unit, &service).unwrap();
        for (age_seconds, budget_seconds) in
            [(0, 540), (50, 540), (82, 508), (300, 290), (470, 120)]
        {
            assert_eq!(
                snapshot
                    .check(&snapshot, 123, snapshot.active_us + age_seconds * 1_000_000)
                    .unwrap(),
                Duration::from_secs(budget_seconds)
            );
        }
        for age_seconds in [471, 590, 600, 601] {
            assert!(matches!(
                snapshot.check(&snapshot, 123, snapshot.active_us + age_seconds * 1_000_000),
                Err(BindingError::Lifetime)
            ));
        }
    }
}
