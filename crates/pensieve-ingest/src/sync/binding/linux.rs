//! Linux-only peer identity and service lifetime proof for an accepted socket.

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};

use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::socket::{getsockopt, sockopt::PeerPidfd};
use nix::time::{ClockId, clock_gettime};
use tokio::net::UnixStream;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::query;
use super::{BindingError, PARENT_CGROUP, QUERY_TIME, Snapshot, WORKER_CGROUP};

static INSPECTION: Semaphore = Semaphore::const_new(1);

/// A connected worker proved against its dedicated service, retaining the
/// kernel pidfd for the entire attempt. It is not a reusable lease permission.
#[derive(Debug)]
pub struct WorkerCandidate {
    pidfd: OwnedFd,
    pid: u32,
    initial: Snapshot,
}

impl WorkerCandidate {
    /// Authenticate the accepted socket before exporting any inventory or lease.
    /// Both systemd reads must agree because multi-property busctl output is not
    /// atomic; the kernel pidfd and cgroup checks bracket those reads.
    pub async fn inspect(socket: &UnixStream, worker_uid: u32) -> Result<Self, BindingError> {
        let _permit = INSPECTION.try_acquire().map_err(|_| BindingError::Busy)?;
        let credentials = socket.peer_cred()?;
        let pid = credentials
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid != 0)
            .ok_or(BindingError::Identity)?;
        if credentials.uid() != worker_uid {
            return Err(BindingError::Identity);
        }
        let pidfd = getsockopt(socket, PeerPidfd).map_err(|_| BindingError::Unavailable)?;
        let candidate = Self {
            pidfd,
            pid,
            initial: Snapshot {
                invocation: [0; 16],
                pid: 0,
                active_us: 0,
                runtime_us: 0,
            },
        };
        candidate.check_process()?;
        let until = Instant::now() + QUERY_TIME;
        let initial = query::snapshot(until).await?;
        let candidate = Self {
            initial,
            ..candidate
        };
        candidate.verify(until).await?;
        Ok(candidate)
    }

    /// Recheck immediately before assignment, and again as needed while the
    /// parent owns the socket. A changed invocation or exhausted runtime fails.
    pub async fn recheck(&self) -> Result<(), BindingError> {
        let _permit = INSPECTION.try_acquire().map_err(|_| BindingError::Busy)?;
        self.check_process()?;
        let until = Instant::now() + QUERY_TIME;
        let before = query::snapshot(until).await?;
        before.check(&self.initial, self.pid, monotonic_us()?)?;
        self.verify(until).await
    }

    async fn verify(&self, until: Instant) -> Result<(), BindingError> {
        let current = query::snapshot(until).await?;
        self.check_process()?;
        current.check(&self.initial, self.pid, monotonic_us()?)
    }

    fn check_process(&self) -> Result<(), BindingError> {
        if !pidfd_alive(&self.pidfd)? {
            return Err(BindingError::Identity);
        }
        if cgroup("/proc/self/cgroup")? != PARENT_CGROUP
            || cgroup(&format!("/proc/{}/cgroup", self.pid))? != WORKER_CGROUP
        {
            return Err(BindingError::Identity);
        }
        if !pidfd_alive(&self.pidfd)? {
            return Err(BindingError::Identity);
        }
        Ok(())
    }
}

fn pidfd_alive(fd: &OwnedFd) -> Result<bool, BindingError> {
    let mut descriptors = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    let ready = poll(&mut descriptors, 0u8).map_err(|_| BindingError::Unavailable)?;
    Ok(ready == 0 && descriptors[0].revents() == Some(PollFlags::empty()))
}

fn cgroup(path: &str) -> Result<String, BindingError> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(BindingError::Limit);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| BindingError::Identity)?;
    parse_cgroup(text)
}

fn parse_cgroup(text: &str) -> Result<String, BindingError> {
    let line = text.strip_suffix('\n').unwrap_or(text);
    let group = line.strip_prefix("0::").ok_or(BindingError::Identity)?;
    if group.is_empty() || group.contains('\n') {
        return Err(BindingError::Identity);
    }
    Ok(group.to_string())
}

fn monotonic_us() -> Result<u64, BindingError> {
    let now = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(|_| BindingError::Unavailable)?;
    let seconds = u64::try_from(now.tv_sec()).map_err(|_| BindingError::Identity)?;
    let nanos = u64::try_from(now.tv_nsec()).map_err(|_| BindingError::Identity)?;
    seconds
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(nanos / 1_000))
        .ok_or(BindingError::Identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_descriptor_fails_liveness_check() {
        let (reader, writer) = nix::unistd::pipe().unwrap();
        drop(writer);
        assert!(!pidfd_alive(&reader).unwrap());
    }

    #[test]
    fn requires_single_unified_cgroup_record() {
        assert_eq!(
            parse_cgroup("0::/system.slice/test.service\n").unwrap(),
            "/system.slice/test.service"
        );
        for bad in ["", "1:name=systemd:/x\n", "0::\n", "0::/x\n0::/y\n"] {
            assert!(parse_cgroup(bad).is_err());
        }
    }

    #[tokio::test]
    async fn wrong_uid_is_rejected_before_systemd_query() {
        let (socket, _) = UnixStream::pair().unwrap();
        let wrong_uid = socket.peer_cred().unwrap().uid().wrapping_add(1);
        assert!(matches!(
            WorkerCandidate::inspect(&socket, wrong_uid).await,
            Err(BindingError::Identity)
        ));
    }
}
