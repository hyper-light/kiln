#!/usr/bin/env bash
# Plan step 26, step 3 (kiln plan 01a1ce705e55753182217afdfe607de9), live on a Linux runner: builds kiln at
# REV, runs `kiln agentfiles context antigravity DIR --platform linux/amd64,linux/arm64`, and holds what
# it wrote to the pin: DIR holds the Agentfile and antigravity-amd64/ and antigravity-arm64/ alone, each
# with harness.json (the embedded one, byte for byte) and bin/agy (the pin's member SHA-256 and size,
# mode 0755); then this machine's own bin/agy --version prints the pin's version as its first line.
# Run from a carrier folder holding s26.bundle (`git bundle create s26.bundle origin/main..s26-unpack`):
#   bash live-run.sh REV
# Everything it makes goes under one ~/.cache directory it removes at the end.
set -u
rev=${1:?usage: live-run.sh REV}
here=$(cd "$(dirname "$0")" && pwd)
echo "START host $(hostname) $(uname -srm) at $(date -u +%FT%TZ) rev $rev load $(cut -d' ' -f1 /proc/loadavg 2>/dev/null)"
export PATH="$HOME/.cargo/bin:$PATH"
W=$(mktemp -d "$HOME/.cache/s26-live.XXXXXX") || exit 90
trap 'cd "$HOME"; rm -rf "$W"; echo "END $(date -u +%FT%TZ), removed $W"' EXIT
if [ -d "$HOME/Projects/kiln/.git" ]; then
  git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$W/kiln" || exit 91
else
  git clone -q --no-checkout https://github.com/hyper-light/kiln.git "$W/kiln" || exit 91
fi
cd "$W/kiln" || exit 92
git bundle verify -q "$here/s26.bundle" 2>/dev/null || git fetch -q https://github.com/hyper-light/kiln.git main || exit 93
git bundle verify -q "$here/s26.bundle" || exit 93
git fetch -q "$here/s26.bundle" "+refs/heads/*:refs/remotes/bundle/*" || exit 94
git -c advice.detachedHead=false checkout -q "$rev" || exit 95
echo "HEAD $(git rev-parse HEAD) dirty $(git status --porcelain | wc -l)"
[ -z "$(git status --porcelain)" ] || exit 96
A=.kiln/images/harness-antigravity
sha256sum $A/pin.json $A/harness.json $A/Agentfile crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/context.rs
cargo --version; rustc --version
export CARGO_TARGET_DIR="$W/target" CARGO_BUILD_JOBS="${KILN_NEEDS_CPUS:-8}" CARGO_PROFILE_DEV_DEBUG=line-tables-only KILN_UI_SERVER=off
t0=$(date +%s)
nice -n 10 cargo build --locked -p kiln --bin kiln 2>&1 | grep -v -E '^\s+(Compiling|Downloaded|Downloading) ' | tail -20
echo "BUILD EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
K="$W/target/debug/kiln"; [ -x "$K" ] || exit 97
D="$W/ctx"
t0=$(date +%s)
echo "== kiln agentfiles context antigravity $D --platform linux/amd64,linux/arm64"
"$K" agentfiles context antigravity "$D" --platform linux/amd64,linux/arm64; rc=$?
echo "CONTEXT EXIT $rc after $(( $(date +%s) - t0 )) s (one observation)"
[ "$rc" = 0 ] || exit 98
echo "context top level: $(ls -A "$D" | tr '\n' ' ')"
cmp "$D/Agentfile" $A/Agentfile && echo "Agentfile: the embedded one"
for arch in amd64 arm64; do
  h="$D/antigravity-$arch"
  echo "== antigravity-$arch: $(ls -A "$h" | tr '\n' ' '); bin: $(ls -A "$h/bin" | tr '\n' ' ')"
  find "$h" -mindepth 1 -printf '%M %m %s %P\n' | sort -k4
  cmp "$h/harness.json" $A/harness.json && echo "harness.json: the embedded one"
  sha256sum "$h/bin/agy"; file "$h/bin/agy" 2>/dev/null | cut -c1-160
done
python3 -I - "$D" $A/pin.json <<'PY'
import hashlib, json, os, stat, sys
d, pin = sys.argv[1], json.load(open(sys.argv[2]))
ok = 0
for b in pin["builds"]:
    arch = b["platform"].split("/")[1]
    u = b["files"][0]["unpack"]
    p = os.path.join(d, f"antigravity-{arch}", "bin", "agy")
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    st = os.stat(p)
    good = h.hexdigest() == u["sha256"] and st.st_size == u["size"] and stat.S_IMODE(st.st_mode) == 0o755 and u.get("alone") is True
    ok += good
    print(f"{'MATCH' if good else 'DIFFERS'} {b['platform']} sha256 {h.hexdigest()} (pin {u['sha256']}) size {st.st_size} (pin {u['size']}) mode {stat.S_IMODE(st.st_mode):o} alone {u.get('alone')}")
print(f"binaries matching the pin's member: {ok} of {len(pin['builds'])}")
sys.exit(0 if ok == len(pin["builds"]) else 1)
PY
echo "PIN CHECK EXIT $?"
own=$(uname -m); case $own in x86_64) own=amd64 ;; aarch64) own=arm64 ;; esac
home=$(mktemp -d "$W/home.XXXXXX")
out=$(HOME="$home" AGY_CLI_DISABLE_AUTO_UPDATE=true "$D/antigravity-$own/bin/agy" --version 2>&1); rc=$?
printf 'antigravity-%s/bin/agy --version: exit %s, first line %s, bytes %s\n' "$own" "$rc" "$(printf '%s\n' "$out" | head -1)" "$(printf '%s\n' "$out" | head -1 | od -An -tx1 | tr -s ' ')"
want=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' $A/pin.json)
[ "$rc" = 0 ] && [ "$(printf '%s\n' "$out" | head -1)" = "$want" ] && echo "VERSION MATCH $want" || echo "VERSION DIFFERS (pin $want)"
echo "DISK $(df -h "$HOME/.cache" | tail -1); target $(du -sh "$W/target" 2>/dev/null | cut -f1)"
