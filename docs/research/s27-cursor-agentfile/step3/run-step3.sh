#!/usr/bin/env bash
# Plan step 27 [kiln], step 3 (plan 01a1ce705e5c7504b88732f443d99288): Cursor's row in kiln's
# built-ins, tested on a Linux runner before it is committed. It works in a --shared clone of the
# runner's own kiln checkout (read-only use of its objects), at the commits of a git bundle:
#   GREEN: s27-cursor-agentfile merged with kiln main, then with s20-surface-fix (step 20's fix of the
#          agentfiles surface test, byte for byte step 25's), and this step's three files: the tree to
#          be committed;
#   RED:   the same without s20-surface-fix: the surface test as kiln main has it.
# The tests: the agentfiles module's, the real-shards checks (shards v0.7.0's release binary, held to
# its SHA256SUMS), the agentfiles integration test (green at GREEN, red at RED), clippy on crates/kiln;
# then six mutations of the embedded files and of kiln's Cursor kind, each of which must fail step
# 3's tests. Run as a transient systemd user unit (it outlives the kiln agent's restarts), at nice 10
# with a target directory of its own (kiln's AGENTS.md for heavy runs), at -j 4, uncapped: capped at
# 17G (the 16.7 GiB kiln's libraries' tests peaked at with -j 4, .kiln/ci.yaml), the kernel killed
# its rustc of kiln's tests at 17.7 GB resident and 3.3 GB swapped (2026-11-12 16:17Z, this runner,
# rustc 1.99.1, kiln main ce63fc099). Its own peak is read at the end, from its cgroup.
#
#   run-step3.sh W   (W holds s27s3.bundle and files/{builtin.rs,cursor_tests.rs,Agentfile})
set -u
W=${1:?usage: run-step3.sh W}
exec >"$W/run.log" 2>&1
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
at() { git checkout -q -f --detach "$1" && echo "# HEAD $(git rev-parse --short HEAD), tree $(git rev-parse HEAD^{tree}): $(git log -1 --format=%s | cut -c1-90)"; }
echo "# step 3's tests, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(rustc --version), $(cargo --version)"
echo "# unit cgroup $(cut -d: -f3 /proc/self/cgroup)"

[ -d "$R/.git" ] || git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$R" || { echo CLONE_FAILED; exit 1; }
git -C "$R" bundle verify "$W/s27s3.bundle" >/dev/null 2>&1 || { echo BUNDLE_UNVERIFIED; exit 1; }
git -C "$R" fetch -q "$W/s27s3.bundle" "+refs/heads/s27-cursor-agentfile:refs/remotes/bundle/s27-cursor-agentfile" "+refs/heads/s20-surface-fix:refs/remotes/bundle/s20-surface-fix" "+refs/remotes/origin/main:refs/remotes/bundle/main" || { echo FETCH_FAILED; exit 1; }
cd "$R" || exit 1
B=$(git rev-parse bundle/s27-cursor-agentfile)
F=$(git rev-parse bundle/s20-surface-fix)
MAIN=$(git rev-parse bundle/main)
echo "# branch $B, kiln main $MAIN, surface fix $F"
paths="crates/kiln/src/agentfiles/builtin.rs crates/kiln/src/agentfiles/builtin/cursor_tests.rs .kiln/images/harness-cursor/Agentfile"
place() {
    cp "$W/files/builtin.rs" crates/kiln/src/agentfiles/builtin.rs
    mkdir -p crates/kiln/src/agentfiles/builtin
    cp "$W/files/cursor_tests.rs" crates/kiln/src/agentfiles/builtin/cursor_tests.rs
    cp "$W/files/Agentfile" .kiln/images/harness-cursor/Agentfile
    # shellcheck disable=SC2086
    git add -- $paths && scratch commit -q -m "$1"
}
at "$B" || exit 1
scratch merge -q --no-ff --no-edit "$MAIN" || { echo MERGE_MAIN_FAILED; exit 1; }
M1=$(git rev-parse HEAD)
echo "# merged with main: tree $(git rev-parse HEAD^{tree})"
place "s27s3 scratch: step 3's files on the branch merged with main (RED)" || exit 1
RED=$(git rev-parse HEAD)
at "$M1" || exit 1
scratch merge -q --no-ff --no-edit "$F" || { echo MERGE_FIX_FAILED; exit 1; }
echo "# merged with the surface fix: tree $(git rev-parse HEAD^{tree})"
place "s27s3 scratch: step 3's files on the branch merged with s20-surface-fix (GREEN)" || exit 1
GREEN=$(git rev-parse HEAD)
echo "# RED $RED tree $(git rev-parse "$RED^{tree}")"
echo "# GREEN $GREEN tree $(git rev-parse "$GREEN^{tree}")"
# shellcheck disable=SC2086
sha256sum $paths crates/kiln/tests/agentfiles.rs

