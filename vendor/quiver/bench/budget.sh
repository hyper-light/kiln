#!/usr/bin/env bash
# The ephemeral class's bench gate (bench/README.md, "The budgets"): each load bench/budgets.tsv
# names, run once, its window 60 s after 5 s of warm-up, held to its budgets (`--budgets`). Exits
# non-zero if a run went over one. A quiet machine's measure (CLAUDE.md §7): a run refuses to start
# above a load average of 2, LOAD_LIMIT raising it for a smoke test of the gate itself.
#
#   bench/budget.sh OUT
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
out=${1:?usage: bench/budget.sh OUT}
# Absolute paths: cargo runs a bench from its package's folder.
mkdir -p "$out" && out=$(cd "$out" && pwd)
budgets=$PWD/bench/budgets.tsv
limit=${LOAD_LIMIT:-2}
QUIVER_COMMIT=$(git rev-parse HEAD)
export QUIVER_COMMIT
cargo bench --locked -q -p quiver-bench --bench fanout --no-run
status=0
while IFS=$'\t' read -r system waits rate subscribers; do
  flags=(--system "$system" --rate "$rate" --subscribers "$subscribers" --seconds 60 --warmup 5
    --runs 1 --max-load "$limit" --budgets "$budgets" --out "$out/runs.tsv")
  if [ "$waits" = busy ]; then flags+=(--busy); fi
  cargo bench --locked -q -p quiver-bench --bench fanout -- fanout "${flags[@]}" \
    | tee -a "$out/budget.md" || status=1
done < <(grep -v -e '^#' -e '^$' "$budgets" | cut -f1-4 | sort -u)
exit "$status"
