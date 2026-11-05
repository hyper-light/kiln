#!/bin/bash
# Lab (goal 4 step 1): capture what Chromium, Firefox, WebKit and Safari send to open a
# WebTransport session. Runs on a Mac (Safari); everything stays on 127.0.0.1.
#   LAB=~/.cache/kiln-bp/g4/lab bash run.sh <out dir>
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
LAB="${LAB:-$HOME/.cache/kiln-bp/g4/lab}"
OUT="$1"; mkdir -p "$OUT"
"$LAB/venv/bin/python" -I "$HERE/server.py" --out "$OUT" > "$OUT/server.log" 2>&1 &
SRV=$!
for i in $(seq 50); do grep -q ready "$OUT/server.log" && break; sleep 0.2; done
cat "$OUT/server.log"
(cd "$LAB/pw" && cp "$HERE/drive.mjs" . && node drive.mjs "http://127.0.0.1:18480/" "${BROWSERS:-chromium,firefox,webkit}" "${WIDTHS:-desktop}") | tee "$OUT/playwright.jsonl"
if [ -z "${NO_SAFARI:-}" ]; then "$LAB/venv/bin/python" -I "$HERE/safari.py" "http://127.0.0.1:18480/" | tee "$OUT/safari.jsonl"; fi
sleep 1
kill $SRV
ls "$OUT"
