#!/bin/bash
cd ~/g121-tmp/s18/kiln
export CARGO_TARGET_DIR=~/g121-tmp/s18/target
echo "START $(date -u +%FT%TZ) $(git rev-parse --short HEAD)"
nice -n 10 cargo check -p kiln --lib --tests -j 8 --message-format short > ../check.log 2>&1
rc=$?
echo "CHECK rc=$rc $(date -u +%FT%TZ)"
if [ $rc -ne 0 ]; then exit $rc; fi
nice -n 10 cargo test -p kiln --lib --no-run -j 8 --message-format short > ../build.log 2>&1
rc=$?
echo "BUILD rc=$rc $(date -u +%FT%TZ)"
exit $rc
