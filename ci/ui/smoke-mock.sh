#!/usr/bin/env bash
# Loads goal 8's pages from the UI mock (MOCK_NO_AUTH=1, port 17073: never 7070) in a browser and
# fails on a page error or a failed asset (goal 8: a build's import map and packed files
# work in the engines). ENGINES (chromium webkit), FORMS (desktop phone).
set -uo pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
PORT=${PORT:-17073}
(builtin cd "$ROOT/ui" && MOCK_NO_AUTH=1 MOCK_SHIP=static PORT=$PORT HOST=127.0.0.1 exec npx tsx mock/server.ts >"${TMPDIR:-/tmp}/mock-$PORT.log" 2>&1) &
MOCK=$!
trap 'kill $MOCK 2>/dev/null' EXIT
for i in $(seq 1 60); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 1; done
node "$ROOT/ci/ui/${SMOKE:-smoke.mjs}" "http://127.0.0.1:$PORT" ${ENGINES:-chromium webkit}
