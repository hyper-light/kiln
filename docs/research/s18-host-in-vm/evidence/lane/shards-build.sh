#!/bin/bash
set -u
cd ~/g121-tmp/s18
echo "START $(date -u +%FT%TZ)"
if [ ! -d shards/.git ]; then
  git clone -q --filter=blob:none --no-checkout git@github.com:hyper-light/shards.git shards || exit 2
fi
cd shards
git fetch -q git@github.com:hyper-light/shards.git main || { echo "fetch failed"; exit 3; }
git checkout -q -f e82b9403b863718930fe9f224186d3ae862bfd61 || exit 4
echo "HEAD $(git rev-parse --short HEAD)"
touch ../dummy-guest
export CARGO_TARGET_DIR=~/g121-tmp/s18/shards-target SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$HOME/g121-tmp/s18/dummy-guest SHARDS_SERVER_BINARY=$HOME/g121-tmp/s18/dummy-guest
nice -n 10 cargo build -p shards -j 6 > ../shards-build.log 2>&1
rc=$?
echo "BUILD rc=$rc $(date -u +%FT%TZ)"; ls -la ~/g121-tmp/s18/shards-target/debug/shards 2>&1
