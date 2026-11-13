#!/bin/bash
cd ~/g121-tmp/s18/kiln
export CARGO_TARGET_DIR=~/g121-tmp/s18/target
echo "START $(date -u +%FT%TZ) $(git rev-parse --short HEAD) +mutants"
nice -n 10 cargo test -p kiln --lib --no-run -j 8 > ../mutbuild.log 2>&1 || { echo "BUILD FAILED"; tail -30 ../mutbuild.log; exit 1; }
while read -r mid test; do
  nice -n 10 cargo test -p kiln --lib -q -- --exact "$test" > "../mut-$mid.log" 2>&1
  rc=$?
  why=$(grep -m1 -E "panicked at|Error: " "../mut-$mid.log" | cut -c1-220)
  echo "$mid rc=$rc $test :: $why"
done < "${MUTANTS:-../mutants.txt}"
echo "END $(date -u +%FT%TZ)"