if [ ! -d "$CARGO_TARGET_DIR" ]; then
    if cp -a --reflink=always "$HOME/.cache/s20s2-kiln-target" "$CARGO_TARGET_DIR" 2>/dev/null; then
        echo "# TARGET seeded from s20s2-kiln-target by reflink"
    else
        rm -rf "$CARGO_TARGET_DIR"
        echo "# TARGET fresh"
    fi
fi

at "$GREEN" || exit 1
step UNIT cargo test --locked -p kiln --lib agentfiles:: -j $J
S=$HOME/.cache/s23-shards-0.7.0
if (cd "$S" && grep " shards-linux-amd64$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1); then
    echo "# SHARDS $("$S/shards-linux-amd64" --version 2>&1 | head -1), sha256 $(sha256sum "$S/shards-linux-amd64" | cut -d' ' -f1), checked against v0.7.0's SHA256SUMS"
    export KILN_TEST_SHARDS=$S/shards-linux-amd64
    step SHARDS cargo test --locked -p kiln --lib real_shards -j $J -- --nocapture
    unset KILN_TEST_SHARDS
else
    echo "SHARDS_UNCHECKED: $S holds no shards matching its SHA256SUMS"
fi
step INTEG cargo test --locked -p kiln --test agentfiles -j $J
step CLIPPY cargo clippy --locked -p kiln --all-targets -j $J -- -D warnings

at "$RED" || exit 1
step INTEG_RED cargo test --locked -p kiln --test agentfiles step30_agentfiles_on_every_surface -j $J

mutant() {
    local name=$1 file=$2
    echo "# $name diff: $(git diff --numstat -- "$file")"
    git diff -U0 -- "$file" | grep -E '^[-+][^-+]' | cut -c1-160
    step "$name" cargo test --locked -p kiln --lib step27_ -j $J
}
# M1: kiln's Cursor kind launches with other ACP arguments, nothing else changed.
at "$GREEN" || exit 1
sed -i '/id: "cursor"/s|Protocol::Acp(&\["acp"\])|Protocol::Acp(\&["acp", "--mutant"])|' crates/kiln/src/harness/kinds.rs
mutant M1 crates/kiln/src/harness/kinds.rs
# M2: kiln's launch of Cursor sets another NO_OPEN_BROWSER, nothing else changed.
at "$GREEN" || exit 1
sed -i 's|p.env("NO_OPEN_BROWSER", "1");|p.env("NO_OPEN_BROWSER", "0");|' crates/kiln/src/harness/kinds.rs
mutant M2 crates/kiln/src/harness/kinds.rs
# M3: the plan's credential, ENV CURSOR_API_KEY=x, in the embedded Agentfile.
at "$GREEN" || exit 1
printf 'ENV CURSOR_API_KEY=x\n' >>.kiln/images/harness-cursor/Agentfile
mutant M3 .kiln/images/harness-cursor/Agentfile
# M4-M6: the embedded pin with a 63-character digest, without its arm64 build, and unstripped.
edit_pin() { python3 -I -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); exec(sys.argv[2]); open(p,"w").write(json.dumps(d, indent=2)+"\n")' .kiln/images/harness-cursor/pin.json "$1"; }
at "$GREEN" || exit 1
edit_pin 'f=d["builds"][0]["files"][0]; f["sha256"]=f["sha256"][1:]'
mutant M4 .kiln/images/harness-cursor/pin.json
at "$GREEN" || exit 1
edit_pin 'd["builds"]=[b for b in d["builds"] if b["platform"]!="linux/arm64"]'
mutant M5 .kiln/images/harness-cursor/pin.json
at "$GREEN" || exit 1
edit_pin 'd["builds"][0]["files"][0]["unpack"]["strip"]=0'
mutant M6 .kiln/images/harness-cursor/pin.json

at "$GREEN" || exit 1
echo "# tree clean at GREEN: $(git status --porcelain | wc -l) changes"
cg=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
echo "# memory.peak $(cat "$cg/memory.peak" 2>/dev/null) bytes; swap.peak $(cat "$cg/memory.swap.peak" 2>/dev/null) bytes"
echo "# target $(du -sh "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1)"
echo "# done $(date -u +%FT%TZ)"
touch "$W/done"
