#!/usr/bin/env bash
# Two builds, the second with a one-line change in the conversation's own view (goal 8, plan
# 01a1a832ebfd step 4): at most 2 hashed asset files may take new names (the shell, which carries
# the import map, isn't one), and each page's bytes a visit after that deploy downloads again are
# reported (ci/ui/page_bytes.mjs --since) and held to their budgets. ui/dist is the first build
# (built here if it isn't there) and is put back as it was.
#
#   ci/ui/two_builds.sh            MAX_RENAMED (2)
set -euo pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
UI=$ROOT/ui
VIEW=$UI/src/views/harness/agent.tsx
MAX=${MAX_RENAMED:-2}
WORK=$(mktemp -d "${TMPDIR:-$HOME/.cache}/two-builds-XXXXXX")
cp "$VIEW" "$WORK/view"
restore() {
  cp "$WORK/view" "$VIEW"
  if [ -d "$WORK/a" ]; then rm -rf "$UI/dist" && mv "$WORK/a" "$UI/dist"; fi
  rm -rf "$WORK"
}
trap restore EXIT
build() { (builtin cd "$UI" && KILN_UI_SERVER=off npm run build >"$WORK/build.log" 2>&1) || { tail -40 "$WORK/build.log"; exit 1; }; }
names() { (builtin cd "$1/client/assets" && ls | grep -v -E '\.(br|gz)$' | sort); }

[ -f "$UI/dist/client/_shell.html" ] || build
cp -a "$UI/dist" "$WORK/a"
grep -q 'aria-label="Path"' "$VIEW" || { echo "two_builds: the line it changes isn't in $VIEW any more" >&2; exit 1; }
sed -i.bak 's/aria-label="Path"/aria-label="Path here"/' "$VIEW" && rm -f "$VIEW.bak"
build
names "$WORK/a" >"$WORK/a.txt"
names "$UI/dist" >"$WORK/b.txt"
renamed=$(comm -13 "$WORK/a.txt" "$WORK/b.txt")
count=$(printf '%s\n' "$renamed" | grep -c . || true)
echo "two_builds: a one-line change in the conversation's view renamed $count hashed files (at most $MAX):"
printf '  %s\n' $renamed
node "$ROOT/ci/ui/page_bytes.mjs" --dist "$UI/dist" --since "$WORK/a" --check >"$WORK/bytes.txt" || { cat "$WORK/bytes.txt"; echo "two_builds: a page's bytes after the deploy are over budget" >&2; exit 1; }
grep 'after a deploy' "$WORK/bytes.txt"
[ "$count" -le "$MAX" ] || { echo "two_builds: $count files renamed, more than $MAX" >&2; exit 1; }
