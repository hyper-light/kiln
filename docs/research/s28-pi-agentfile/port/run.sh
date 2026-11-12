#!/usr/bin/env bash
# Step 28's port to main's design, its light tests on one machine (kiln plan
# 01a1cd8e862e7169a074db3d5fa33f3d, step 28), from a git bundle of BRANCH over kiln's main, at SHA:
#   pin    pin.sh run again for pi v1.5.2, ripgrep 15.2.0 and fd v10.5.2, its pin.json held to the
#          committed one with read_at left out;
#   unit   cargo test --locked -p kiln --lib agentfiles:: (the module the change touches);
#   live   the live context: step28_a_live_pi_context_holds_the_pinned_builds with
#          KILN_TEST_LIVE_CONTEXT, kiln's fetcher and the one unpacker on pi's real archives, then this
#          machine's own pi run with --version alone (Linux only; a Mac makes the context, runs nothing);
#   api    cargo test --locked -p kiln --test agentfiles (the built-ins on the API's list);
#   shards the real Shards' static check of every built-in, with SHARDS (a shards binary) or else
#          shards v0.7.0's Linux release, checked against the release's SHA256SUMS;
#   clippy cargo clippy --locked -p kiln --all-targets -- -D warnings.
#
#   run.sh BUNDLE BRANCH SHA [STEP...]     (default: every step above, in that order)
#
# Work goes under ~/.cache/s28-port (kiln's clone, its own CARGO_TARGET_DIR, the live context); every
# step runs at nice 10 with JOBS cargo jobs (default 8). Prints each step's output, cargo's progress
# lines left out, and a verdict line per step; exits non-zero unless every step asked passed.
set -u
here=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
bundle=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
branch=$2
sha=$3
shift 3
steps=${*:-pin unit live api shards clippy}
W=$HOME/.cache/s28-port
R=$W/kiln
export CARGO_TARGET_DIR=$W/target
jobs=${JOBS:-8}
mkdir -p "$W"
quiet() { # cargo's output without its progress lines
    grep -v -E '^\s+(Compiling|Checking|Downloaded|Downloading|Locking|Adding|Updating|Blocking|Fresh|Documenting|Building) ' || true
}
sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

if [ ! -d "$R/.git" ]; then
    git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/kiln.git "$R" || { echo "CLONE_FAILED"; exit 1; }
fi
git -C "$R" fetch -q origin main || { echo "FETCH_FAILED"; exit 1; }
git -C "$R" bundle verify "$bundle" >/dev/null 2>&1 || { echo "BUNDLE_UNVERIFIED"; exit 1; }
git -C "$R" fetch -q "$bundle" "+refs/heads/$branch:refs/remotes/bundle/$branch" || { echo "BUNDLE_FETCH_FAILED"; exit 1; }
[ "$(git -C "$R" rev-parse "refs/remotes/bundle/$branch")" = "$sha" ] || { echo "BUNDLE_NOT_AT_$sha"; exit 1; }
git -C "$R" checkout -q -f --detach "$sha" || { echo "CHECKOUT_FAILED"; exit 1; }
cd "$R" || exit 1
[ -z "$(git status --porcelain)" ] || { echo "TREE_NOT_CLEAN"; git status --short | head; exit 1; }
echo "# step 28 port, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(cargo --version), $(cargo clippy --version 2>/dev/null)"
echo "# run.sh sha256 $(sum "$here")"
echo "# HEAD $(git rev-parse HEAD) on kiln main $(git rev-parse origin/main); main contained: $(git merge-base --is-ancestor "$(git rev-parse HEAD^1)" origin/main && echo yes || echo no)"
for f in .kiln/images/harness-pi/Agentfile .kiln/images/harness-pi/harness.json .kiln/images/harness-pi/pin.json .kiln/images/harness-pi/pin.sh crates/kiln/src/agentfiles/builtin.rs crates/kiln/src/agentfiles/builtin/pi_tests.rs; do
    echo "#   $f sha256 $(sum "$f")"
