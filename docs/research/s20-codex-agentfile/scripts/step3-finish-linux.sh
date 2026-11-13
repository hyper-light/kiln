# Step 20's finish on kiln main (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 20; plan
# 01a1ce705d7076648c1d9f9ee26214d3, step 3). Runs in the synced copy of p/s20-finish-v1 on a Linux runner,
# as a systemd user unit, since kiln's own runs end with its server: the agentfiles tests, clippy on the
# kiln crate, the live Codex context through the fetcher (its test, then `kiln agentfiles context codex`
# from a built kiln, its tree listed). Never runs Codex. Log and exit under $R; the target directory is
# this run's own, a reflink copy of SEED (a target built from the same Cargo.lock) where one is given.
set -u
R=${R:-$HOME/.cache/s20-finish}; mkdir -p "$R"
command -v systemd-run >/dev/null || { echo "no systemd-run"; exit 3; }
unit="s20-finish-$(date +%s)"; log="$R/$unit.log"
cat > "$R/$unit.sh" <<INNER
set -u
cd "$PWD" || exit 3
. "\$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="$R/target"
if [ ! -d "\$CARGO_TARGET_DIR" ] && [ -n "${SEED:-}" ]; then cp -a --reflink=always "${SEED:-}" "\$CARGO_TARGET_DIR" && echo "SEEDED from ${SEED:-}"; fi
echo "host \$(uname -srm) \$(hostname) \$(date -u +%FT%TZ) cpus \$(nproc) umask \$(umask)"; uptime; cargo --version; rustc --version
git log -1 --format='HEAD %H %s' | cut -c1-140; git status --short | head -20
sha256sum crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/context/unpack_tests.rs crates/kiln/src/agentfiles/cli.rs .kiln/images/harness-codex/pin.json
nice -n 10 cargo test -p kiln --lib -j 8 agentfiles:: 2>&1 | grep -E '^test |^test result|panicked|FAILED|^error|^warning' | tail -150; t=\${PIPESTATUS[0]}; echo "TEST_EXIT \$t \$(date -u +%FT%TZ)"
nice -n 10 cargo clippy -p kiln --all-targets -j 8 -- -D warnings 2>&1 | tail -60; c=\${PIPESTATUS[0]}; echo "CLIPPY_EXIT \$c \$(date -u +%FT%TZ)"
rm -rf "$R/live"; mkdir -p "$R/live"
KILN_TEST_LIVE_CONTEXT="$R/live" nice -n 10 cargo test -p kiln --lib -j 8 agentfiles::cli::tests::step20_a_live_codex_context_holds_the_pinned_builds -- --exact --nocapture 2>&1 | grep -E '^test |^test result|^TREE|^MATCH|panicked|^Error|^error' | tail -40; l=\${PIPESTATUS[0]}; echo "LIVE_TEST_EXIT \$l \$(date -u +%FT%TZ)"
rm -rf "$R/live"
nice -n 10 cargo build -p kiln --bin kiln -j 8 2>&1 | tail -5; b=\${PIPESTATUS[0]}; echo "BUILD_EXIT \$b \$(date -u +%FT%TZ)"
rm -rf "$R/ctx"
"\$CARGO_TARGET_DIR/debug/kiln" agentfiles context codex "$R/ctx" --platform linux/amd64,linux/arm64; k=\$?; echo "CLI_EXIT \$k \$(date -u +%FT%TZ)"
if [ -d "$R/ctx" ]; then
  cd "$R/ctx" || exit 3
  echo "CONTEXT \$(ls -A | tr '\n' ' ')"
  for d in codex-amd64 codex-arm64; do
    echo "DIR \$d dirs \$(find \$d -mindepth 1 -type d | wc -l) files \$(find \$d -mindepth 1 -type f ! -path \$d/harness.json | wc -l) other \$(find \$d -mindepth 1 ! -type d ! -type f | wc -l) bytes \$(find \$d -mindepth 1 -type f ! -path \$d/harness.json -printf '%s\n' | awk '{s+=\$1} END {print s}')"
  done
  sha256sum codex-amd64/codex-path/rg codex-arm64/codex-path/rg codex-amd64/bin/codex codex-arm64/bin/codex
  cmp -s codex-amd64/harness.json codex-arm64/harness.json && echo "HARNESS_JSON same in both"
  echo "LISTING"; find . -mindepth 1 -printf '%M %12s %P\n' | sort -k3
fi
INNER
systemd-run --user --unit="$unit" --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" bash -c "bash '$R/$unit.sh' > '$log' 2>&1; echo \$? > '$log.done'" 2>&1
echo "UNIT $unit LOG $log PWD $PWD"
