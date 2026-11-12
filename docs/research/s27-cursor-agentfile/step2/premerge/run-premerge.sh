#!/usr/bin/env bash
# Plan step 27 [kiln] (plan 01a1ce705e5c7504b88732f443d99288), steps 2 and 4 checked before what they
# wait for lands: Cursor's row (s27-cursor-agentfile) merged with step 28's s28-unpack-hook (the one
# unpacker, 4d1f96a5d, hooked into step 19's fetcher by 73fc55df9), neither on kiln main yet. At the
# merged tree: the agentfiles module's tests; then kiln's own binary makes Cursor's live context,
# `kiln agentfiles context cursor DIR --platform linux/amd64,linux/arm64`, from Cursor's own archives,
# and each platform's directory is measured against the pin: its entries (directories and regular
# files, the stripped dist-package/ being the directory itself, harness.json left out) and its bytes
# (the regular files' sizes, harness.json left out), its modes, its harness.json and Agentfile against
# the embedded ones, and the package's own `cursor-agent --version` on this x86_64 host. It works in
# the --shared clone and target run-step3.sh made, at nice 10 and -j 4, and removes the context after.
#
#   run-premerge.sh W   (W holds premerge.bundle)
set -u
W=${1:?usage: run-premerge.sh W}
exec >"$W/premerge.log" 2>&1
R=$W/kiln
export PATH=$HOME/.cargo/bin:$PATH CARGO_TARGET_DIR=$W/target CARGO_PROFILE_DEV_DEBUG=line-tables-only KILN_UI_SERVER=off
J=4
quiet() { grep --line-buffered -v -E '^\s+(Compiling|Checking|Downloaded|Downloading|Locking|Adding|Updating|Fresh|Blocking)'; }
step() {
    local name=$1
    shift
    echo "## $name: $* (from $(date -u +%T))"
    "$@" 2>&1 | quiet
    echo "${name}_EXIT=${PIPESTATUS[0]} at $(date -u +%T)"
}
scratch() { git -c user.name=s27s3 -c user.email=s27s3@invalid -c commit.gpgsign=false "$@"; }
echo "# steps 2 and 4 early, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(rustc --version), $(cargo --version)"
git -C "$R" bundle verify "$W/premerge.bundle" >/dev/null 2>&1 || { echo BUNDLE_UNVERIFIED; exit 1; }
git -C "$R" fetch -q "$W/premerge.bundle" "+refs/heads/s27-cursor-agentfile:refs/remotes/bundle/s27-cursor-agentfile" "+refs/heads/s28-unpack-hook:refs/remotes/bundle/s28-unpack-hook" || { echo FETCH_FAILED; exit 1; }
cd "$R" || exit 1
B=$(git rev-parse bundle/s27-cursor-agentfile)
H=$(git rev-parse bundle/s28-unpack-hook)
echo "# branch $B, step 28's hook $H"
git checkout -q -f --detach "$B" || exit 1
scratch merge -q --no-ff --no-edit "$H" || { echo MERGE_FAILED; git status --short; exit 1; }
echo "# merged: $(git rev-parse HEAD), tree $(git rev-parse HEAD^{tree})"

step UNIT cargo test --locked -p kiln --lib agentfiles:: -j $J
step BUILD cargo build --locked -p kiln --bin kiln -j $J
K=$CARGO_TARGET_DIR/debug/kiln
C=$W/ctx
rm -rf "$C"
step CONTEXT "$K" agentfiles context cursor "$C" --platform linux/amd64,linux/arm64
echo "# context: $(cd "$C" 2>/dev/null && ls | tr '\n' ' ')"
cmp -s "$C/Agentfile" .kiln/images/harness-cursor/Agentfile && echo "# Agentfile: the embedded one, mode $(stat -c %a "$C/Agentfile")"
python3 -I - .kiln/images/harness-cursor/pin.json <<'PY'
import json, sys
for b in json.load(open(sys.argv[1]))["builds"]:
    f = b["files"][0]
    print(f"# pin {b['platform']}: entries {f['unpack']['entries']}, unpacked_bytes {f['unpack']['unpacked_bytes']}")
PY
for a in amd64 arm64; do
    d=$C/cursor-$a
    [ -d "$d" ] || { echo "# cursor-$a: none"; continue; }
    entries=$(find "$d" \( -type d -o -type f \) ! -path "$d/harness.json" | wc -l)
    bytes=$(find "$d" -type f ! -path "$d/harness.json" -printf '%s\n' | awk '{s += $1} END {print s + 0}')
    others=$(find "$d" ! -type d ! -type f | wc -l)
    echo "# cursor-$a: entries $entries, bytes $bytes, neither file nor directory $others"
    echo "# cursor-$a modes: $(find "$d" ! -path "$d/harness.json" -printf '%y%m\n' | sort | uniq -c | tr -s ' ' | tr '\n' ',')"
    cmp -s "$d/harness.json" .kiln/images/harness-cursor/harness.json && echo "# cursor-$a/harness.json: the embedded one, mode $(stat -c %a "$d/harness.json")"
    echo "# cursor-$a/cursor-agent: $(stat -c '%F %a %s' "$d/cursor-agent" 2>&1)"
done
h=$(mktemp -d)
echo "# this host's cursor-amd64/cursor-agent --disable-auto-update --version, HOME an empty folder: $(env -i PATH=/usr/bin:/bin HOME="$h" "$C/cursor-amd64/cursor-agent" --disable-auto-update --version 2>&1 | head -3)"
echo "# its home after: $(find "$h" | wc -l) entries"
rm -rf "$h" "$C"
echo "# context removed: $([ -e "$C" ] && echo no || echo yes)"
git checkout -q -f --detach "$B"
echo "# done $(date -u +%FT%TZ)"
touch "$W/premerge.done"
