#!/bin/bash
# Envoy's bazel, in a capped low-priority unit, one slice at a time (ci/bench/README.md).
# Usage: envoy-bazel.sh <log> <slice seconds|0> <bazel command and args...>
# A build slice holds the swarm's shared build lock (~/.cache/kiln-lb/build.lock) for at most
# <slice seconds>, so Envoy's hours-long build never starves the other agents' cargo builds; bazel
# resumes from its output base on the next slice. Refuses below 100 GiB free (the brief's floor).
set -u
log=$1 slice=$2; shift 2
B="$HOME/.cache/kiln-lb/bench"
avail=$(df --output=avail -B1G "$HOME/.cache" | tail -1 | tr -d ' ')
if [ "$avail" -lt 100 ]; then echo "envoy-bazel: only ${avail}G free under ~/.cache: not building" | tee -a "$log"; exit 90; fi
# The slice's bound is the unit's own (RuntimeMaxSec): systemd stops bazel and its server with it.
lock=() cap=()
[ "$slice" != 0 ] && lock=(flock "$HOME/.cache/kiln-lb/build.lock") cap=(-p "RuntimeMaxSec=$slice")
"${lock[@]}" systemd-run --user --unit "kiln-lb-bench-envoy-$(date +%s%N)" --collect --wait --pipe -q \
  -p MemoryMax=24G -p CPUWeight=1 -p Nice=19 "${cap[@]}" --working-directory="$B/src/envoy" \
  "$B/tools/bazelisk" "--output_user_root=$B/bazel-root" "$@" >> "$log" 2>&1
s=$?
echo "envoy-bazel exit $s ($(date -Is), $(df --output=avail -B1G "$HOME/.cache" | tail -1 | tr -d ' ')G free)" >> "$log"
exit $s
