#!/usr/bin/env bash
# kiln's serving of the UI's files, checked against a real server (goal 8, plan 01a1a832ebfd steps
# 3 and 5), byte counts by curl over loopback (the link-level numbers are the page-load lab's):
#   - every compressible file of the build (crates/kiln/src/ui_pack.rs's rule: text, scripts, JSON,
#     SVG and WebAssembly past 1 KiB), asked once just after a start, goes out compressed: the
#     binary carries its build's brotli and gzip copies (crates/kiln/build.rs), so none goes
#     uncompressed, to a browser (`br, gzip`) nor to a client that takes gzip alone (every browser
#     over plain http://; goal 11's audit, row B15);
#   - an earlier build's asset, kept, goes out as its build packed it (brotli, or gzip to a client
#     that takes gzip alone);
#   - the shell (a page) and every unhashed file carry an ETag, and a repeat with If-None-Match is
#     a 304 with no body: what a repeat visit costs for them;
#   - with an earlier build kept (build A, then a server on build B), B's changed chunk asked with
#     A's version as the dictionary goes as a delta (dcb) once kiln has made it.
# Never port 7070 (production's). Debug builds read ui/dist at run time, so one binary serves both;
# the binary is built with dist A in ui/dist (its packed copies are A's).
#
#   ci/ui/serve_check.sh <kiln binary> <dist A> <dist B>
set -uo pipefail
KILN=$(realpath "${1:?kiln binary}")
A=$(realpath "${2:?dist A}")
B=$(realpath "${3:?dist B}")
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
WORK=$(mktemp -d "${TMPDIR:-$HOME/.cache}/serve-check-XXXXXX")
PORT=${PORT:-17081}
QUIC=${QUIC:-17082}
fail=0
cat >"$WORK/kiln.toml" <<EOF
[server]
http_addr = "127.0.0.1:$PORT"
quic_addr = "127.0.0.1:$QUIC"
public_url = "http://localhost:$PORT"
quic_advertise = "127.0.0.1:$QUIC"
data_dir = "$WORK/data"
https = false
[gitops]
auto_pipelines = false
[local_agent]
enabled = false
[kubernetes]
enabled = false
[terminal]
enabled = false
[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false
[cd]
enabled = false
EOF
start() {
  rm -rf "$ROOT/ui/dist" && cp -a "$1" "$ROOT/ui/dist"
  # A home of its own and none of the caller's KILN_ variables: nothing of this machine's own kiln.
  mkdir -p "$WORK/home"
  env -i PATH="$PATH" HOME="$WORK/home" NO_COLOR=1 KILN_UI_SERVER=off "$KILN" --config "$WORK/kiln.toml" server >"$WORK/kiln-$2.log" 2>&1 &
  PID=$!
  for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && return 0; sleep 1; done
  echo "serve_check: kiln didn't start"; tail -20 "$WORK/kiln-$2.log"; return 1
}
stop() { kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null; }
head_of() { curl -s -o "$WORK/body" -D - "$@" | tr -d '\r'; }
trap 'stop; rm -rf "$ROOT/ui/dist"; cp -a "$A" "$ROOT/ui/dist"; rm -rf "$WORK"' EXIT

start "$A" a || exit 1
# Every compressible file, at once after the start: as a browser asks (`br, gzip`), then as a client
# that takes gzip alone asks, each file's first gzip-only request (nothing made it a copy before).
for asked in 'br, gzip' 'gzip'; do
  plain=0; packed=0; total=0
  while read -r f; do
    rel=${f#"$A/client/"}
    case "$rel" in _*) continue ;; esac
    h=$(head_of -H "accept-encoding: $asked" "http://127.0.0.1:$PORT/$rel")
    total=$((total + 1))
    if [[ "$f" =~ \.(js|mjs|css|html|json|webmanifest|svg|wasm|txt|xml|map|scm)$ ]] && [ "$(wc -c <"$f")" -gt 1024 ]; then
      if [ "$asked" = gzip ]; then want='^content-encoding: gzip'; else want='^content-encoding: \(br\|gzip\)'; fi
      if grep -qi "$want" <<<"$h"; then packed=$((packed + 1)); else plain=$((plain + 1)); echo "  uncompressed ($asked): $rel"; fi
    fi
  done < <(find "$A/client" -type f | sort)
  echo "compressible files sent compressed just after a start, asked '$asked': $packed, uncompressed: $plain (of $total asked)"
  [ "$plain" -eq 0 ] || fail=1
done
# The shell (a page) and the unhashed files: a 304 on a repeat.
rep=0
for path in /agents /agents/demo-long / /manifest.webmanifest /sw.js /favicon.svg /icon-192.png; do
  h=$(head_of -H 'accept: text/html' -H 'accept-encoding: br, gzip' "http://127.0.0.1:$PORT$path")
  tag=$(grep -i '^etag:' <<<"$h" | cut -d' ' -f2-)
  first=$(wc -c <"$WORK/body")
  : >"$WORK/body2"
  h2=$(curl -s -o "$WORK/body2" -D - -H 'accept: text/html' -H 'accept-encoding: br, gzip' -H "if-none-match: $tag" "http://127.0.0.1:$PORT$path" | tr -d '\r')
  code=$(head -1 <<<"$h2" | cut -d' ' -f2)
  again=$(( $(wc -c <"$WORK/body2") + $(printf '%s' "$h2" | wc -c) ))
  rep=$((rep + again))
  echo "  $path: $first bytes ($(grep -i '^content-encoding:' <<<"$h" | cut -d' ' -f2 || true)), etag $tag; again: $code, $again bytes with its head"
  [ "$code" = 304 ] || fail=1
done
echo "a repeat visit's shell and unhashed files: $rep bytes in all (heads included)"
stop

# Build B with A kept: the changed chunk as a delta against A's.
start "$B" b || exit 1
old=$(comm -23 <(ls "$A/client/assets" | sort) <(ls "$B/client/assets" | sort) | grep '\.js$' | head -1)
new=$(comm -13 <(ls "$A/client/assets" | sort) <(ls "$B/client/assets" | sort) | grep '\.js$' | head -1)
if [ -z "$old" ] || [ -z "$new" ]; then echo "serve_check: no changed chunk between the builds"; exit 1; fi
# A's version, which only the kept assets have now: as A's build packed it.
h=$(head_of -H 'accept-encoding: br, gzip' "http://127.0.0.1:$PORT/assets/$old")
kenc=$(grep -i '^content-encoding:' <<<"$h" | cut -d' ' -f2)
echo "  $old, kept from build A: $(head -1 <<<"$h" | cut -d' ' -f2) ${kenc:-uncompressed}, $(wc -c <"$WORK/body") bytes"
[ "$kenc" = br ] || fail=1
h=$(head_of -H 'accept-encoding: gzip' "http://127.0.0.1:$PORT/assets/$old")
kenc=$(grep -i '^content-encoding:' <<<"$h" | cut -d' ' -f2)
echo "  $old, kept from build A, gzip alone: $(head -1 <<<"$h" | cut -d' ' -f2) ${kenc:-uncompressed}, $(wc -c <"$WORK/body") bytes"
[ "$kenc" = gzip ] || fail=1
dict=$(node -e 'console.log(require("node:crypto").createHash("sha256").update(require("node:fs").readFileSync(process.argv[1])).digest("base64"))' "$A/client/assets/$old")
# The delta is made on the compute pool after the first ask, once the server has found the kept
# builds' dictionaries (keep_assets, in the background as it starts).
for i in $(seq 1 30); do
  h=$(head_of -H 'accept-encoding: br, gzip, dcb' -H "available-dictionary: :$dict:" "http://127.0.0.1:$PORT/assets/$new")
  enc=$(grep -i '^content-encoding:' <<<"$h" | cut -d' ' -f2)
  [ "$enc" = dcb ] && break
  sleep 2
done
size=$(wc -c <"$WORK/body")
br=$(node -e 'const z=require("node:zlib"),d=require("node:fs").readFileSync(process.argv[1]);console.log(z.brotliCompressSync(d,{params:{[z.constants.BROTLI_PARAM_QUALITY]:11}}).length)' "$B/client/assets/$new")
echo "  $new against $old: $enc, $size bytes (Brotli alone $br); $(grep -i '^use-as-dictionary:' <<<"$h")"
echo "  vary: $(grep -i '^vary:' <<<"$h" | cut -d' ' -f2-)"
[ "$enc" = dcb ] || fail=1
node -e 'const fs=require("node:fs"),d=fs.readFileSync(process.argv[1]),b=fs.readFileSync(process.argv[2]),h=require("node:crypto").createHash("sha256").update(b).digest();const ok=d.subarray(0,4).equals(Buffer.from([0xff,0x44,0x43,0x42]))&&d.subarray(4,36).equals(h);console.log("  dcb header names the dictionary:",ok);process.exit(ok?0:1)' "$WORK/body" "$A/client/assets/$old" || fail=1
stop
exit $fail
