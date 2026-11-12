#!/usr/bin/env bash
# kiln plan 01a1ce705e4d7008993f5c7291ef4476 step 3's light tests, on a Linux runner or a Mac Studio:
# kiln at BASE (fetched at depth 1) with this branch's diff applied, every changed file held to its
# SHA-256 as sent; then the agentfiles module's unit tests, tests/agentfiles.rs, clippy on
# crates/kiln, and three mutations of the files the rows' tests guard, each of which must fail them.
# With SHARDS_REV set, a check-only shards from that Shards revision (shards-build.sh's way) is
# built first and named by KILN_TEST_SHARDS, so step23_a_real_shards_checks_every_builtin checks
# every row with it.
# stdin: `patch <base64 of git diff --binary BASE>`, then `sha <sha256> <path>` lines.
# GNU and BSD tools alike: sha256sum or shasum, base64 -d or -D, sed -i with a suffix.
set -euo pipefail
sums() { if command -v sha256sum > /dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
unbase64() { if base64 -d < /dev/null > /dev/null 2>&1; then base64 -d; else base64 -D; fi; }
BASE=${BASE:-172176b23e2bec820a05a430f2482bfbdbb806c8}
ROOT="$HOME/.cache/s25-step3"
KILN="$ROOT/kiln"
export CARGO_TARGET_DIR="$ROOT/kiln-target"
JOBS="${JOBS:-16}"
mkdir -p "$ROOT"
echo "host $(hostname) $(uname -srm) at $(date -u +%FT%TZ), KILN_NEEDS=${KILN_NEEDS:-}"
: > "$ROOT/shas"
while read -r kind a b; do
  case "$kind" in
    patch) printf '%s' "$a" | unbase64 > "$ROOT/step3.patch" ;;
    sha) echo "$a  $b" >> "$ROOT/shas" ;;
  esac
done
if [ ! -d "$KILN/.git" ]; then
  git init -q "$KILN"
  git -C "$KILN" remote add origin https://github.com/hyper-light/kiln
