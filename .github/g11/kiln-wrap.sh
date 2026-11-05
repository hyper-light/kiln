#!/usr/bin/env bash
# Goal 11 (plan 01a1a87b2e56 steps 1 to 4), trial only: the lab's --kiln. Runs the real kiln (G11_KILN); when it's
# the round's server, its memory is sampled first (sampler.sh, into <out>/mem/<round>). G11_JEMALLOC=1: it
# allocates through jemalloc (G11_JEMALLOC_LIB) with heap profiling on, dumped every 2^G11_JE_INTERVAL bytes
# allocated (and at its end).
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
  if [ "${G11_JEMALLOC:-}" = 1 ]; then
    export LD_PRELOAD=${G11_JEMALLOC_LIB:?} MALLOC_CONF="prof:true,prof_final:true,lg_prof_interval:${G11_JE_INTERVAL:-30},lg_prof_sample:${G11_JE_SAMPLE:-19},prof_prefix:$out/jeprof"
  fi
fi
exec "$REAL" "$@"
