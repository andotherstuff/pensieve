//! Fixed, bounded systemd property reads. Never accepts a caller-supplied bus
//! name, object path, interface, or executable.

use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::{Instant, timeout_at};

use super::{BindingError, OUTPUT_LIMIT};

const BUSCTL: &str = "/usr/bin/busctl";
const DESTINATION: &str = "org.freedesktop.systemd1";
const OBJECT: &str = "/org/freedesktop/systemd1/unit/pensieve_2dnegentropy_2dworker_2eservice";

// Dropping a query, including caller cancellation, kills and asynchronously
// reaps its child. No successful output is accepted before the child exits 0.
struct Reap(Option<Child>);

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

async fn call(
    interface: &str,
    properties: &[&str],
    until: Instant,
) -> Result<Vec<u8>, BindingError> {
    let mut command = Command::new(BUSCTL);
    command
        .arg("--json=short")
        .arg("--no-pager")
        .arg("get-property")
        .arg(DESTINATION)
        .arg(OBJECT)
        .arg(interface)
        .args(properties)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    capture(command.spawn()?, until).await
}

async fn capture(process: Child, until: Instant) -> Result<Vec<u8>, BindingError> {
    let mut child = Reap(Some(process));
    let mut stdout = child
        .0
        .as_mut()
        .ok_or(BindingError::Identity)?
        .stdout
        .take()
        .ok_or(BindingError::Identity)?;
    let read = async {
        let mut bytes = Vec::with_capacity(1024);
        let mut chunk = [0; 1024];
        loop {
            let count = stdout.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            if bytes.len().saturating_add(count) > OUTPUT_LIMIT {
                return Err(BindingError::Limit);
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        let status = child
            .0
            .as_mut()
            .ok_or(BindingError::Identity)?
            .wait()
            .await?;
        if !status.success() {
            return Err(BindingError::Identity);
        }
        child.0.take();
        Ok(bytes)
    };
    timeout_at(until, read)
        .await
        .map_err(|_| BindingError::Deadline)?
}

/// Read the two fixed interfaces before the shared absolute deadline.
pub(super) async fn snapshot(until: Instant) -> Result<super::Snapshot, BindingError> {
    let unit = call(
        "org.freedesktop.systemd1.Unit",
        &[
            "Id",
            "ActiveState",
            "SubState",
            "InvocationID",
            "ActiveEnterTimestampMonotonic",
            "ControlGroup",
        ],
        until,
    )
    .await?;
    let service = call(
        "org.freedesktop.systemd1.Service",
        &["Type", "MainPID", "RuntimeMaxUSec"],
        until,
    )
    .await?;
    super::parse(&unit, &service)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn output_limit_kills_unbounded_child() {
        let mut command = Command::new("/usr/bin/head");
        command
            .arg("-c")
            .arg("20000")
            .arg("/dev/zero")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let result = capture(
            command.spawn().unwrap(),
            Instant::now() + Duration::from_secs(2),
        )
        .await;
        assert!(matches!(result, Err(BindingError::Limit)));
    }

    #[tokio::test]
    async fn absolute_deadline_kills_and_reaps_child() {
        let mut command = Command::new("/usr/bin/sleep");
        command
            .arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let process = command.spawn().unwrap();
        let pid = process.id().unwrap();
        let result = capture(process, Instant::now() + Duration::from_millis(20)).await;
        assert!(matches!(result, Err(BindingError::Deadline)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    // Run only on a disposable systemd test host with the real static unit
    // already active. This reads the actual manager interfaces, so a fixture
    // cannot conceal a property/interface mismatch.
    #[tokio::test]
    #[ignore = "requires marked disposable systemd host and active worker unit"]
    async fn live_systemd_property_contract() {
        assert!(std::path::Path::new("/etc/pensieve-isolation-test-host").exists());
        let snapshot = snapshot(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        assert_ne!(snapshot.invocation, [0; 16]);
        assert_ne!(snapshot.pid, 0);
        assert_eq!(snapshot.runtime_us, 600_000_000);
    }
}