fi
git -C "$KILN" fetch -q --depth 1 origin "$BASE"
git -C "$KILN" checkout -q --force --detach "$BASE"
git -C "$KILN" clean -q -fdx -e target
git -C "$KILN" apply --whitespace=nowarn "$ROOT/step3.patch"
cd "$KILN"
sums -c "$ROOT/shas"
rustc --version
cargo --version
if [ -n "${SHARDS_REV:-}" ]; then
  SRC="$ROOT/shards"
  [ -d "$SRC/.git" ] || { git init -q "$SRC" && git -C "$SRC" remote add origin https://github.com/hyper-light/shards; }
  git -C "$SRC" fetch -q --depth 1 origin "$SHARDS_REV"
  git -C "$SRC" checkout -q --force --detach "$SHARDS_REV"
  printf 'placeholder: a check-only shards boots no guest\n' > "$ROOT/placeholder-guest"
  (cd "$SRC" && SHARDS_HELPERS=skip SHARDS_INIT_BINARY="$ROOT/placeholder-guest" SHARDS_SERVER_BINARY="$ROOT/placeholder-guest" \
    CARGO_TARGET_DIR="$ROOT/shards-target" nice -n 10 cargo build -q -j "$JOBS" --locked -p shards)
  export KILN_TEST_SHARDS="$ROOT/shards-target/debug/shards"
  # Its own home and an empty Docker config: nothing of the runner's own Shards or credentials.
  mkdir -p "$ROOT/shards-home" "$ROOT/docker-config"
  export SHARDS_HOME="$ROOT/shards-home" DOCKER_CONFIG="$ROOT/docker-config"
  echo "KILN_TEST_SHARDS: $("$KILN_TEST_SHARDS" --version), Shards $(git -C "$SRC" rev-parse HEAD)"
fi

run() { # run NAME CMD…: its last lines and its exit code, which must be 0
  echo "== $1: ${*:2}"
  local start=$SECONDS
  set +e
  nice -n 10 "${@:2}" > "$ROOT/$1.log" 2>&1
  local code=$?
  set -e
  { grep -E '^test |^test result|^error|^warning|panicked|FAILED|failures:' "$ROOT/$1.log" | grep -v -E '^test .* \.\.\. ok$' | tail -40; } || true
  echo "$1: exit $code in $((SECONDS - start)) s"
  return $code
}
failed=""
run unit cargo test -j "$JOBS" -p kiln --lib agentfiles:: || failed="$failed unit"
grep -E '^test agentfiles::builtin::' "$ROOT/unit.log" || true
if [ -n "${KILN_TEST_SHARDS:-}" ]; then
  run real-shards cargo test -j "$JOBS" -p kiln --lib agentfiles::builtin::tests::step23_a_real_shards_checks_every_builtin -- --exact --nocapture || failed="$failed real-shards"
  if grep -q '^skipped' "$ROOT/real-shards.log"; then echo "real-shards: SKIPPED"; failed="$failed real-shards-skipped"; else echo "real-shards: ran with $KILN_TEST_SHARDS"; fi
fi
run integration cargo test -j "$JOBS" -p kiln --test agentfiles || failed="$failed integration"
grep -E '^test ' "$ROOT/integration.log" || true
run clippy cargo clippy -j "$JOBS" -p kiln --all-targets -- -D warnings || failed="$failed clippy"

# Mutations: each file changed alone, the builtin tests run, then the file restored (cp, so its mtime
# moves on and cargo rebuilds).
mutate() { # mutate NAME FILE SED-SCRIPT TEST: TEST (its path in kiln's lib) must fail with FILE changed
  local name=$1 file=$2 expr=$3 test=$4
  cp "$file" "$ROOT/$name.orig"
  sed -i.s25 -e "$expr" "$file"
  rm -f "$file.s25"
  if cmp -s "$file" "$ROOT/$name.orig"; then echo "$name: the mutation changed nothing"; return 1; fi
  diff "$ROOT/$name.orig" "$file" || true
  set +e
  nice -n 10 cargo test -j "$JOBS" -p kiln --lib "$test" -- --exact > "$ROOT/$name.log" 2>&1
  local code=$?
  set -e
  cp "$ROOT/$name.orig" "$file"
  touch "$file"
  { grep -E '^test |^test result|panicked at|left:|right:|assertion' "$ROOT/$name.log" | head -12; } || true
  if [ "$code" -eq 0 ]; then echo "mutation $name: SURVIVED"; return 1; fi
  if ! grep -q -F "test $test ... FAILED" "$ROOT/$name.log"; then
    tail -20 "$ROOT/$name.log"
    echo "mutation $name: exit $code, but not by $test failing"
    return 1
  fi
  echo "mutation $name: killed, $test FAILED (exit $code)"
}
mutate kind-args crates/kiln/src/harness/kinds.rs \
  's|protocol: Protocol::Acp(&\["acp"\]), screen: &\["devin"\]|protocol: Protocol::Acp(\&["acp", "--stdio"]), screen: \&["devin"]|' \
  agentfiles::builtin::devin_tests::step25_devins_harness_json_starts_it_as_kiln_does || failed="$failed kind-args"
mutate dockerfile-copy Dockerfile '\|^COPY \.kiln/images/ \.kiln/images/$|d' \
  agentfiles::builtin::tests::step23_kilns_dockerfile_copies_what_builtins_embed || failed="$failed dockerfile-copy"
mutate agentfile-secret .kiln/images/harness-devin/Agentfile 's|^VOLUME --target-kind=harness /workspace FOR devin$|ENV WINDSURF_API_KEY=x\
&|' \
  agentfiles::builtin::devin_tests::step25_devins_files_carry_no_credential || failed="$failed agentfile-secret"
sums -c "$ROOT/shas"
if [ -n "$failed" ]; then echo "FAILED:$failed"; exit 1; fi
echo "all passed, every mutation killed"
