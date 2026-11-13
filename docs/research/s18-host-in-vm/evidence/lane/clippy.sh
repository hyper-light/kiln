#!/bin/bash
cd ~/g121-tmp/s18/kiln
export CARGO_TARGET_DIR=~/g121-tmp/s18/target
echo "START $(date -u +%FT%TZ) $(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
nice -n 10 cargo clippy --locked -p kiln -p kiln-ci --all-targets -j 8 --message-format short -- -D warnings > ../clippy.log 2>&1
echo "CLIPPY rc=$? $(date -u +%FT%TZ)"
