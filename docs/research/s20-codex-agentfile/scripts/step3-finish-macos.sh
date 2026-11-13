# Step 20's finish on kiln main, on macOS arm64 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 20;
# plan 01a1ce705d7076648c1d9f9ee26214d3, step 3). Runs in the synced copy of p/s20-finish-v1, through
# `kiln run` in the background: the agentfiles tests and clippy on the kiln crate, with this run's own
# target directory under $R. Exits 0 only when both did.
set -u
R=${R:-$HOME/.cache/s20-finish}; mkdir -p "$R"
. "$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="$R/target"
J=${J:-12}
echo "host $(uname -srm) $(hostname) $(date -u +%FT%TZ) cpus $(sysctl -n hw.ncpu) jobs $J umask $(umask)"; uptime; cargo --version; rustc --version
git log -1 --format='HEAD %H %s' | cut -c1-140; git status --short | head -20
shasum -a 256 crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/context/unpack_tests.rs crates/kiln/src/agentfiles/cli.rs .kiln/images/harness-codex/pin.json
nice -n 10 cargo test -p kiln --lib -j "$J" agentfiles:: 2>&1 | grep -E '^test |^test result|panicked|FAILED|^error|^warning' | tail -150; t=${PIPESTATUS[0]}; echo "TEST_EXIT $t $(date -u +%FT%TZ)"
nice -n 10 cargo clippy -p kiln --all-targets -j "$J" -- -D warnings 2>&1 | tail -60; c=${PIPESTATUS[0]}; echo "CLIPPY_EXIT $c $(date -u +%FT%TZ)"
[ "$t" = 0 ] && [ "$c" = 0 ]
