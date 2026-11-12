#!/bin/bash
# Reproduce: does gh attestation verify fail under concurrent calls? 16 at once on one real archive,
# twice; every failure's full stderr kept.
set -u
A=${1:?archive}; D=$HOME/.cache/kiln-pins/ghpar.$$; mkdir -p "$D"
echo "GHPAR $(date -u +%Y-%m-%dT%H:%M:%SZ) gh $(gh --version | head -n1 | cut -d' ' -f3) on $(basename "$A")"
for round in 1 2; do
  for i in $(seq 1 16); do
    ( s=$(date +%s); gh attestation verify "$A" --repo anomalyco/opencode --signer-workflow anomalyco/opencode/.github/workflows/release.yml --format json >"$D/$round-$i.out" 2>"$D/$round-$i.err"; echo "$? $(( $(date +%s) - s ))" >"$D/$round-$i.rc" ) &
  done
  wait
  fails=0
  for i in $(seq 1 16); do read -r rc secs <"$D/$round-$i.rc"; if [ "$rc" != 0 ]; then fails=$((fails+1)); echo "round $round call $i: exit $rc after ${secs}s: $(tr '\n' ' ' <"$D/$round-$i.err" | cut -c1-600)"; fi; done
  echo "ROUND $round: $fails of 16 failed; times $(cat "$D"/$round-*.rc | cut -d' ' -f2 | sort -n | tr '\n' ' ')"
done
rm -rf "$D"
