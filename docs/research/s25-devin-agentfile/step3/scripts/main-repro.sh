#!/usr/bin/env bash
# R09, before this branch's fix counts: kiln main 172176b23's own tests/agentfiles.rs on main's tree
# exactly. Its step30_agentfiles_on_every_surface asserts the listing's `builtin` is `[]`, and
# main's builtin::ALL holds harness-goose. Run where kiln-tests.sh left its checkout and target
# (~/.cache/s25-step3), so only what main's tree changes is rebuilt.
set -euo pipefail
ROOT="$HOME/.cache/s25-step3"
KILN="$ROOT/kiln"
export CARGO_TARGET_DIR="$ROOT/kiln-target"
JOBS="${JOBS:-12}"
BASE=172176b23e2bec820a05a430f2482bfbdbb806c8
git -C "$KILN" checkout -q --force --detach "$BASE"
git -C "$KILN" clean -q -fdx
git -C "$KILN" log -1 --format='kiln %H'
echo "tree as main's: $([ -z "$(git -C "$KILN" status --porcelain)" ] && echo yes || echo no)"
grep -n 'builtin' "$KILN/crates/kiln/tests/agentfiles.rs" | head -2 || true
cd "$KILN"
set +e
nice -n 10 cargo test -j "$JOBS" -p kiln --test agentfiles > "$ROOT/main-integration.log" 2>&1
code=$?
set -e
{ grep -E '^test |^test result|^failures:' "$ROOT/main-integration.log"; grep -o -E '"builtin":\[.{0,70}' "$ROOT/main-integration.log" | head -1; } || true
echo "main's tests/agentfiles.rs: exit $code"
