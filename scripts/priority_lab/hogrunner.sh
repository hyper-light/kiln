#!/bin/bash
# Step 7's lab, its fence configuration (docs/priority.md, "Step 7's lab"; goal 8, plan 01a1a59c2b29 step 7): the hogs
# as kiln's own work, placed by kiln. A runner of the build under test beside the round's server, held to RUNNER_CPUS
# in a scope of its own at cpu.weight 1 (where legs and agents run), its config declaring the server's cores as its
# machine's does (`[server] cores`), opens an agent whose turn is THREADS threads busy on the CPU for SECS (the lab's
# stand-in, `lab:hog`): kiln starts it on its work CPUs, the runner's less the server's. Then it notes the CPUs each of
# the agent's threads may run on, and whether any is the server's (OUT/fence.txt), and waits for OUT/hogrunner.stop;
# its scope stopped, everything it started goes with it.
#   hogrunner.sh KILN OUT SERVER_CORES RUNNER_CPUS THREADS SECS UNIT     (OUT: the round's kiln-lab --out)
set -u
kiln=$1 out=$2 cores=$3 cpus=$4 threads=$5 secs=$6 unit=$7
hr=$out/hogrunner
mkdir -p "$hr/home" "$hr/work" "$hr/state" "$hr/agent"
say() { echo "$(date -u +%H:%M:%S) $*"; }
stop() { systemctl --user stop "$unit.scope" 2>/dev/null; say "stopped"; }
round=""
for _ in $(seq 600); do
  [ -e "$out/hogrunner.stop" ] && exit 0
  round=$(ls -d "$out"/rounds/*/ 2>/dev/null | head -1)
  [ -n "$round" ] && [ -s "$round/data/admin.token" ] && [ -s "$round/data/join.token" ] && [ -s "$round/kiln.toml" ] && break
  sleep 1
done
port=$(sed -n 's/^http_addr = "127.0.0.1:\([0-9]*\)"/\1/p' "$round/kiln.toml" 2>/dev/null)
[ -n "$port" ] || { say "no server came up"; exit 1; }
api() { curl -sS -m 30 -H "Authorization: Bearer $(cat "$round/data/admin.token")" -H 'Content-Type: application/json' "$@"; }
# The server's cores in the runner's own config, as a runner beside the server reads the machine's.
printf '[server]\ncores = "%s"\n' "$cores" > "$hr/kiln.toml"
key=$(python3 -I -c 'import hashlib; print(hashlib.sha256(b"kiln machine id\nhog-runner").hexdigest()[:32])')
systemd-run --user --scope --quiet --collect --unit="$unit" -p CPUWeight=1 -p MemoryMax=16G \
  env -i HOME="$hr/home" PATH="$out/bin:$PATH" XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" USER="${USER:-}" LANG=C.UTF-8 \
  TMPDIR="${TMPDIR:-$hr}" KILN_MACHINE_ID=hog-runner KILN_WARM_FLOOR=0 NO_COLOR=1 \
  taskset -c "$cpus" "$kiln" --config "$hr/kiln.toml" agent --server "http://127.0.0.1:$port" --token "$(cat "$round/data/join.token")" \
  --name hog-runner --labels hogs --slots 4 --work-dir "$hr/work" --state-dir "$hr/state" > "$hr/runner.log" 2>&1 &
up=no
for _ in $(seq 120); do
  [ -e "$out/hogrunner.stop" ] && { stop; exit 0; }
  if api "http://127.0.0.1:$port/api/agents" | grep -q "\"$key\""; then up=yes; break; fi
  sleep 1
done
[ "$up" = yes ] || { say "the hog runner never connected: $(tail -3 "$hr/runner.log" | tr '\n' ' ')"; stop; exit 1; }
# Its headroom off, as the lab's own runners' is: the A/B is of where the hogs run, not of pacing them.
api -X PUT "http://127.0.0.1:$port/api/runners/$key/headroom" -d '{"enabled":false}' > /dev/null
id=$(api -X POST "http://127.0.0.1:$port/api/harness/sessions" -d "{\"target\":\"agent:$key\",\"spec\":{\"kind\":\"claude\",\"cwd\":\"$hr/agent\",\"prompt\":\"lab:hog:$threads:$secs\"}}" |
  python3 -I -c 'import json,sys; v=json.load(sys.stdin); print(v[0]["id"] if isinstance(v, list) else v.get("id", v))')
say "hog runner $key up on CPUs $cpus; agent $id: $threads threads for ${secs}s"
sleep 10
cg=/sys/fs/cgroup$(systemctl --user show "$unit.scope" -p ControlGroup --value)
{
  echo "# the server's cores $cores; the hog runner held to $cpus; what the runner said of its legs and agents:"
  grep -h "legs and agents" "$hr/runner.log" | head -2
  echo "# each process in the runner's scope: pid, name, the CPUs it may run on"
  for f in $(find "$cg" -name cgroup.procs 2>/dev/null); do cat "$f"; done | sort -un | while read -r p; do
    echo "$p $(cat "/proc/$p/comm" 2>/dev/null) $(awk '/^Cpus_allowed_list/ {print $2}' "/proc/$p/status" 2>/dev/null)"
  done
  python3 -I - "$cg" "$cores" <<'PY'
import os, sys
def cpus(text):
    out = set()
    for part in text.split(","):
        a, _, b = part.strip().partition("-")
        if a:
            out.update(range(int(a), int(b or a) + 1))
    return out
cg, server = sys.argv[1], cpus(sys.argv[2])
pids = set()
for root, _, files in os.walk(cg):
    if "cgroup.procs" in files:
        pids.update(int(x) for x in open(os.path.join(root, "cgroup.procs")).read().split())
seen, on = 0, []
for p in sorted(pids):
    try:
        if open(f"/proc/{p}/comm").read().strip() != "claude":
            continue
        for t in os.listdir(f"/proc/{p}/task"):
            allowed = next(l.split(":", 1)[1].strip() for l in open(f"/proc/{p}/task/{t}/status") if l.startswith("Cpus_allowed_list"))
            seen += 1
            if cpus(allowed) & server:
                on.append(f"{p}/{t}: {allowed}")
    except (OSError, StopIteration):
        continue
print(f"verdict: {seen} threads of the hog agent, {len(on)} of them may run on the server's cores" + (": " + "; ".join(on[:8]) if on else ""))
PY
} > "$out/fence.txt" 2>&1
say "$(tail -1 "$out/fence.txt")"
while [ ! -e "$out/hogrunner.stop" ]; do sleep 2; done
stop
