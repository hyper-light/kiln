#!/usr/bin/env bash
# Plan step 26, step 3 (kiln plan 01a1ce705e55753182217afdfe607de9): writes scratch-run.sh, a script that
# tests the one unpacker alone as a scratch crate (scratch.Cargo.toml: kiln's versions of its dependencies
# and kiln's workspace lints) on a runner: its tests, clippy -D warnings and mutate.py. The module's
# files travel inside it, base64, and are checked by SHA-256 on arrival. Run from the repository root:
#   bash docs/research/s26-agy-agentfile/step3/scripts/make-scratch-run.sh OUT
# then run OUT on a runner (kiln `run`, its folder synced, or `bash -s` on stdin).
set -euo pipefail
out=${1:?usage: make-scratch-run.sh OUT}
here=$(cd "$(dirname "$0")" && pwd)
S=$(mktemp -d); trap 'rm -rf "$S"' EXIT
mkdir -p "$S/c/src/unpack"
cp crates/kiln/src/agentfiles/unpack.rs "$S/c/src/unpack.rs"
cp crates/kiln/src/agentfiles/unpack/tests.rs "$S/c/src/unpack/tests.rs"
cp "$here/scratch.Cargo.toml" "$S/c/Cargo.toml"; cp "$here/scratch.lib.rs" "$S/c/src/lib.rs"
cp "$here/../mutate.py" "$S/c/mutate.py"
(cd "$S/c" && tar czf "$S/scratch.tgz" .)
H=$(shasum -a 256 "$S/scratch.tgz" | cut -c1-64)
{
  cat <<'HEAD'
set -u
echo "START host $(hostname) $(uname -srm) at $(date -u +%FT%TZ)"
export PATH="$HOME/.cargo/bin:$PATH"
s256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
W=$(mktemp -d "$HOME/.cache/s26-scratch.XXXXXX"); cd "$W" || exit 90
trap 'cd "$HOME"; rm -rf "$W"; echo "END $(date -u +%FT%TZ), removed $W"' EXIT
base64 -d > scratch.tgz <<'B64'
HEAD
  base64 < "$S/scratch.tgz"
  echo B64
  echo "want=$H"
  cat <<'TAIL'
got=$(s256 scratch.tgz | cut -c1-64); echo "payload sha256 $got (want $want)"; [ "$got" = "$want" ] || { echo PAYLOAD MISMATCH; exit 91; }
mkdir c && tar xzf scratch.tgz -C c && cd c || exit 92
s256 src/unpack.rs src/unpack/tests.rs mutate.py
cargo --version; rustc --version; cargo clippy --version
export CARGO_TARGET_DIR="$W/target"
t0=$(date +%s); nice -n 10 cargo test 2>&1 | grep -v -E '^\s+(Compiling|Downloaded|Downloading) ' | grep -E '^(test |test result:|error|warning|thread|failures)' | tail -60; echo "TEST EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
t0=$(date +%s); nice -n 10 cargo clippy --all-targets -q -- -D warnings 2>&1 | tail -60; echo "CLIPPY EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
t0=$(date +%s); MUTATE_SCRATCH=1 python3 -I mutate.py 2>&1 | grep -E '^(==|KILLED|SURVIVED|NOT-|[a-z-]+: |test result:|test unpack)' | tail -60; echo "MUTATE EXIT ${PIPESTATUS[0]} after $(( $(date +%s) - t0 )) s"
TAIL
} > "$out"
echo "wrote $out: payload $H"
