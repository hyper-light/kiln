#!/bin/bash
# W21-AC01 in CI (docs/ci.md): every UI check against servers of this build, the browsers in
# Playwright's image (Chromium and WebKit, at 1440×900 and 390×844), each on the fixture it needs:
# - signin_setup.py's server: sign-in (alone on one), then People and the editor's saving, the
#   editor's completions, the runs list's window, the editor's ranges (W21-AC02), and
#   graph_setup.py's runs for the graph bench (W21-AC03);
# - insights_setup.py's: Insights, and the pages no other check drives (pages_check.mjs);
# - review_setup.py's: the review pages (W14-AC06);
# - none: the flow view's edits (workflow_doc_check.ts) and the editor's patches (patch_check.ts).
# Needs the kiln binary to check (`$1`, its UI built in), Docker, python3, git, and node with npm
# (bun if it's on PATH, else Bun's npm package). Each check's report and screenshots go to
# `$SHOTS/<check>` (default ci/ui/shots); a FAIL line is said for each failed check, and the exit
# code is how many checks failed (or 1 when a fixture couldn't be made).
#
#     ci/ui/run-all.sh target/debug/kiln
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
kiln=$(realpath "${1:?the kiln binary to check}")
shots=${SHOTS:-$here/shots}
image=mcr.microsoft.com/playwright:v1.64.1-noble
run=$here/run
pw=$run/pw
mkdir -p "$shots" "$run"
failed=0

# playwright-core as the image's browsers want it, outside the image (the checks import it).
[ -f "$pw/node_modules/playwright-core/index.mjs" ] || npm install --prefix "$pw" --no-save --no-audit --no-fund --loglevel=error playwright-core@1.64.1 > "$run/pw-install.log" 2>&1 || {
  echo "FAIL playwright-core: $(tail -3 "$run/pw-install.log")"
  exit 1
}
if command -v bun > /dev/null; then bun=(bun); else bun=(npx --yes bun@1.4.3); fi

# A fixture's server, stopped: its pid, if it's still the binary its setup copied under `$2`.
stop() {
  local pid
  pid=$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1])).get("pid", ""))' "$1" 2> /dev/null) || return 0
  [ -n "$pid" ] && case "$(readlink "/proc/$pid/exe" 2> /dev/null)" in "$2"/*) kill "$pid" ;; esac
  return 0
}

# `fixture NAME SCRIPT ENVVAR`: SCRIPT run with the binary, its JSON in $run/NAME.json.
fixture() {
  local out=$run/$1.json
  if ! KILN_UI_RUN="$run/$1" AC06_DIR="$run/$1" timeout 1800 python3 -I "$here/$2" "$kiln" > "$out" 2> "$run/$1.err"; then
    echo "FAIL $1's fixture ($2): $(tail -3 "$run/$1.err")"
    stop "$out" "$run/$1"
    exit 1
  fi
}

# `browser CHECK ENVVAR JSON [docker options]`: CHECK.mjs in Playwright's image; its report kept.
browser() {
  local name=$1 var=$2 json=$3
  shift 3
  rm -rf "${shots:?}/$name" && mkdir -p "$shots/$name"
  timeout 2400 docker run --rm --network host "$@" -e "$var=$(cat "$json")" -v "$pw:/pw:ro" -v "$here:/check:ro" -v "$shots/$name:/shots" "$image" node "/check/$name.mjs" > "$shots/$name/report.txt" 2>&1
  report "$name" $?
}

# `bunned CHECK [ENVVAR JSON]`: CHECK.ts in Bun, from the repository's root.
bunned() {
  local name=$1
  rm -rf "${shots:?}/$name" && mkdir -p "$shots/$name"
  (cd "$root" && if [ $# -gt 1 ]; then env "$2=$(cat "$3")" "${bun[@]}" "ci/ui/$name.ts"; else "${bun[@]}" "ci/ui/$name.ts"; fi) > "$shots/$name/report.txt" 2>&1
  report "$name" $?
}

report() {
  local oks fails
  oks=$(grep -c '^ok' "$shots/$1/report.txt")
  fails=$(grep -c '^FAIL' "$shots/$1/report.txt")
  if [ "$2" -eq 0 ]; then
    echo "ok   $1 ($oks passed)"
  else
    failed=$((failed + 1))
    echo "FAIL $1 (exit $2, $oks passed, $fails failed)"
    grep '^FAIL' "$shots/$1/report.txt" | cut -c1-300 | head -20 | sed 's/^/     /'
    [ "$fails" -eq 0 ] && tail -5 "$shots/$1/report.txt" | sed 's/^/     /'
  fi
}

bunned workflow_doc_check
bunned patch_check

fixture signin signin_setup.py
browser signin_check SIGNIN "$run/signin.json"
stop "$run/signin.json" "$run/signin"

fixture people signin_setup.py
browser people_check SIGNIN "$run/people.json"
browser editor_check SIGNIN "$run/people.json"
browser runs_window_check SIGNIN "$run/people.json"
bunned editor_ranges_check SIGNIN "$run/people.json"
if SIGNIN="$(cat "$run/people.json")" timeout 600 python3 -I "$here/graph_setup.py" > "$run/graph.json" 2> "$run/graph.err"; then
  browser graph_bench GRAPH "$run/graph.json"
else
  failed=$((failed + 1))
  echo "FAIL graph_bench's runs (graph_setup.py): $(tail -3 "$run/graph.err")"
fi
stop "$run/people.json" "$run/people"

fixture insights insights_setup.py
browser insights_check INSIGHTS "$run/insights.json"
browser pages_check INSIGHTS "$run/insights.json"
stop "$run/insights.json" "$run/insights"

fixture review review_setup.py
browser review_ac06 AC06 "$run/review.json" --ipc host
stop "$run/review.json" "$run/review"

echo "$failed of 13 checks failed"
exit "$failed"
