#!/bin/bash
# Step 2's guard-removal mutants (R10), run in the synced kiln tree on a Linux runner: each changes
# what one test holds, runs the agentfiles tests, expects that test to fail, and puts the file back
# (from a copy, touched, so cargo rebuilds: a restored file can keep an older mtime).
set -u
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.cache/s22-opencode-target}
echo "MUTANTS2 $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname) at $(git log --oneline -1 | cut -c1-60)"
mutant() { # name file python-replacement(old,new) expected-failing-test
  local name=$1 file=$2 old=$3 new=$4 test=$5
  cp "$file" "$file.s22bak"
  python3 -I -c 'import sys
p, old, new = sys.argv[1:4]
s = open(p).read()
if s.count(old) != 1: sys.exit("%s: found %d times" % (old, s.count(old)))
open(p, "w").write(s.replace(old, new, 1))' "$file" "$old" "$new" || { echo "$name: NOT APPLIED"; mv -f "$file.s22bak" "$file"; touch "$file"; return; }
  out=$(nice -n 10 cargo test --locked -p kiln --lib agentfiles::builtin 2>&1)
  rc=$?
  cp -f "$file.s22bak" "$file" && rm -f "$file.s22bak" && touch "$file"
  failed=$(printf '%s\n' "$out" | grep -E '^test .* FAILED$' | sed 's/^test //; s/ \.\.\. FAILED$//' | tr '\n' ' ')
  if printf '%s\n' "$out" | grep -q -E "^test agentfiles::builtin::tests::$test \.\.\. FAILED$"; then
    echo "$name: KILLED (cargo test exit $rc) by $test; failed: $failed"
  else
    echo "$name: SURVIVED (cargo test exit $rc); failed: ${failed:-none}"
    printf '%s\n' "$out" | grep -E '^error' | head -5
  fi
}
mutant kinds-acp-args crates/kiln/src/harness/kinds.rs 'program: "opencode", verified: false, protocol: Protocol::Acp(&["acp"])' 'program: "opencode", verified: false, protocol: Protocol::Acp(&["acp", "--print-logs"])' opencodes_harness_json_starts_it_as_kiln_does
mutant dockerfile-copy-removed Dockerfile 'COPY .kiln/images/ .kiln/images/' '' the_server_image_builds_with_the_builtin_agentfiles
mutant agentfile-env-added .kiln/images/harness-opencode/Agentfile 'ARG OPENCODE_LIBC=gnu' 'ARG OPENCODE_LIBC=gnu
ENV OPENAI_API_KEY=x' no_builtin_carries_a_credential
mutant harness-home-added .kiln/images/harness-opencode/harness.json '"OPENCODE_DISABLE_AUTOUPDATE=1"' '"OPENCODE_DISABLE_AUTOUPDATE=1", "HOME=/home/opencode"' opencodes_harness_json_starts_it_as_kiln_does
nice -n 10 cargo test --locked -p kiln --lib agentfiles::builtin 2>&1 | grep -E '^test result'; echo "RESTORED_EXIT=${PIPESTATUS[0]}"
git status --short | grep -v '^??' | head
