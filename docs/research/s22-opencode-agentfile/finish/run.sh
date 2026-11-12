#!/usr/bin/env bash
# Step 22's finish (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d): kiln's agentfiles lib tests, then clippy on the
# kiln crate, at the bundle's head, in a kiln clone of this run's own (blobless, over anonymous HTTPS).
#   run.sh BUNDLE HEAD JOBS [MODE]   (MODE all, the default: the tests, then clippy; tests or clippy: that alone)
set -uo pipefail
export PATH=$HOME/.cargo/bin:$PATH
bundle=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
head=$2 jobs=$3 mode=${4:-all}
G=$HOME/.cache/s22-finish R=$HOME/.cache/s22-finish/kiln T=$HOME/.cache/s22-finish/target
mkdir -p "$G"
if command -v sha256sum >/dev/null; then sum() { sha256sum "$@"; }; else sum() { shasum -a 256 "$@"; }; fi
echo "START $(date -u +%FT%TZ) on $(hostname) $(uname -sm), $(getconf _NPROCESSORS_ONLN) cores, -j $jobs, mode $mode"
echo "bundle $(sum "$bundle" | cut -c1-64)"
if [ ! -d "$R/.git" ]; then
    git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/kiln.git "$R" || { echo NO_CLONE; exit 3; }
fi
# The clone is blobless: a checkout fetches its blobs, so a dropped connection is tried again.
tries() { local n; for n in 1 2 3; do "$@" && return 0; echo "retry $n: $*"; done; return 1; }
tries git -C "$R" fetch -q origin main || { echo NO_FETCH; exit 3; }
git -C "$R" fetch -q "$bundle" "+refs/heads/p/s22-finish-v1:refs/s22/head" || { echo NO_BUNDLE; exit 4; }
tries git -C "$R" checkout -q -f --detach refs/s22/head || { echo NO_CHECKOUT; exit 5; }
[ "$(git -C "$R" rev-parse HEAD)" = "$head" ] || { echo "WRONG_HEAD $(git -C "$R" rev-parse HEAD)"; exit 6; }
[ -z "$(git -C "$R" status --porcelain)" ] || { echo DIRTY; exit 7; }
cd "$R" || exit 8
echo "at $(git log --oneline -1 | cut -c1-100) | dirty 0 | $(rustc --version)"
export CARGO_TARGET_DIR=$T
# Windows builds aws-lc-sys with its prebuilt NASM objects, as ci/gate.sh sets it there.
case "$(uname -s)" in MINGW* | MSYS*) export AWS_LC_SYS_PREBUILT_NASM=1 ;; esac
if [ "$mode" = all ] || [ "$mode" = tests ]; then
    nice -n 10 cargo test --locked -p kiln --lib -j "$jobs" agentfiles:: 2>&1 | grep -E '^test |^test result|^error|^ +--> |panicked|^failures:|FAILED'
    echo "TEST_EXIT=${PIPESTATUS[0]} $(date -u +%T)"
fi
if [ "$mode" = all ] || [ "$mode" = clippy ]; then
    nice -n 10 cargo clippy --locked -p kiln --all-targets -j "$jobs" -- -D warnings 2>&1 | grep -E '^(error|warning)|^ +--> ' | head -60
    echo "CLIPPY_EXIT=${PIPESTATUS[0]} $(date -u +%T)"
fi
