#!/usr/bin/env bash
# The profile's allocation count, checked against a known answer (bench/README.md, "The profile"):
# the same short runs from the bench as built and from a test build that plants one allocation in
# every delivery (`--features plant`, in follow.rs's `Seen::take`). Their profiles must differ by one
# allocation and one free a delivery, all of it on the subscribers' threads: the count sees every
# thread. Quiver's run and the wake-up floor's, both in this process, their subscribers polling
# (`--busy`): a waiting subscriber's own allocations go with how often it waits, which a loaded
# machine moves between two runs, and a polling one's don't. bench/compare.sh runs this before it
# starts its brokers, so every comparison's profiles stand on it.
#
#   bench/plant.sh OUT
#
# Writes OUT/plant.md; exits non-zero unless each system's difference is one a delivery, within a
# hundredth.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
out=${1:?usage: bench/plant.sh OUT}
# An absolute path: cargo runs a bench from its package's folder.
mkdir -p "$out" && out=$(cd "$out" && pwd)
flags=(--rate 10000 --subscribers 16 --workers 4 --busy --seconds 3 --warmup 1 --runs 1 --max-load 1000)
for build in base plant; do
  features=()
  if [ "$build" = plant ]; then features=(--features plant); fi
  rm -f "${out:?}/plant-$build.tsv"
  for system in quiver wake; do
    cargo bench --locked -q -p quiver-bench "${features[@]}" --bench fanout -- \
      fanout --system "$system" "${flags[@]}" --out "$out/plant-$build.tsv" > "$out/plant-$build-$system.md"
  done
done
awk -F'\t' -v base="$out/plant-base.tsv" -v plant="$out/plant-plant.tsv" '
  FNR == 1 { for (i = 1; i <= NF; i++) col[FILENAME, $i] = i; next }
  {
    s = $col[FILENAME, "system"]; systems[s] = 1
    d = $col[FILENAME, "profile_deliveries"] + 0
    if (d == 0) { print s ": no deliveries in " FILENAME; bad = 1; next }
    a[FILENAME, s] = $col[FILENAME, "process_allocations"] / d
    f[FILENAME, s] = $col[FILENAME, "process_frees"] / d
    sub_[FILENAME, s] = $col[FILENAME, "subscribers_allocations"] / d
  }
  function near_one(x) { return x >= 0.99 && x <= 1.01 }
  END {
    print "| system | allocations a delivery, as built | planted | difference | frees, difference | the subscribers'\'' threads'\'' allocations, difference |"
    print "|---|---|---|---|---|---|"
    for (s in systems) {
      da = a[plant, s] - a[base, s]; df = f[plant, s] - f[base, s]; ds = sub_[plant, s] - sub_[base, s]
      printf "| %s | %.4f | %.4f | %+.4f | %+.4f | %+.4f |\n", s, a[base, s], a[plant, s], da, df, ds
      if (!near_one(da) || !near_one(df) || !near_one(ds)) bad = 1
    }
    print ""
    print (bad ? "FAILED: the profile does not show the planted allocation, one a delivery" : "passed: the profile shows the planted allocation, one a delivery, on the subscribers'\'' threads")
    exit bad
  }' "$out/plant-base.tsv" "$out/plant-plant.tsv" | tee "$out/plant.md"
