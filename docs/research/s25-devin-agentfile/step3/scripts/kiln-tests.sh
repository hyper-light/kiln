#!/usr/bin/env bash
# kiln plan 01a1ce705e4d7008993f5c7291ef4476 step 3's light tests, on a Linux runner or a Mac Studio:
# kiln at c09d23207 (fetched at depth 1) with this branch's diff applied, every changed file held to
# its SHA-256 as sent; then the agentfiles module's unit tests, tests/agentfiles.rs, clippy on
# crates/kiln, and three mutations of the files the row's tests guard, each of which must fail them.
# stdin: `patch <base64 of git diff --binary c09d23207>`, then `sha <sha256> <path>` lines.
# GNU and BSD tools alike: sha256sum or shasum, base64 -d or -D, sed -i with a suffix.
set -euo pipefail
sums() { if command -v sha256sum > /dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
unbase64() { if base64 -d < /dev/null > /dev/null 2>&1; then base64 -d; else base64 -D; fi; }
BASE=c09d232074950062b0a1b2e87ed373c4d02f9840
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
run integration cargo test -j "$JOBS" -p kiln --test agentfiles || failed="$failed integration"
grep -E '^test ' "$ROOT/integration.log" || true
run clippy cargo clippy -j "$JOBS" -p kiln --all-targets -- -D warnings || failed="$failed clippy"

# Mutations: each file changed alone, the builtin tests run, then the file restored (cp, so its mtime
# moves on and cargo rebuilds).
mutate() { # mutate NAME FILE SED-SCRIPT TEST: TEST must fail with FILE changed
  local name=$1 file=$2 expr=$3 test=$4
  cp "$file" "$ROOT/$name.orig"
  sed -i.s25 -e "$expr" "$file"
  rm -f "$file.s25"
  if cmp -s "$file" "$ROOT/$name.orig"; then echo "$name: the mutation changed nothing"; return 1; fi
  diff "$ROOT/$name.orig" "$file" || true
  set +e
  nice -n 10 cargo test -j "$JOBS" -p kiln --lib "agentfiles::builtin::tests::$test" -- --exact > "$ROOT/$name.log" 2>&1
  local code=$?
  set -e
  cp "$ROOT/$name.orig" "$file"
  touch "$file"
  { grep -E '^test |^test result|panicked at|left:|right:|assertion' "$ROOT/$name.log" | head -12; } || true
  if [ "$code" -eq 0 ]; then echo "mutation $name: SURVIVED"; return 1; fi
  if ! grep -q -F "test agentfiles::builtin::tests::$test ... FAILED" "$ROOT/$name.log"; then
    tail -20 "$ROOT/$name.log"
    echo "mutation $name: exit $code, but not by $test failing"
    return 1
  fi
  echo "mutation $name: killed, $test FAILED (exit $code)"
}
mutate kind-args crates/kiln/src/harness/kinds.rs \
  's|protocol: Protocol::Acp(&\["acp"\]), screen: &\["devin"\]|protocol: Protocol::Acp(\&["acp", "--stdio"]), screen: \&["devin"]|' \
  devins_harness_json_starts_it_as_kiln_does || failed="$failed kind-args"
mutate dockerfile-copy Dockerfile '\|^COPY \.kiln/images/ \.kiln/images/$|d' \
  the_server_image_copies_every_path_a_row_includes || failed="$failed dockerfile-copy"
mutate agentfile-secret .kiln/images/harness-devin/Agentfile 's|^VOLUME --target-kind=harness /workspace FOR devin$|ENV WINDSURF_API_KEY=x\
&|' \
  no_builtin_carries_a_credential || failed="$failed agentfile-secret"
sums -c "$ROOT/shas"
if [ -n "$failed" ]; then echo "FAILED:$failed"; exit 1; fi
echo "all passed, every mutation killed"
