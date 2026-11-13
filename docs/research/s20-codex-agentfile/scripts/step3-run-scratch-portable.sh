# Runs on the runner (Linux, macOS or Windows' bash): the bundle from stdin into a fresh directory under
# ~/.cache, then the module's tests and clippy -D warnings.
set -u
mkdir -p "$HOME/.cache"
D=$(mktemp -d "$HOME/.cache/s20-step3.XXXXXX") || exit 3
trap 'rm -rf "$D"' EXIT
base64 -d | tar xzf - -C "$D" 2>/dev/null || { echo "bundle: bad"; exit 3; }
cd "$D" || exit 3
echo "host $(uname -srm) $(hostname)"; . "$HOME/.cargo/env" 2>/dev/null || true
cargo --version; rustc --version
h() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
h src/unpack.rs src/unpack/tests.rs Cargo.toml
export CARGO_TARGET_DIR="$D/target"
cargo test --quiet 2>&1 | tail -40; t=${PIPESTATUS[0]}
echo "TEST_EXIT $t"
cargo clippy --quiet --all-targets -- -D warnings 2>&1 | tail -60; c=${PIPESTATUS[0]}
echo "CLIPPY_EXIT $c"
[ "$t" = 0 ] && [ "$c" = 0 ]
