#!/usr/bin/env bash
# Goal 11 (plan 01a1a87b2e56 steps 1 to 4), trial only: the lab's --kiln. Runs the real kiln (G11_KILN); when it's
# the round's server, its memory is sampled first (sampler.sh, into <out>/mem/<round>).
REAL=${G11_KILN:?G11_KILN names the kiln to run}
HERE=$(cd "$(dirname "$0")" && pwd)
server=0 cfg="" prev=""
for a in "$@"; do
  [ "$prev" = "--config" ] && cfg=$a
  [ "$a" = server ] && server=1
  [ "$a" = agent ] && server=0
  prev=$a
done
if [ "$server" = 1 ] && [ -n "$cfg" ]; then
  round=$(dirname "$cfg")
  out=$(dirname "$(dirname "$round")")/mem/$(basename "$round")
  mkdir -p "$out"
  env | grep -E '^(G11_|MALLOC_|KILN_RT|LD_PRELOAD|GLIBC_TUNABLES)' > "$out/env.txt"
  bash "$HERE/sampler.sh" $$ "$out" < /dev/null > "$out/sampler.log" 2>&1 &
fi
exec "$REAL" "$@"
