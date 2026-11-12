#!/usr/bin/env bash
# Step 2's tests (kiln plan 01a1ce705e62732b83f16295d74ee263, step 2; plan 01a1cd8e862e7169a074db3d5fa33f3d,
# step 28) on a Linux runner, from a git bundle of BRANCH over kiln's main:
#   1. the bytes `pi --version` writes, from pi's pinned linux/amd64 tarball (checked by SHA-256 and
#      size), as a smoke run asks it: an empty environment but HOME, no network;
#   2. cargo test -p kiln --lib agentfiles:: (the module the change touches);
#   3. the real Shards' check of every built-in: v0.7.0's release binary, checked against the release's
#      SHA256SUMS (KILN_TEST_SHARDS; no Linux runner has shards on its PATH);
#   4. cargo clippy -p kiln --all-targets -- -D warnings;
#   5. cargo check --locked -p kiln in a folder holding only what the Dockerfile's build stage copies
#      (its COPY sources, as git tracks them), an empty ui/dist standing in for the ui stage's output.
#
#   step2-run.sh BUNDLE BRANCH
#
# Prints a short account, then the whole record (each step's output, cargo's progress lines and
# warning blocks left out) gzipped and base64-encoded between RECORD_B64_BEGIN and RECORD_B64_END,
# with its SHA-256. Exits non-zero unless every step passed.
set -u
bundle=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
branch=$2
R=$HOME/.cache/s28-kiln
T=$HOME/.cache/s28-kiln-target
log=$(mktemp "$HOME/.cache/s28-step2-record.XXXXXX")
say() { echo "$*" | tee -a "$log"; }
quiet() { # cargo's output without its progress lines and its warning blocks (a block's one-line summary kept)
    awk '/^[ \t]+(Compiling|Checking|Downloaded|Downloading|Locking|Adding|Updating|Blocking|Fresh|Documenting) / {next}
         /^warning: .*generated [0-9]+ warnings?/ {print; next}
         /^warning: / {skip=1; next}
         skip && /^$/ {skip=0; next}
         skip {next}
         {print}'
}

if [ ! -d "$R/.git" ]; then
    git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/kiln.git "$R" || { echo "CLONE_FAILED"; exit 1; }
fi
git -C "$R" fetch -q origin main || { echo "FETCH_FAILED"; exit 1; }
git -C "$R" bundle verify "$bundle" >/dev/null 2>&1 || { echo "BUNDLE_UNVERIFIED"; exit 1; }
git -C "$R" fetch -q "$bundle" "+refs/heads/$branch:refs/remotes/bundle/$branch" || { echo "BUNDLE_FETCH_FAILED"; exit 1; }
git -C "$R" checkout -q -f --detach "refs/remotes/bundle/$branch" || { echo "CHECKOUT_FAILED"; exit 1; }
cd "$R" || exit 1
[ -z "$(git status --porcelain)" ] || { echo "TREE_NOT_CLEAN"; git status --short | head; exit 1; }
say "# step 2 tests, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(cargo --version), HEAD $(git rev-parse --short=9 HEAD) ($(git log -1 --format=%s | cut -c1-90)), on kiln main $(git rev-parse --short=9 origin/main)"
say "# step2-run.sh sha256 $(sha256sum "$R/docs/research/s28-pi-agentfile/step2/step2-run.sh" | cut -d' ' -f1); pin.json sha256 $(sha256sum .kiln/images/harness-pi/pin.json | cut -d' ' -f1)"
fails=0

