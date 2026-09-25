#!/bin/sh
# Read-only property/interface proof on a disposable systemd 257 test host.
# The static pensieve-negentropy-worker.service must already be active there.
set -eu

if [ ! -e /etc/pensieve-isolation-test-host ] || [ ! -d /run/systemd/system ]; then
    echo 'Refusing: disposable systemd test-host marker is required' >&2
    exit 2
fi
if ! systemctl --version | head -n 1 | grep -q '^systemd 257 '; then
    echo 'Refusing: systemd 257 is required for this contract proof' >&2
    exit 2
fi
if ! systemctl is-active --quiet pensieve-negentropy-worker.service; then
    echo 'Refusing: the static worker unit must already be active' >&2
    exit 2
fi

cargo test -p pensieve-ingest \
    sync::binding::query::tests::live_systemd_property_contract \
    -- --ignored --exact
