set -u
B=$HOME/.cache/s23-bundle; mkdir -p "$B"; cat > "$B/s23.bundle"
R=$HOME/.cache/s23-kiln
if [ ! -d "$R/.git" ]; then git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$R" || { echo "CLONE_FAILED"; exit 1; }; fi
git -C "$R" bundle verify "$B/s23.bundle" >/dev/null 2>&1 || { echo "BUNDLE_UNVERIFIED"; exit 1; }
git -C "$R" fetch -q "$B/s23.bundle" "+s23-goose-agentfile:refs/remotes/bundle/s23-goose-agentfile" && git -C "$R" checkout -q -f --detach refs/remotes/bundle/s23-goose-agentfile || { echo "CHECKOUT_FAILED"; exit 1; }
cd "$R"
echo "# step 2 tests, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(cargo --version), HEAD $(git rev-parse --short HEAD) ($(git log -1 --format=%s | cut -c1-60))"
export CARGO_TARGET_DIR=$HOME/.cache/s23-kiln-target
nice -n 10 cargo test -p kiln --lib agentfiles:: -j 8 2>&1 | grep --line-buffered -v -E '^\s+(Compiling|Downloaded|Downloading|Locking|Adding|Updating)'; echo "TEST_EXIT=${PIPESTATUS[0]} at $(date -u +%T)"
S=$HOME/.cache/s23-shards-0.7.0; mkdir -p "$S"
if ! (cd "$S" 2>/dev/null && grep " shards-linux-amd64$" SHA256SUMS 2>/dev/null | sha256sum -c - >/dev/null 2>&1); then curl -fsSL -o "$S/SHA256SUMS" https://github.com/hyper-light/shards/releases/download/v0.7.0/SHA256SUMS && curl -fsSL -o "$S/shards-linux-amd64" https://github.com/hyper-light/shards/releases/download/v0.7.0/shards-linux-amd64 && (cd "$S" && grep " shards-linux-amd64$" SHA256SUMS | sha256sum -c -) && chmod +x "$S/shards-linux-amd64"; fi; echo "SHARDS $("$S/shards-linux-amd64" --version 2>&1 | head -1) sha256 $(sha256sum "$S/shards-linux-amd64" | cut -d" " -f1)"
KILN_TEST_SHARDS=$S/shards-linux-amd64 nice -n 10 cargo test -p kiln --lib real_shards -j 8 -- --nocapture 2>&1 | grep --line-buffered -v -E '^\s+(Compiling|Downloaded|Downloading|Locking|Adding|Updating)'; echo "SHARDS_TEST_EXIT=${PIPESTATUS[0]} at $(date -u +%T)"
nice -n 10 cargo clippy -p kiln --all-targets -j 8 -- -D warnings 2>&1 | grep --line-buffered -v -E '^\s+(Compiling|Checking|Downloaded|Downloading)'; echo "CLIPPY_EXIT=${PIPESTATUS[0]} at $(date -u +%T)"
