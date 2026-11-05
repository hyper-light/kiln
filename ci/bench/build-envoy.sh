#!/bin/bash
# Envoy's selection kernels (ci/bench/drivers/envoy/envoy_pick.cc) and its own stock LB benches, at
# the pinned release, with bazel -c opt (ci/bench/README.md). Envoy's build is hours long here: it
# runs in slices (ci/bench/envoy-bazel.sh: the swarm's build lock held for at most a slice, a
# capped low-priority unit, 100 GiB free floor) and resumes where the last slice stopped. Run it
# again until it says "built"; each slice's log is ~/.cache/kiln-lb/bench/logs/envoy-build.log.
# Usage: build-envoy.sh [slice seconds, default 1800]
set -uo pipefail
B="$HOME/.cache/kiln-lb/bench" HERE="$(builtin cd "$(dirname "$0")" && pwd)"
SRC="$B/src/envoy" TAG=v1.40.2 SHA=b2d89e0ae53916f28770771fbdd49c5c99c5c5a2 slice=${1:-1800}
[ -d "$SRC" ] || git clone -q --depth 1 -b "$TAG" https://github.com/envoyproxy/envoy "$SRC"
[ "$(git -C "$SRC" rev-parse HEAD)" = "$SHA" ] || { echo "envoy: $SRC is not $SHA" >&2; exit 1; }
pkg="$SRC/test/extensions/load_balancing_policies/kiln_bench"
mkdir -p "$pkg"
cp "$HERE/drivers/envoy/BUILD" "$HERE/drivers/envoy/envoy_pick.cc" "$HERE/drivers/common.h" "$pkg/"
# The stock benches beside ours, so their own numbers are on record too.
targets=(//test/extensions/load_balancing_policies/kiln_bench:envoy_pick
  //test/extensions/load_balancing_policies/least_request:least_request_lb_benchmark
  //test/extensions/load_balancing_policies/round_robin:round_robin_lb_benchmark
  //test/extensions/load_balancing_policies/random:random_lb_benchmark
  //test/common/upstream:scheduler_benchmark)
log="$B/logs/envoy-build.log"
echo "== slice $(date -Is), ${slice}s" >> "$log"
"$HERE/envoy-bazel.sh" "$log" "$slice" build -c opt --jobs=6 --color=no --curses=no --show_progress_rate_limit=60 "${targets[@]}"
s=$?
if [ $s -eq 0 ]; then
  mkdir -p "$B/bin"
  for t in "${targets[@]}"; do p=${t#//}; cp -f "$SRC/bazel-bin/${p/://}" "$B/bin/" && echo "built $B/bin/${t##*:}"; done
fi
exit $s
