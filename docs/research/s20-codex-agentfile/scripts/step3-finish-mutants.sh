# Step 20's finish: the tree's skip in kiln main's unpacker, before and bent (kiln plan
# 01a1cd8e862e7169a074db3d5fa33f3d, step 20; plan 01a1ce705d7076648c1d9f9ee26214d3, step 3). In the synced
# copy of p/s20-finish-v1, with this branch's fetcher tests:
# - M0, before: kiln main's own unpack.rs and unpack/tests.rs (3a4b09005), which take no skip;
# - M9 to M11: one guard of the skip bent in unpack.rs, applied alone and exactly once.
# The agentfiles tests run against each, and must fail the tests written for it. The files are restored
# and touched after each (cargo keeps a build whose source's mtime didn't move), their hashes checked at the
# end, and the tests run once more as the control.
set -u
R=${R:-$HOME/.cache/s20-finish}; mkdir -p "$R"
. "$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="$R/target"
J=${J:-12}
MAIN=3a4b090054c59dc50eee8e196d78a52cbf749234
F=crates/kiln/src/agentfiles/unpack.rs
T=crates/kiln/src/agentfiles/unpack/tests.rs
sum() { shasum -a 256 "$@" 2>/dev/null || sha256sum "$@"; }
cp "$F" "$R/unpack.rs.orig"; cp "$T" "$R/tests.rs.orig"
before=$(sum "$F" "$T")
mutate() {
  python3 -I -c 'import sys,pathlib
p=pathlib.Path(sys.argv[1]); s=p.read_text()
n=s.count(sys.argv[2])
if n!=1: sys.exit("%d matches of %r" % (n, sys.argv[2]))
p.write_text(s.replace(sys.argv[2], sys.argv[3]))' "$F" "$1" "$2"
}
run() {
  nice -n 10 cargo test -p kiln --lib -j "$J" agentfiles:: 2>&1 | grep -E '^test .*FAILED|^test result|^error|unknown field' | tail -30
  echo "EXIT ${PIPESTATUS[0]} (want non-zero) $(date -u +%FT%TZ)"
}
restore() { cp "$R/unpack.rs.orig" "$F" && cp "$R/tests.rs.orig" "$T" && touch "$F" "$T"; }
echo "M0 before: main's unpack.rs and unpack/tests.rs ($MAIN), no skip"
# From git, or where a copy without the history has them beside it (MAIN_FILES: unpack.rs, tests.rs).
if [ -n "${MAIN_FILES:-}" ]; then cp "$MAIN_FILES/unpack.rs" "$F" && cp "$MAIN_FILES/tests.rs" "$T"; else git show "$MAIN:$F" > "$F" && git show "$MAIN:$T" > "$T"; fi && sum "$F" "$T" && run; restore
echo "M9 a skipped file written: the tree's skip is noted, then the file is written anyway"
mutate 'if skip.take(&rel) {' 'if skip.take(&rel) && false {' && run; restore
echo "M10 a skip path the archive never held passes: no Missing at the end"
mutate 'if let Some(shown) = skip.missing() {' 'if let Some(shown) = skip.missing().filter(|_| false) {' && run; restore
echo "M11 a directory at a skip path taken: no Kind refusal"
mutate 'if skip.lists(&rel) {' 'if skip.lists(&rel) && false {' && run; restore
after=$(sum "$F" "$T")
echo "$after"; [ "$before" = "$after" ] && echo "RESTORED_SAME yes"
nice -n 10 cargo test -p kiln --lib -j "$J" agentfiles:: 2>&1 | grep -E '^test result' | tail -2; echo "CONTROL_EXIT ${PIPESTATUS[0]} $(date -u +%FT%TZ)"
