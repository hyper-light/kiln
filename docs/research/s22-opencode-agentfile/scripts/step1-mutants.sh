#!/bin/bash
# Step 1's guard-removal mutants (R10): each disables one guard of pin.sh in a scratch copy and runs
# pin_test.sh against it; the test that holds that guard must fail, for its reason.
set -u
SRC=$HOME/Projects/kiln/.claude/worktrees/s22-opencode-agentfile/.kiln/images/harness-opencode
KEEP=${1:?kept archives dir}
M=$HOME/.cache/kiln-pins/mutants.$$; mkdir -p "$M"
echo "MUTANTS $(date -u +%Y-%m-%dT%H:%M:%SZ) of $(git -C "$SRC" rev-parse --short HEAD) (pin.sh sha256 $(shasum -a 256 "$SRC/pin.sh" | cut -c1-16)), archives $KEEP"
mutate() { # name, the guard's line as pin.sh has it, the test that must fail
  local d=$M/$1; mkdir -p "$d"; cp "$SRC/pin.sh" "$SRC/pin_test.sh" "$d/"
  python3 -I -c 'import sys
p, old = sys.argv[1], sys.argv[2]
s = open(p).read()
if s.count(old) != 1: sys.exit("guard line found %d times" % s.count(old))
open(p, "w").write(s.replace(old, "if False:", 1))' "$d/pin.sh" "$2" || { echo "$1: MUTATION NOT APPLIED"; return; }
  "$d/pin_test.sh" "$KEEP" > "$d/test.log" 2>&1
  local rc=$? hit
  hit=$(grep -F "FAIL $3:" "$d/test.log" | head -n 1 | cut -c1-220)
  echo "$1: pin_test.sh exit $rc; $(grep -E 'passed, ' "$d/test.log"); $( [ -n "$hit" ] && echo "KILLED by: $hit" || echo "SURVIVED: no FAIL for '$3'")"
  grep -E '^FAIL' "$d/test.log" | cut -c1-220 | sed 's/^/    /'
}
mutate no-sha256sums-compare 'if lines[0][0] != sys.argv[3]:' "one byte flipped: SHA256SUMS" &
mutate no-one-entry-check 'if len(members) != 1:' "a second entry" &
mutate no-provenance-commit 'if want_commit and c.get("sourceRepositoryDigest") != want_commit:' "another commit" &
mutate no-1x-refusal 'if int(m.group(1)) < 2:' "refuses OpenCode 1.19.5" &
wait
rm -rf "$M"
