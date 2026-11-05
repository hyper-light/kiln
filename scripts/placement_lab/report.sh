#!/bin/bash
# Goal 7's lab's report (driver.sh's runs): each condition's placements side by side, and each placement against
# round-robin, number by number, beyond noise or not (kiln-lab's table and compare).
D=$(cd "$(dirname "$0")" && pwd)
RUNS=${RUNS:-runs} MODES=${MODES:-round-robin steer steal both} CONDS=${CONDS:-prod shared calm}
cfgs=()
for c in $CONDS; do for m in $MODES; do cfgs+=("$c-$m"); done; done
python3 "$D/merge.py" "$D/$RUNS" "${cfgs[@]}"
for c in $CONDS; do
  echo "## $c"
  args=()
  for m in $MODES; do [ -f "$D/$RUNS/$c-$m/run.json" ] && args+=("$m=$D/$RUNS/$c-$m/run.json"); done
  "$D/bin/kiln-lab" table "${args[@]}"
  for m in $MODES; do
    [ "$m" = round-robin ] && continue
    [ -f "$D/$RUNS/$c-$m/run.json" ] || continue
    echo "### $c: $m against round-robin"
    "$D/bin/kiln-lab" compare "$D/$RUNS/$c-round-robin/run.json" "$D/$RUNS/$c-$m/run.json"
  done
done
