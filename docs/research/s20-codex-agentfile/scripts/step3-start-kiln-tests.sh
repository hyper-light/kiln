# Runs in the synced copy of s20-unpack on the runner: starts the kiln crate's unpack tests and clippy as a
# systemd user unit (the kiln server drops long runs), its log and exit under ~/.cache/s20-step3.
set -u
R=$HOME/.cache/s20-step3; mkdir -p "$R"
command -v systemd-run >/dev/null || { echo "no systemd-run"; exit 3; }
unit="s20-step3-kiln-$(date +%s)"; log="$R/$unit.log"
cat > "$R/$unit.sh" <<INNER
set -u
cd "$PWD" || exit 3
. "\$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="$R/target"
echo "host \$(uname -srm) \$(hostname) \$(date -u +%FT%TZ)"; cargo --version
git log -1 --format='HEAD %H %s' | cut -c1-120; git status --short | head -20
sha256sum crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/mod.rs
nice -n 10 cargo test -p kiln --lib -j 8 agentfiles::unpack 2>&1 | tail -60; t=\${PIPESTATUS[0]}; echo "TEST_EXIT \$t \$(date -u +%FT%TZ)"
nice -n 10 cargo clippy -p kiln --all-targets -j 8 -- -D warnings 2>&1 | tail -80; c=\${PIPESTATUS[0]}; echo "CLIPPY_EXIT \$c \$(date -u +%FT%TZ)"
INNER
systemd-run --user --unit="$unit" --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" bash -c "bash '$R/$unit.sh' > '$log' 2>&1; echo \$? > '$log.done'" 2>&1
echo "UNIT $unit LOG $log PWD $PWD"