done
failed=0
verdict() { # NAME EXIT
    echo "STEP $1 EXIT $2 at $(date -u +%T)"
    [ "$2" = 0 ] || failed=$((failed + 1))
}
for step in $steps; do
    echo
    case $step in
    pin)
        echo "## pin: pin.sh v1.5.2 15.2.0 v10.5.2, its output held to the committed pin.json, read_at aside"
        out=$W/pin-rerun.json
        rm -f "$out"
        PIN_OUT=$out nice -n 10 bash .kiln/images/harness-pi/pin.sh v1.5.2 15.2.0 v10.5.2 2>&1
        rc=$?
        if [ "$rc" = 0 ]; then
            python3 -I -c 'import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
print("read_at: committed", a.pop("read_at", None), "rerun", b.pop("read_at", None))
print("the rest identical:", a == b, "| key order identical:", list(a) == list(b), "| smoke:", json.dumps(b.get("smoke")))
sys.exit(0 if a == b and list(a) == list(b) else 1)' .kiln/images/harness-pi/pin.json "$out"
            rc=$?
        fi
        verdict pin "$rc"
        ;;
    unit)
        echo "## unit: cargo test --locked -p kiln --lib agentfiles::"
        nice -n 10 cargo test --locked -p kiln --lib -j "$jobs" agentfiles:: 2>&1 | quiet
        verdict unit "${PIPESTATUS[0]}"
        ;;
    live)
        echo "## live: KILN_TEST_LIVE_CONTEXT=$W/live cargo test --locked -p kiln --lib step28_a_live_pi_context_holds_the_pinned_builds -- --nocapture"
        rm -rf "$W/live" && mkdir -p "$W/live"
        KILN_TEST_LIVE_CONTEXT=$W/live nice -n 10 cargo test --locked -p kiln --lib -j "$jobs" step28_a_live_pi_context_holds_the_pinned_builds -- --nocapture 2>&1 | quiet
        rc=${PIPESTATUS[0]}
        echo "left in $W/live: $(find "$W/live" -mindepth 1 | wc -l | tr -d ' ') entries"
        rm -rf "$W/live"
        verdict live "$rc"
        ;;
    api)
        echo "## api: cargo test --locked -p kiln --test agentfiles"
        nice -n 10 cargo test --locked -p kiln --test agentfiles -j "$jobs" 2>&1 | quiet
        verdict api "${PIPESTATUS[0]}"
        ;;
    shards)
        echo "## shards: the real Shards' static check of every built-in"
        S=${SHARDS:-}
        if [ -z "$S" ]; then
            D=$W/shards-0.7.0
            mkdir -p "$D"
            if ! (cd "$D" && grep " shards-linux-amd64$" SHA256SUMS 2>/dev/null | sha256sum -c - >/dev/null 2>&1); then
                curl -fsSL -o "$D/SHA256SUMS" https://github.com/hyper-light/shards/releases/download/v0.7.0/SHA256SUMS &&
                    curl -fsSL -o "$D/shards-linux-amd64" https://github.com/hyper-light/shards/releases/download/v0.7.0/shards-linux-amd64
            fi
            if (cd "$D" && grep " shards-linux-amd64$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1); then
                chmod +x "$D/shards-linux-amd64"
                S=$D/shards-linux-amd64
                echo "shards v0.7.0's release binary, sha256 $(sum "$S"), matching the release's SHA256SUMS"
            fi
        else
            echo "shards $S, sha256 $(sum "$S")"
        fi
        if [ -n "$S" ]; then
            "$S" --version 2>&1 | head -3
            H=$(mktemp -d "$W/shards-home.XXXXXX")
            KILN_TEST_SHARDS=$S SHARDS_HOME=$H nice -n 10 cargo test --locked -p kiln --lib -j "$jobs" step23_a_real_shards_checks_every_builtin -- --nocapture 2>&1 | quiet
            rc=${PIPESTATUS[0]}
            rm -rf "$H"
        else
            echo "SHARDS_UNAVAILABLE"
            rc=1
        fi
        verdict shards "$rc"
        ;;
    clippy)
        echo "## clippy: cargo clippy --locked -p kiln --all-targets -- -D warnings"
        nice -n 10 cargo clippy --locked -p kiln --all-targets -j "$jobs" -- -D warnings 2>&1 | quiet
        verdict clippy "${PIPESTATUS[0]}"
        ;;
    *)
        echo "unknown step $step"
        verdict "$step" 2
        ;;
    esac
done
echo
echo "STEPS_FAILED=$failed"
[ "$failed" = 0 ]