say "## 1. pi --version's bytes: linux/amd64's pinned tarball, env -i HOME=<empty dir>, no network (unshare -rn)"
P=$(mktemp -d "$HOME/.cache/s28-pi-version.XXXXXX")
read -r url sha size < <(python3 -I -c 'import json, sys
b = [b for b in json.load(open(sys.argv[1]))["builds"] if b["platform"] == "linux/amd64"][0]
f = [f for f in b["files"] if f["path"] == "."][0]
print(f["url"], f["sha256"], f["size"])' .kiln/images/harness-pi/pin.json)
if curl -fsSL -o "$P/pi.tar.gz" "$url" && [ "$(sha256sum "$P/pi.tar.gz" | cut -d' ' -f1)" = "$sha" ] && [ "$(stat -c %s "$P/pi.tar.gz")" = "$size" ]; then
    mkdir "$P/pi" "$P/home" && tar -xzf "$P/pi.tar.gz" -C "$P/pi" --strip-components=1
    unshare -rn env -i HOME="$P/home" "$P/pi/pi" --version >"$P/stdout" 2>"$P/stderr"
    rc=$?
    say "$(basename "$url"): sha256 and size match pin.json; exit $rc"
    say "stdout bytes:$(od -An -tx1 -v "$P/stdout" | tr -s ' \n' ' ')"
    say "stderr bytes: $(wc -c <"$P/stderr"); files under HOME afterwards: $(find "$P/home" -mindepth 1 | wc -l)"
    [ "$rc" = 0 ] && [ "$(od -An -tx1 -v "$P/stdout" | tr -d ' \n')" = "312e352e320a" ] && [ ! -s "$P/stderr" ] || fails=$((fails + 1))
else
    say "PI_TARBALL_MISMATCH: $url"
    fails=$((fails + 1))
fi
rm -rf "$P"
say "VERSION_BYTES_DONE fails=$fails at $(date -u +%T)"

say "## 2. cargo test -p kiln --lib agentfiles::"
CARGO_TARGET_DIR=$T nice -n 10 cargo test -p kiln --lib agentfiles:: -j 8 2>&1 | quiet >>"$log"
rc=${PIPESTATUS[0]}
say "TEST_EXIT=$rc at $(date -u +%T)"
[ "$rc" = 0 ] || fails=$((fails + 1))

say "## 3. the real Shards' check of every built-in (shards v0.7.0's release binary)"
S=$HOME/.cache/s28-shards-0.7.0
mkdir -p "$S"
if ! (cd "$S" && grep " shards-linux-amd64$" SHA256SUMS 2>/dev/null | sha256sum -c - >/dev/null 2>&1); then
    curl -fsSL -o "$S/SHA256SUMS" https://github.com/hyper-light/shards/releases/download/v0.7.0/SHA256SUMS &&
        curl -fsSL -o "$S/shards-linux-amd64" https://github.com/hyper-light/shards/releases/download/v0.7.0/shards-linux-amd64
fi
if (cd "$S" && grep " shards-linux-amd64$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1); then
    chmod +x "$S/shards-linux-amd64"
    say "SHARDS $("$S/shards-linux-amd64" --version 2>&1 | head -1), sha256 $(sha256sum "$S/shards-linux-amd64" | cut -d' ' -f1), matching the release's SHA256SUMS"
    KILN_TEST_SHARDS=$S/shards-linux-amd64 CARGO_TARGET_DIR=$T nice -n 10 cargo test -p kiln --lib real_shards -j 8 -- --nocapture 2>&1 | quiet >>"$log"
    rc=${PIPESTATUS[0]}
else
    say "SHARDS_UNVERIFIED"
    rc=1
fi
say "SHARDS_TEST_EXIT=$rc at $(date -u +%T)"
[ "$rc" = 0 ] || fails=$((fails + 1))

say "## 4. cargo clippy -p kiln --all-targets -- -D warnings"
CARGO_TARGET_DIR=$T nice -n 10 cargo clippy -p kiln --all-targets -j 8 -- -D warnings 2>&1 | quiet >>"$log"
rc=${PIPESTATUS[0]}
say "CLIPPY_EXIT=$rc at $(date -u +%T)"
[ "$rc" = 0 ] || fails=$((fails + 1))

say "## 5. cargo check --locked -p kiln from only what the Dockerfile's build stage copies"
D=$(mktemp -d "$HOME/.cache/s28-dockerctx.XXXXXX")
mapfile -t srcs < <(awk '/^FROM .* AS build$/ {s = 1; next} /^FROM / {s = 0}
    s && /^COPY / && !/--from=/ {n = 0; for (i = 2; i <= NF; i++) if ($i !~ /^--/) w[++n] = $i; for (i = 1; i < n; i++) print w[i]; delete w}' Dockerfile)
say "the build stage copies: ${srcs[*]}, and ui/dist from the ui stage"
git ls-files -z -- "${srcs[@]}" | tar --null -T - -cf - | tar -xf - -C "$D" && mkdir -p "$D/ui/dist"
say "files in the folder: $(find "$D" -type f | wc -l); none from docs/, deploy/, .github/ or another harness's folder outside .kiln/images/: $(find "$D" -mindepth 1 -maxdepth 1 -printf '%f ' | tr -s ' ')"
(cd "$D" && KILN_SKIP_UI_BUILD=1 CARGO_TARGET_DIR=$HOME/.cache/s28-dockerctx-target nice -n 10 cargo check --locked -p kiln -j 8 2>&1) | quiet >>"$log"
rc=${PIPESTATUS[0]}
say "DOCKER_CONTEXT_CHECK_EXIT=$rc at $(date -u +%T)"
[ "$rc" = 0 ] || fails=$((fails + 1))
rm -rf "$D"

say "STEPS_FAILED=$fails"
echo "## the record: $(wc -l <"$log") lines"
grep -E '^(#|test |test result|running |failures:|---- |Error|error|thread |SKIP|skipped|[A-Z_]+_EXIT=|STEPS_FAILED|SHARDS|VERSION|PI_|stdout|stderr|the build|files in)' "$log" | cut -c1-220 | head -90
echo "RECORD_SHA256 $(sha256sum "$log" | cut -d' ' -f1)"
echo "RECORD_B64_BEGIN"
gzip -9c "$log" | base64 -w0
echo
echo "RECORD_B64_END"
rm -f "$log"
[ "$fails" = 0 ]
