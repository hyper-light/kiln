#!/usr/bin/env bash
# Step 27's merge with kiln main (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27), tested at its tree before
# it is committed and pushed, as the person's rule asks (ROOT's goal for the step, 2026-11-12): the tests of the
# code it touches on Linux, and kiln's clippy gate on Linux, macOS and Windows where a runner has room now.
# From a git bundle: s27-cursor-agentfile merged with kiln main, scratch commits only, the tree held to WANT.
#
#   run-merge.sh W MODE WANT   (W holds merge.bundle, and resolved/ where the merge conflicts; MODE linux:
#                               the agentfiles tests, the agentfiles integration test and the clippy gate;
#                               MODE clippy: the clippy gate alone)
#
# Its clone is a --shared one of the runner's own kiln checkout where there is one, else kiln's public
# repository's; its target directory is its own, under W. On Linux the gate is a systemd user unit of its own
# (ci/gate.sh); on macOS and Windows it runs uncapped at its lowest priority, as ci/gate.sh says.
set -u
W=${1:?usage: run-merge.sh W MODE WANT}
MODE=${2:?linux or clippy}
WANT=${3:?the tree to test}
exec >"$W/merge-$MODE.log" 2>&1
R=$W/kiln
export PATH=$HOME/.cargo/bin:$PATH CARGO_TARGET_DIR=$W/target CARGO_PROFILE_DEV_DEBUG=line-tables-only KILN_UI_SERVER=off
quiet() { grep --line-buffered -v -E '^\s+(Compiling|Checking|Downloaded|Downloading|Locking|Adding|Updating|Fresh|Blocking)'; }
step() {
    local name=$1
    shift
    echo "## $name: $* (from $(date -u +%T))"
    "$@" 2>&1 | quiet
    echo "${name}_EXIT=${PIPESTATUS[0]} at $(date -u +%T)"
}
scratch() { git -c user.name=s27s3 -c user.email=s27s3@invalid -c commit.gpgsign=false "$@"; }
echo "# step 27's merge, mode $MODE, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(rustc --version 2>&1), $(cargo --version 2>&1)"
if [ ! -d "$R/.git" ]; then
    if [ -d "$HOME/Projects/kiln/.git" ]; then
        git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$R" || { echo CLONE_FAILED; exit 1; }
    else
        git clone -q --no-checkout https://github.com/hyper-light/kiln.git "$R" || { echo CLONE_FAILED; exit 1; }
    fi
fi
git -C "$R" bundle verify "$W/merge.bundle" >/dev/null 2>&1 || { echo BUNDLE_UNVERIFIED; git -C "$R" bundle verify "$W/merge.bundle" 2>&1 | tail -5; exit 1; }
# The bundle's refs land under refs/bundle/: the branch, and kiln main as tested (refs/s27/test-main, a ref of
# the bundle's maker pinning it, or the maker's refs/remotes/origin/main).
git -C "$R" fetch -q "$W/merge.bundle" "+refs/*:refs/bundle/*" || { echo FETCH_FAILED; exit 1; }
cd "$R" || exit 1
B=$(git rev-parse refs/bundle/heads/s27-cursor-agentfile)
MAIN=$(git rev-parse -q --verify refs/bundle/s27/test-main || git rev-parse refs/bundle/remotes/origin/main)
echo "# branch $B, kiln main $MAIN"
git checkout -q -f --detach "$B" || exit 1
# A merge that conflicts takes its resolution from W/resolved: MANIFEST names each file there and its path.
if ! scratch merge -q --no-ff --no-edit "$MAIN"; then
    echo "# merge conflicts in: $(git diff --name-only --diff-filter=U | tr '\n' ' ')"
    [ -f "$W/resolved/MANIFEST" ] || { echo MERGE_FAILED; git status --short | head; exit 1; }
    while read -r name p; do
        [ -n "$name" ] || continue
        cp "$W/resolved/$name" "$p" && git add -- "$p" || { echo "PLACE_FAILED $p"; exit 1; }
    done <"$W/resolved/MANIFEST"
    scratch commit -q --no-edit || { echo COMMIT_FAILED; git status --short | head; exit 1; }
fi
TREE=$(git rev-parse HEAD^{tree})
echo "# merged: $(git rev-parse HEAD), tree $TREE, wanted $WANT"
[ "$TREE" = "$WANT" ] || { echo TREE_MISMATCH; exit 1; }

if [ "$MODE" = linux ]; then
    step UNIT cargo test --locked -p kiln --lib agentfiles:: -j 4
    step INTEG cargo test --locked -p kiln --test agentfiles -j 4
fi
step CLIPPY_GATE ci/gate.sh -p kiln --cpus "${CPUS:-4}" --mem "${MEM:-16g}" --target-dir "$CARGO_TARGET_DIR" clippy
echo "# tree clean: $(git status --porcelain | wc -l) changes"
echo "# done $(date -u +%FT%TZ)"
touch "$W/merge-$MODE.done"
