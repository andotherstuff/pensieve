#!/usr/bin/env bash
# Deploy (or roll back to) an ingester release built by build-release.sh.
#
#   ops/scripts/deploy-ingest.sh v2026.11.23
#
# Run as `pensieve` (it uses sudo). Steps, stopping at the first failure:
#   1. verify releases/<tag>/ against its build-manifest.json;
#   2. refuse if the unit does not start releases/current (a drop-in overrides
#      ExecStart) or an archive recovery marker exists;
#   3. stop the negentropy worker, then the ingester, and require a graceful
#      stop (Result=success, exit 0, "shutdown complete" in the journal). On a
#      failed stop it exits without starting anything: investigate first;
#   4. install the release's worker to /usr/local/libexec (keeping the old one
#      beside it), switch releases/current to <tag> atomically, start the
#      ingester (which pulls in the worker);
#   5. check the running binary, the startup log, archive admission and the worker.
# Rolling back is the same command with the previous tag, which it prints.
set -euo pipefail

tag=${1:?usage: deploy-ingest.sh <tag>}
releases=${PENSIEVE_RELEASES:-/home/pensieve/pensieve/releases}
libexec=/usr/local/libexec
segments=/archive/segments
admission=http://127.0.0.1:9091/health/archive-admission
dest=$releases/$tag
unit=pensieve-ingest
worker=pensieve-negentropy-worker

die() { echo "deploy-ingest: $*" >&2; exit 1; }
say() { echo "deploy-ingest: $*"; }

# 1. The release is intact.
[[ -f $dest/build-manifest.json ]] || die "no $dest/build-manifest.json"
python3 -I - "$dest" <<'EOF' || die "release does not match its manifest"
import hashlib, json, os, sys
dest = sys.argv[1]
manifest = json.load(open(os.path.join(dest, "build-manifest.json")))
for name in ("pensieve-ingest", "pensieve-negentropy-worker"):
    with open(os.path.join(dest, name), "rb") as f:
        actual = hashlib.file_digest(f, "sha256").hexdigest()
    if manifest["sha256"].get(name) != actual:
        sys.exit(f"{name}: manifest {manifest['sha256'].get(name)} != {actual}")
print(f"release {manifest.get('tag', os.path.basename(dest))} from {manifest['source_commit']} ({manifest['compiler']})")
EOF

# 2. The unit runs releases/current, nothing overrides it, no recovery is pending.
exec_start=$(systemctl show "$unit" -p ExecStart --value)
[[ $exec_start == *"path=$releases/current/pensieve-ingest "* ]] ||
    die "$unit does not start $releases/current/pensieve-ingest; install ops/systemd/$unit.service and remove ExecStart drop-ins first"
shopt -s nullglob
markers=("$segments"/*.recovery-required)
shopt -u nullglob
(( ${#markers[@]} == 0 )) || die "archive recovery marker present: ${markers[*]} (docs/archive_failure_recovery.md)"
previous=$(readlink "$releases/current" 2>/dev/null || true)
[[ $previous != "$tag" ]] || die "$tag is already current"
if systemctl is-active --quiet "$unit"; then
    code=$(curl -s -o /dev/null -w '%{http_code}' "$admission" || true)
    [[ $code == 200 ]] || die "archive admission answers $code before the stop; investigate first"
fi

# 3. Graceful stop: worker first, then the ingester.
since=$(date '+%Y-%m-%d %H:%M:%S')
say "stopping $worker and $unit (up to TimeoutStopSec)"
sudo systemctl stop "$worker"
sudo systemctl stop "$unit"
result=$(systemctl show "$unit" -p Result --value)
status=$(systemctl show "$unit" -p ExecMainStatus --value)
if [[ $result != success || $status != 0 ]] ||
    ! sudo journalctl -u "$unit" --since "$since" --no-pager -q | grep -q 'shutdown complete'; then
    die "stop was not graceful (Result=$result ExecMainStatus=$status); nothing was started. Check the journal and the RUNBOOK's graceful-stop checks, then start $unit by hand"
fi
say "graceful stop confirmed"

# 4. Worker binary, release link, start.
if [[ -e $libexec/$worker ]]; then
    sudo cp -p "$libexec/$worker" "$libexec/$worker.${previous:-pre-$tag}"
fi
sudo install -o root -g root -m 755 "$dest/$worker" "$libexec/$worker"
sudo ln -sfn "$tag" "$releases/.current.tmp"
sudo mv -Tf "$releases/.current.tmp" "$releases/current"
say "releases/current -> $tag (was ${previous:-unset})"
started=$(date '+%Y-%m-%d %H:%M:%S')
sudo systemctl reset-failed "$unit" 2>/dev/null || true
sudo systemctl start "$unit"

# 5. Verify.
ok=0
for _ in $(seq 1 24); do
    sleep 5
    systemctl is-active --quiet "$unit" || continue
    pid=$(systemctl show "$unit" -p MainPID --value)
    exe=$(sudo readlink "/proc/$pid/exe" || true)
    code=$(curl -s -o /dev/null -w '%{http_code}' "$admission" || true)
    if [[ $exe == "$dest/pensieve-ingest" && $code == 200 ]] &&
        sudo journalctl -u "$unit" --since "$started" --no-pager -q | grep -q 'ingest configuration' &&
        systemctl is-active --quiet "$worker"; then
        ok=1
        break
    fi
done
sudo journalctl -u "$unit" --since "$started" --no-pager -q | grep 'ingest configuration' | tail -1 || true
if (( ok )); then
    say "OK: $unit runs $exe, admission 200, $worker active"
else
    say "NOT VERIFIED after 2 minutes: active=$(systemctl is-active "$unit") exe=${exe:-?} admission=${code:-?} worker=$(systemctl is-active "$worker")"
fi
[[ -n $previous ]] && say "roll back with: ops/scripts/deploy-ingest.sh $previous"
(( ok ))
