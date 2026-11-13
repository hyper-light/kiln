# Runs in the synced copy of s20-unpack on the runner: the agentfiles tests, clippy on kiln, and one
# mutation for the pinned-unpack test, as a systemd user unit; log and exit under ~/.cache/s20-step3.
set -u
R=$HOME/.cache/s20-step3; mkdir -p "$R"
command -v systemd-run >/dev/null || { echo "no systemd-run"; exit 3; }
unit="s20-step3-final-$(date +%s)"; log="$R/$unit.log"
cat > "$R/$unit.sh" <<INNER
set -u
cd "$PWD" || exit 3
. "\$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="$R/target"
echo "host \$(uname -srm) \$(hostname) \$(date -u +%FT%TZ)"; cargo --version
git log -1 --format='HEAD %H %s' | cut -c1-140; git status --short | head -20
sha256sum crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/mod.rs .kiln/images/harness-codex/pin.json
nice -n 10 cargo test -p kiln --lib -j 8 agentfiles:: 2>&1 | grep -E '^test |^test result|panicked|FAILED|^error' | tail -120; t=\${PIPESTATUS[0]}; echo "TEST_EXIT \$t \$(date -u +%FT%TZ)"
nice -n 10 cargo clippy -p kiln --all-targets -j 8 -- -D warnings 2>&1 | tail -40; c=\${PIPESTATUS[0]}; echo "CLIPPY_EXIT \$c \$(date -u +%FT%TZ)"
P=.kiln/images/harness-codex/pin.json; cp \$P "$R/pin.orig"
jq 'del(.builds[0].files[0].unpack.entries)' "$R/pin.orig" > \$P
nice -n 10 cargo test -p kiln --lib -j 8 agentfiles::unpack::tests::every_builtins_pinned_unpack_is_a_spec 2>&1 | grep -E '^test |^test result|Lacks|entries' | tail -8; m=\${PIPESTATUS[0]}
cp "$R/pin.orig" \$P && touch \$P
echo "MUTANT_M8 amd64-tree-without-entries exit \$m (want non-zero) \$(date -u +%FT%TZ)"
nice -n 10 cargo test -p kiln --lib -j 8 agentfiles::unpack::tests::every_builtins_pinned_unpack_is_a_spec 2>&1 | grep -E '^test result' | tail -2; r=\${PIPESTATUS[0]}
echo "RESTORED exit \$r; pin.json \$(sha256sum \$P | cut -c1-16)"
INNER
systemd-run --user --unit="$unit" --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" bash -c "bash '$R/$unit.sh' > '$log' 2>&1; echo \$? > '$log.done'" 2>&1
echo "UNIT $unit LOG $log PWD $PWD"
