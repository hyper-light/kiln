#!/usr/bin/env bash
# Plan step 26, step 3 (kiln plan 01a1ce705e55753182217afdfe607de9): the step's live test word for word, at
# a commit of kiln's main, on a Linux machine: kiln itself built, then
#   kiln agentfiles context antigravity DIR --platform linux/amd64,linux/arm64
# and what it wrote held to the done-condition: DIR holds the Agentfile (the embedded one) and
# antigravity-amd64/ and antigravity-arm64/ alone; each holds harness.json (the embedded one) and
# bin/agy, mode 0755, hashing to the pin's member SHA-256 (sha256sum); then this machine's own
# antigravity-<arch>/bin/agy --version prints the pin's version as its first line.
#   bash cli-run.sh REV
# Everything it makes is under one ~/.cache directory, removed at the end; the log is printed.
set -u
rev=${1:?usage: cli-run.sh REV}
echo "START host $(hostname) $(uname -srm) at $(date -u +%FT%TZ) rev $rev load $(cut -d' ' -f1 /proc/loadavg 2>/dev/null)"
export PATH="$HOME/.cargo/bin:$PATH"
s256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
W=$(mktemp -d "$HOME/.cache/s26-cli.XXXXXX") || exit 90
trap 'cd "$HOME"; rm -rf "$W"; echo "END $(date -u +%FT%TZ), removed $W"' EXIT
if [ -d "$HOME/Projects/kiln/.git" ]; then
  git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$W/kiln" || exit 91
else
  git clone -q --no-checkout https://github.com/hyper-light/kiln.git "$W/kiln" || exit 91
fi
cd "$W/kiln" || exit 92
git cat-file -e "$rev^{commit}" 2>/dev/null || git fetch -q https://github.com/hyper-light/kiln.git main || exit 93
git -c advice.detachedHead=false checkout -q "$rev" || exit 94
echo "HEAD $(git rev-parse HEAD) dirty $(git status --porcelain | wc -l)"
[ -z "$(git status --porcelain)" ] || exit 95
A=.kiln/images/harness-antigravity
s256 $A/pin.json $A/harness.json $A/Agentfile crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/context.rs
cargo --version; rustc --version
export CARGO_TARGET_DIR="$W/target" CARGO_BUILD_JOBS="${JOBS:-8}" CARGO_PROFILE_DEV_DEBUG=line-tables-only KILN_UI_SERVER=off
t0=$(date +%s)
nice -n 10 cargo build --locked -p kiln --bin kiln 2>&1 | grep -v -E '^\s+(Compiling|Downloaded|Downloading) ' | tail -5
echo "BUILD EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
K="$W/target/debug/kiln"; [ -x "$K" ] || exit 96
D="$W/ctx"
echo "== $K agentfiles context antigravity $D --platform linux/amd64,linux/arm64"
t0=$(date +%s)
"$K" agentfiles context antigravity "$D" --platform linux/amd64,linux/arm64; rc=$?
echo "CONTEXT EXIT $rc after $(( $(date +%s) - t0 )) s (one observation)"
[ "$rc" = 0 ] || exit 97
echo "context top level: $(ls -A "$D" | tr '\n' ' ')"
[ "$(ls -A "$D" | tr '\n' ' ')" = "Agentfile antigravity-amd64 antigravity-arm64 " ] && echo "TOP LEVEL OK" || echo "TOP LEVEL DIFFERS"
cmp "$D/Agentfile" $A/Agentfile && echo "Agentfile: the embedded one"
for arch in amd64 arm64; do
  h="$D/antigravity-$arch"
  echo "== antigravity-$arch: $(ls -A "$h" | tr '\n' ' '); bin: $(ls -A "$h/bin" | tr '\n' ' ')"
  python3 -I -c 'import os,stat,sys
for r,ds,fs in os.walk(sys.argv[1]):
    for n in sorted(ds+fs):
        p=os.path.join(r,n); st=os.lstat(p); print(stat.filemode(st.st_mode), oct(stat.S_IMODE(st.st_mode))[2:], st.st_size, os.path.relpath(p,sys.argv[1]))' "$h"
  cmp "$h/harness.json" $A/harness.json && echo "harness.json: the embedded one"
  s256 "$h/bin/agy"; file "$h/bin/agy" 2>/dev/null | cut -c1-160
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
    good = h.hexdigest() == u["sha256"] and st.st_size == u["size"] and stat.S_IMODE(st.st_mode) == 0o755
    ok += good
    print(f"{'MATCH' if good else 'DIFFERS'} {b['platform']} sha256 {h.hexdigest()} (pin {u['sha256']}) size {st.st_size} (pin {u['size']}) mode {stat.S_IMODE(st.st_mode):o}")
print(f"binaries matching the pin's member SHA-256, size and mode 0755: {ok} of {len(pin['builds'])}")
sys.exit(0 if ok == len(pin["builds"]) else 1)
PY
echo "PIN CHECK EXIT $?"
own=$(uname -m); case $own in x86_64) own=amd64 ;; aarch64) own=arm64 ;; esac
if [ "$(uname -s)" != Linux ]; then echo "VERSION not run: $(uname -s) runs no Linux binary"; echo "DISK $(df -h "$HOME/.cache" | tail -1)"; exit 0; fi
home=$(mktemp -d "$W/home.XXXXXX")
out=$(HOME="$home" AGY_CLI_DISABLE_AUTO_UPDATE=true "$D/antigravity-$own/bin/agy" --version 2>&1); rc=$?
first=$(printf '%s\n' "$out" | head -1)
echo "antigravity-$own/bin/agy --version: exit $rc, first line '$first', bytes $(printf '%s\n' "$first" | od -An -tx1 | tr -s ' ')"
want=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' $A/pin.json)
if [ "$rc" = 0 ] && [ "$first" = "$want" ]; then echo "VERSION MATCH $want"; else echo "VERSION DIFFERS (pin $want)"; fi
echo "DISK $(df -h "$HOME/.cache" | tail -1); target $(du -sh "$W/target" 2>/dev/null | cut -f1)"
