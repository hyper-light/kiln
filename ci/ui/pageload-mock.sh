#!/usr/bin/env bash
# ci/ui/pageload.mjs against the UI mock (MOCK_NO_AUTH=1, port 17073: never 7070), no shaped link:
# a check of the driver itself (goal 8 step 1), not a measurement of a link. Its arguments go to it.
set -uo pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || (builtin cd "$(dirname "$0")/../.." && pwd))
PORT=${PORT:-17073}
(builtin cd "$ROOT/ui" && MOCK_NO_AUTH=1 MOCK_SHIP=static PORT=$PORT HOST=127.0.0.1 exec npx tsx mock/server.ts >"${TMPDIR:-$HOME/.cache}/mock-$PORT.log" 2>&1) &
MOCK=$!
trap 'kill $MOCK 2>/dev/null' EXIT
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 1; done
node "$ROOT/ci/ui/pageload.mjs" --base "http://127.0.0.1:$PORT" --agent demo-long "$@"
