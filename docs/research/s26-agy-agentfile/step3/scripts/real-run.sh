#!/usr/bin/env bash
# Plan step 26, step 3 (kiln plan 01a1ce705e55753182217afdfe607de9): kiln's agentfiles tests, the live
# agy context and clippy on kiln, at one committed revision, on a runner. Run from a carrier folder that
# holds s26.bundle (`git bundle create s26.bundle origin/main..s26-unpack`) beside this script:
#   [LIVE=0] [CLI=1] bash real-run.sh REV [mutate]
# CLI=1 also builds kiln itself and runs `kiln agentfiles context antigravity` as the step's live test says.
# It clones kiln (the runner's own checkout, shared, read only; else GitHub's, anonymously), fetches the
# bundle, checks REV out detached, requires a clean tree, and prints each step's exit. With `mutate` it
# runs step3/mutate.py against the crate as well. Everything it makes goes under one ~/.cache directory
# it removes at the end.
set -u
rev=${1:?usage: real-run.sh REV [mutate]}
here=$(cd "$(dirname "$0")" && pwd)
echo "START host $(hostname) $(uname -srm) at $(date -u +%FT%TZ) rev $rev"
export PATH="$HOME/.cargo/bin:$PATH"
s256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
W=$(mktemp -d "$HOME/.cache/s26-real.XXXXXX") || exit 90
trap 'cd "$HOME"; rm -rf "$W"; echo "END $(date -u +%FT%TZ), removed $W"' EXIT
echo "DISK $(df -h "$HOME/.cache" | tail -1)"
if [ -d "$HOME/Projects/kiln/.git" ]; then
  git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$W/kiln" || exit 91
else
  git clone -q --no-checkout https://github.com/hyper-light/kiln.git "$W/kiln" || exit 91
fi
cd "$W/kiln" || exit 92
# A runner's checkout may lack main's newest commits, which the bundle starts from: GitHub has them.
git bundle verify -q "$here/s26.bundle" 2>/dev/null || git fetch -q https://github.com/hyper-light/kiln.git main || exit 93
git bundle verify -q "$here/s26.bundle" || exit 93
git fetch -q "$here/s26.bundle" "+refs/heads/*:refs/remotes/bundle/*" || exit 94
git -c advice.detachedHead=false checkout -q "$rev" || exit 95
echo "HEAD $(git rev-parse HEAD) dirty $(git status --porcelain | wc -l)"
[ -z "$(git status --porcelain)" ] || exit 96
s256 crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/context.rs crates/kiln/src/agentfiles/cli.rs .kiln/images/harness-antigravity/pin.json
cargo --version; rustc --version; cargo clippy --version
export CARGO_TARGET_DIR="$W/target" CARGO_BUILD_JOBS="${KILN_NEEDS_CPUS:-8}" RUST_TEST_THREADS=4 CARGO_PROFILE_DEV_DEBUG=line-tables-only KILN_UI_SERVER=off
t0=$(date +%s)
nice -n 10 cargo test --locked -p kiln --lib agentfiles:: 2>&1 | grep -v -E '^\s+(Compiling|Downloaded|Downloading) ' | tail -120
echo "TEST EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
if [ "${LIVE:-1}" = 1 ]; then
  mkdir -p "$W/live"
  t0=$(date +%s)
  KILN_TEST_LIVE_CONTEXT="$W/live" nice -n 10 cargo test --locked -p kiln --lib agentfiles::cli::tests::step26_a_live_antigravity_context_holds_the_pinned_builds -- --nocapture 2>&1 | grep -E '^(MATCH|VERSION|test |test result:|Error|thread|skipped)' | tail -20
  echo "LIVE EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s; live left $(find "$W/live" -mindepth 1 | wc -l) entries"
else
  echo "LIVE skipped (LIVE=${LIVE:-})"
fi
t0=$(date +%s)
nice -n 10 cargo clippy --locked -p kiln --all-targets -- -D warnings 2>&1 | grep -v -E '^\s+(Compiling|Checking|Downloaded|Downloading) ' | tail -60
echo "CLIPPY EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
if [ "${2:-}" = mutate ]; then
  t0=$(date +%s)
  python3 -I docs/research/s26-agy-agentfile/step3/mutate.py 2>&1 | grep -v -E '^\s+(Compiling|Downloaded|Downloading) ' | grep -E '^(==|KILLED|SURVIVED|NOT-|[a-z-]+: |test result:|test agentfiles)' | tail -60
  echo "MUTATE EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
fi
if [ "${CLI:-0}" = 1 ]; then
  # The step's live test word for word: kiln itself built, `kiln agentfiles context antigravity DIR
  # --platform linux/amd64,linux/arm64`, each bin/agy held to the pin, this machine's own run.
  A=.kiln/images/harness-antigravity
  t0=$(date +%s)
  nice -n 10 cargo build --locked -p kiln --bin kiln 2>&1 | grep -v -E '^\s+(Compiling|Downloaded|Downloading) ' | tail -5
  echo "BUILD EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
  K="$W/target/debug/kiln" D="$W/ctx"
  t0=$(date +%s)
  echo "== kiln agentfiles context antigravity $D --platform linux/amd64,linux/arm64"
  "$K" agentfiles context antigravity "$D" --platform linux/amd64,linux/arm64; rc=$?
  echo "CONTEXT EXIT $rc after $(( $(date +%s) - t0 )) s (one observation)"
  if [ "$rc" = 0 ]; then
    echo "context top level: $(ls -A "$D" | tr '\n' ' ')"
    cmp "$D/Agentfile" $A/Agentfile && echo "Agentfile: the embedded one"
    for arch in amd64 arm64; do
      h="$D/antigravity-$arch"
      echo "== antigravity-$arch: $(ls -A "$h" | tr '\n' ' '); bin: $(ls -A "$h/bin" | tr '\n' ' ')"
      cmp "$h/harness.json" $A/harness.json && echo "harness.json: the embedded one"
      s256 "$h/bin/agy"; ls -l "$h/bin/agy" | cut -c1-10; file "$h/bin/agy" 2>/dev/null | cut -c1-160
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
    if [ "$(uname -s)" = Linux ]; then
      home=$(mktemp -d "$W/home.XXXXXX")
      out=$(HOME="$home" AGY_CLI_DISABLE_AUTO_UPDATE=true "$D/antigravity-$own/bin/agy" --version 2>&1); rc=$?
      first=$(printf '%s\n' "$out" | head -1)
      echo "antigravity-$own/bin/agy --version: exit $rc, first line '$first', bytes $(printf '%s\n' "$first" | od -An -tx1 | tr -s ' ')"
      want=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' $A/pin.json)
      if [ "$rc" = 0 ] && [ "$first" = "$want" ]; then echo "VERSION MATCH $want"; else echo "VERSION DIFFERS (pin $want)"; fi
    fi
  fi
fi
echo "DISK $(df -h "$HOME/.cache" | tail -1); target $(du -sh "$W/target" 2>/dev/null | cut -f1)"
