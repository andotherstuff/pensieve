//! Fail-closed Linux worker identity inspection. Never launches a worker or
//! grants a lease. A snapshot is not a cached admission permission.

#[cfg(target_os = "linux")]
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
    /// Full job plus safety margin no longer fits.
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
        #[cfg(test)]
        {
            if socket.peer_cred()?.uid() == worker_uid {
                return Ok(Self);
            }
            Err(BindingError::Identity)
        }
        #[cfg(not(test))]
        {
            let _ = (socket, worker_uid);
            Err(BindingError::Unavailable)
        }
    }

    /// Always fails closed on this platform.
    pub async fn recheck(&self) -> Result<(), BindingError> {
        #[cfg(test)]
        {
            Ok(())
        }
        #[cfg(not(test))]
        {
            Err(BindingError::Unavailable)
        }
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
    let unit = properties(unit, &["s", "s", "s", "ay", "t", "s"])?;
    let service = properties(service, &["s", "u", "t"])?;
    if unit[0].as_str() != Some(UNIT)
        || unit[1].as_str() != Some("active")
        || unit[2].as_str() != Some("running")
        || service[0].as_str() != Some("simple")
        || unit[5].as_str() != Some(WORKER_CGROUP)
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
        runtime_us: service[2].as_u64().ok_or(BindingError::Identity)?,
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
    fn check(&self, expected: &Self, pid: u32, now_us: u64) -> Result<(), BindingError> {
        if self != expected || self.pid != pid || now_us < self.active_us {
            return Err(BindingError::Identity);
        }
        let deadline = self
            .active_us
            .checked_add(self.runtime_us)
            .ok_or(BindingError::Lifetime)?;
        if now_us
            .checked_add(JOB_US + MARGIN_US)
            .is_none_or(|end| end > deadline)
        {
            return Err(BindingError::Lifetime);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs() -> (Vec<u8>, Vec<u8>) {
        let unit = format!(
            "{{\"type\":\"s\",\"data\":\"{UNIT}\"}}\n{{\"type\":\"s\",\"data\":\"active\"}}\n{{\"type\":\"s\",\"data\":\"running\"}}\n{{\"type\":\"ay\",\"data\":[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}}\n{{\"type\":\"t\",\"data\":1000000}}\n{{\"type\":\"s\",\"data\":\"{WORKER_CGROUP}\"}}"
        );
        let service = "{\"type\":\"s\",\"data\":\"simple\"}\n{\"type\":\"u\",\"data\":123}\n{\"type\":\"t\",\"data\":600000000}"
            .to_owned();
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
            snapshot.check(&snapshot, 123, 51_000_001),
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
}
