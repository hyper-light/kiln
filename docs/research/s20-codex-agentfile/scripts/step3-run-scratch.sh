# Runs on the runner: unpack the bundle from stdin into a fresh directory under ~/.cache, test and lint it.
set -u
D=$(mktemp -d "$HOME/.cache/s20-step3.XXXXXX") || exit 3
trap 'rm -rf "$D"' EXIT
base64 -d | tar xzf - -C "$D" || { echo "bundle: bad"; exit 3; }
cd "$D" || exit 3
echo "host $(uname -srm) $(hostname)"; . "$HOME/.cargo/env" 2>/dev/null || true
cargo --version; rustc --version
for f in src/unpack.rs src/unpack/tests.rs Cargo.toml; do sha256sum "$f"; done
export CARGO_TARGET_DIR="$D/target"
nice -n 10 cargo test --quiet 2>&1 | tail -40; t=${PIPESTATUS[0]}
echo "TEST_EXIT $t"
nice -n 10 cargo clippy --quiet --all-targets -- -D warnings 2>&1 | tail -60; c=${PIPESTATUS[0]}
echo "CLIPPY_EXIT $c"
[ "$t" = 0 ] && [ "$c" = 0 ]
