#!/usr/bin/env bash
# Goal 11 (plan 01a1a87b2e56 steps 1 and 3), trial only: an idle kiln server's memory, as the lab configures one
# (no load), for 60 s: RSS by kind at 5, 15, 30 and 60 s, then its /metrics (kiln_memory_*, kiln_shard_*), its
# smaps and a summary.   idle.sh KILN OUT [VAR=VALUE ...]
set -u
KILN=$1 OUT=$2; shift 2
mkdir -p "$OUT/run/data" "$OUT/run/home" "$OUT/run/xdg"; chmod 700 "$OUT/run/xdg"
read -r P Q < <(python3 -I -c 'import socket
a=socket.socket(); a.bind(("127.0.0.1",0)); b=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); b.bind(("127.0.0.1",0)); print(a.getsockname()[1], b.getsockname()[1])')
cat > "$OUT/run/kiln.toml" <<TOML
[server]
http_addr = "127.0.0.1:$P"
quic_addr = "127.0.0.1:$Q"
public_url = "http://127.0.0.1:$P"
quic_advertise = "127.0.0.1:$Q"
data_dir = "$OUT/run/data"
[gitops]
auto_pipelines = false
[local_agent]
enabled = false
[terminal]
enabled = false
[kubernetes]
enabled = false
[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false
[cd]
enabled = false
[ci]
enabled = false
TOML
env "$@" XDG_RUNTIME_DIR="$OUT/run/xdg" HOME="$OUT/run/home" KILN_MACHINE_ID="g11 idle" KILN_WARM_FLOOR=0 NO_COLOR=1 setsid "$KILN" --config "$OUT/run/kiln.toml" server > "$OUT/server.log" 2>&1 < /dev/null &
PID=$!
for t in 5 15 30 60; do
  sleep $(( t - ${last:-0} )); last=$t
  [ -d /proc/$PID ] || { echo "server gone at ${t}s"; tail -5 "$OUT/server.log"; exit 1; }
  echo "t=${t}s $(grep -E '^(VmRSS|RssAnon|RssFile|RssShmem|Threads)' /proc/$PID/status | tr -s ' \t' ' ' | tr '\n' ' ')"
done
cp /proc/$PID/smaps "$OUT/smaps"
curl -s "http://127.0.0.1:$P/metrics" -H "Authorization: Bearer $(cat "$OUT/run/data/admin.token" 2>/dev/null)" > "$OUT/metrics.txt" 2>/dev/null
grep -E '^(kiln_memory_|process_resident|kiln_shard_tasks|kiln_bus_memory)' "$OUT/metrics.txt"
kill -TERM $PID 2>/dev/null; for i in $(seq 20); do [ -d /proc/$PID ] || break; sleep 0.5; done; kill -KILL $PID 2>/dev/null; wait $PID 2>/dev/null
true
