#!/bin/sh
# Disposable Linux-host rehearsal only. This never starts/stops Pensieve units,
# opens archives, or exercises real leases. Run the durable-ledger fault gate
# separately before any production activation.
set -eu

if [ ! -e /etc/pensieve-isolation-test-host ] || [ ! -d /run/systemd/system ]; then
    echo 'Refusing: disposable systemd test-host marker is required' >&2
    exit 2
fi

parent="pensieve-isolation-test-parent-$$.service"
worker="pensieve-isolation-test-worker-$$.service"
cleanup() {
    systemctl stop "$worker" "$parent" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

systemd-run --quiet --unit="$parent" --property=Type=simple \
    /usr/bin/sleep 120
attempt=0
parent_pid=0
while [ "$parent_pid" -eq 0 ] && [ "$attempt" -lt 10 ]; do
    parent_pid=$(systemctl show "$parent" --property=MainPID --value)
    attempt=$((attempt + 1))
    sleep 1
done
parent_restarts=$(systemctl show "$parent" --property=NRestarts --value)
test "$parent_pid" -gt 0

check_parent() {
    test "$(systemctl show "$parent" --property=MainPID --value)" = "$parent_pid"
    test "$(systemctl show "$parent" --property=NRestarts --value)" = "$parent_restarts"
    attempt=0
    while [ -d "/sys/fs/cgroup/system.slice/$worker" ] && [ "$attempt" -lt 10 ]; do
        attempt=$((attempt + 1))
        sleep 1
    done
    test ! -d "/sys/fs/cgroup/system.slice/$worker"
}

# Contained synthetic allocation pressure. A worker OOM is expected; an OOM
# failure in this rehearsal must never be interpreted as job completion.
if systemd-run --quiet --wait --collect --unit="$worker" \
    --property=Type=simple --property=MemoryMax=64M \
    --property=MemorySwapMax=0 --property=OOMPolicy=kill \
    /usr/bin/python3 -c 'x=bytearray(256*1024*1024); x[0]=1'; then
    echo 'Synthetic worker unexpectedly survived memory limit' >&2
    exit 1
fi
check_parent

# A non-yielding worker must be reaped by its own service runtime ceiling.
if systemd-run --quiet --wait --collect --unit="$worker" \
    --property=Type=simple --property=RuntimeMaxSec=4s \
    --property=TimeoutStopSec=1s --property=KillMode=control-group \
    /usr/bin/sleep 60; then
    echo 'Synthetic worker unexpectedly survived runtime limit' >&2
    exit 1
fi
check_parent

# A separately signalled worker must leave no child or cgroup behind.
systemd-run --quiet --unit="$worker" --property=Type=simple \
    --property=KillMode=control-group /usr/bin/sleep 60
systemctl kill "$worker"
attempt=0
while systemctl is-active --quiet "$worker" && [ "$attempt" -lt 10 ]; do
    attempt=$((attempt + 1))
    sleep 1
done
test "$attempt" -lt 10
check_parent
echo 'Synthetic worker-only OOM, hang and kill left parent unchanged'
