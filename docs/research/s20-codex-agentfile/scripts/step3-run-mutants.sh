# Runs on the runner: the bundle from stdin into a fresh directory under ~/.cache, then each mutant.
set -u
D=$(mktemp -d "$HOME/.cache/s20-step3.XXXXXX") || exit 3
trap 'rm -rf "$D"' EXIT
base64 -d | tar xzf - -C "$D" 2>/dev/null || { echo "bundle: bad"; exit 3; }
cd "$D" || exit 3
echo "host $(uname -srm) $(hostname)"; . "$HOME/.cargo/env" 2>/dev/null || true
for f in src/unpack.rs src/unpack/tests.rs mutants.py; do sha256sum "$f"; done
export CARGO_TARGET_DIR="$D/target"
nice -n 10 cargo test --quiet >/dev/null 2>&1; echo "control: exit $?"
nice -n 10 python3 -I mutants.py
