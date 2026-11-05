#!/bin/bash
# kiln-lab's hook (KILN_LAB_HOOK) for goal 11, plan 01a1a87b2e56 step 3: the bus's memory bound, from the lab.
#
# The bus's bounds ([bus] memory and topic_bytes: docs/bus.md) are derived from what it samples (docs/runtime-lab.md,
# "Measuring the bus's bound"). This hook samples the round's server's /metrics once a second from fill-start to window-end, and keeps per
# sample: the bus's bytes and limit (kiln_bus_memory_bytes, kiln_bus_memory_limit_bytes), its topics and viewers,
# the most one topic has asked it to keep (kiln_bus_topic_bytes_peak: a feed's newest 2048 frames, by their keys and
# payloads, as topic_bytes counts them), the frames published (kiln_bus_published_total), what it refused (every
# kiln_bus_refused_total{op} series, summed) and the lag notices it sent (kiln_bus_lag_notices_total), every memory
# owner (kiln_memory_bytes{owner}) and the process's resident memory by kind. bus-summary.py reads the rounds'
# samples and says the bounds the measurement gives.
#
#   bus-hook.sh <event> <server pid> <round>.hook        (the lab runs it; NEXT_HOOK: a hook to run after it,
#                                                        such as scripts/shard_lab/hook.sh, with the same args)
#
# The round's server: its port from <round>/kiln.toml, its admin token from <round>/data/admin.token (read, never
# written anywhere). Samples go to <round>.hook/bus.tsv; the sampler's pid to <round>.hook/bus.pid. A scrape that
# didn't answer writes no line (so a counter never reads as falling back to nothing).
event=$1 pid=$2 dir=$3
round=${dir%.hook}
port=$(sed -n 's/^http_addr = "127\.0\.0\.1:\([0-9]*\)"$/\1/p' "$round/kiln.toml" 2>/dev/null | head -1)
sample() {
  # One line a second: unix ms, then name=value for each figure kept, while the server lives.
  local token
  token=$(cat "$round/data/admin.token" 2>/dev/null)
  while kill -0 "$pid" 2>/dev/null; do
    local now; now=$(date +%s%3N)
    curl -s --max-time 2 -H "Authorization: Bearer $token" "http://127.0.0.1:$port/metrics" 2>/dev/null \
      | awk -v now="$now" '
          BEGIN { refused = 0; seen = 0 }
          /^kiln_bus_refused_total[ {]/ { refused += $2; next }
          /^kiln_bus_(memory_bytes|memory_limit_bytes|topics|viewers|lag_notices_total|published_total|topic_bytes_peak) / { kept = kept " " $1 "=" $2; seen = 1 }
          /^kiln_memory_bytes\{owner="/ { o = $1; sub(/^kiln_memory_bytes\{owner="/, "", o); sub(/"\}$/, "", o); kept = kept " owner." o "=" $2 }
          /^kiln_memory_resident_bytes\{kind="/ { k = $1; sub(/^kiln_memory_resident_bytes\{kind="/, "", k); sub(/"\}$/, "", k); kept = kept " rss." k "=" $2 }
          END { if (seen) print now kept " kiln_bus_refused_total=" refused }' >> "$dir/bus.tsv"
    sleep 1
  done
}
case "$event" in
  fill-start)
    if [ -n "$port" ]; then
      sample > /dev/null 2>&1 &
      echo $! > "$dir/bus.pid"
    fi
    ;;
  fill-end) echo "$(date +%s%3N) fill-end" >> "$dir/bus.marks" ;;
  window-start) echo "$(date +%s%3N) window-start" >> "$dir/bus.marks" ;;
  window-end)
    echo "$(date +%s%3N) window-end" >> "$dir/bus.marks"
    if [ -s "$dir/bus.pid" ]; then
      kill "$(cat "$dir/bus.pid")" 2>/dev/null
    fi
    ;;
esac
if [ -n "${NEXT_HOOK:-}" ]; then
  "$NEXT_HOOK" "$@"
fi
exit 0
