#!/usr/bin/env bash
# Build a Pensieve release on the production host from a release tag.
#
#   ops/scripts/build-release.sh v2026.11.23
#
# Run as `pensieve` (it uses sudo). The tag must point at a commit on
# origin/master. The build runs in a throwaway detached worktree, inside a
# resource-limited transient unit so the running ingester keeps its CPU, memory
# and disk bandwidth, against a shared warm target dir. The binaries, Cargo.lock
# and a build-manifest.json (tag, commit, toolchain, SHA-256 of each binary) go
# into a new root-owned releases/<tag>/ that is never overwritten. Nothing is
# deployed; see ops/scripts/deploy-ingest.sh and ops/RUNBOOK.md -> Releases.
set -euo pipefail

tag=${1:?usage: build-release.sh <tag>  (e.g. v2026.11.23)}
repo=${PENSIEVE_REPO:-/home/pensieve/pensieve}
releases=${PENSIEVE_RELEASES:-$repo/releases}
build_root=${PENSIEVE_BUILD_ROOT:-/data/pensieve-build}
target_dir=$build_root/target
jobs=${PENSIEVE_BUILD_JOBS:-4}
# Binaries the production host runs. pensieve-serve and pensieve-preview are
# retired with ClickHouse (2026-11-23).
bins=(pensieve-ingest pensieve-negentropy-worker)

die() { echo "build-release: $*" >&2; exit 1; }

[[ $tag =~ ^v[0-9]{4}\.[0-9]{2}\.[0-9]{2}(\.[0-9]+)?$ ]] ||
    die "tag must look like vYYYY.MM.DD or vYYYY.MM.DD.N, got '$tag'"
dest=$releases/$tag
[[ ! -e $dest ]] || die "$dest already exists; releases are never overwritten"

git -C "$repo" fetch --quiet --tags origin
commit=$(git -C "$repo" rev-parse --verify --quiet "refs/tags/$tag^{commit}") ||
    die "no tag $tag on origin (create it with gh release create, see the RUNBOOK)"
git -C "$repo" merge-base --is-ancestor "$commit" origin/master ||
    die "$tag ($commit) is not on origin/master"

sudo install -d -o pensieve -g pensieve -m 755 "$build_root"
src=$build_root/src-$tag
stage=$build_root/stage-$tag
cleanup() {
    git -C "$repo" worktree remove --force "$src" 2>/dev/null || true
    rm -rf "$stage"
}
trap cleanup EXIT
cleanup
git -C "$repo" worktree add --quiet --detach "$src" "$commit"

echo "build-release: building $tag ($commit) with $jobs jobs"
bin_args=()
for b in "${bins[@]}"; do bin_args+=(--bin "$b"); done
# systemd-run looks the executable up in its own PATH, so name rustup's proxy in full.
sudo systemd-run --quiet --wait --pipe --collect \
    --unit="pensieve-build-${tag//./-}" \
    --uid=pensieve --gid=pensieve \
    --working-directory="$src" \
    --property=CPUQuota=$((jobs * 100))% \
    --property=MemoryMax=12G \
    --property=IOSchedulingClass=idle \
    --property=Nice=10 \
    --setenv=HOME=/home/pensieve \
    --setenv=PATH="/home/pensieve/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
    --setenv=CARGO_TARGET_DIR="$target_dir" \
    --setenv=CARGO_BUILD_JOBS="$jobs" \
    /home/pensieve/.cargo/bin/cargo build --release --locked -p pensieve-ingest "${bin_args[@]}"

mkdir -p "$stage"
for b in "${bins[@]}"; do
    install -m 755 "$target_dir/release/$b" "$stage/$b"
done
install -m 644 "$src/Cargo.lock" "$stage/Cargo.lock"
TAG=$tag COMMIT=$commit STAGE=$stage \
    RUSTC=$(/home/pensieve/.cargo/bin/rustc --version) \
    CARGO=$(/home/pensieve/.cargo/bin/cargo --version) \
    python3 -I - "${bins[@]}" <<'EOF'
import hashlib, json, os, sys, datetime
stage = os.environ["STAGE"]
digests = {}
for name in sys.argv[1:]:
    with open(os.path.join(stage, name), "rb") as f:
        digests[name] = hashlib.file_digest(f, "sha256").hexdigest()
manifest = {
    "tag": os.environ["TAG"],
    "source_commit": os.environ["COMMIT"],
    "built_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "compiler": os.environ["RUSTC"],
    "cargo": os.environ["CARGO"],
    "sha256": digests,
}
with open(os.path.join(stage, "build-manifest.json"), "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF

sudo install -d -o root -g root -m 755 "$dest"
sudo install -o root -g root -m 755 "${bins[@]/#/$stage/}" "$dest/"
sudo install -o root -g root -m 644 "$stage/Cargo.lock" "$stage/build-manifest.json" "$dest/"
cat "$dest/build-manifest.json"
echo "build-release: installed $dest (not deployed)"
