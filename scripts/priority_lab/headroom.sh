#!/bin/bash
# Goal 8's headroom lab (plan 01a1a59c2b29 step 5, docs/priority.md "headroom listens to the
# server"): the lab's server under its realistic load (kiln-lab, one long round), and beside it a
# runner of the same build with headroom on, whose CI legs and agents are CPU hogs it started
# (hogs.sh starts them, raises them, and turns its headroom off and on). record.py notes what the
# server and headroom say meanwhile; timeline.py reads it back.
#
#   headroom.sh KILN KILN_LAB OUT [WINDOW_S]
#
# The server's unit at CPUWeight=$SERVER_WEIGHT (kiln.slice's, 1000), the runner in a scope of its
# own at $RUNNER_WEIGHT (100: a runner's unit beside it); the lab, and the runner's legs and its
# agents' hogs, held to CPUs $CPUS. Writes OUT/env.sh for hogs.sh. Run it in a unit of its own:
# systemd-run --user --unit=... bash headroom.sh ...
set -uo pipefail
KILN=$1 LAB=$2 OUT=$3 WINDOW=${4:-3600}
CPUS=${CPUS:-0-15} SERVER_WEIGHT=${SERVER_WEIGHT:-1000} RUNNER_WEIGHT=${RUNNER_WEIGHT:-100}
NAME=$(basename "$OUT")
D=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$OUT/driver.log"; }
say "start: $NAME, CPUs $CPUS, server at $SERVER_WEIGHT, runner at $RUNNER_WEIGHT, window ${WINDOW}s; load $(cut -d' ' -f1-3 /proc/loadavg)"

# The lab: one round, its window long enough for every phase.
systemd-run --user --quiet --collect --unit="g8b-s5-lab-$NAME" -p CPUWeight="$SERVER_WEIGHT" -p MemoryMax=16G \
  --working-directory="$OUT" -E HOME="$HOME" -E PATH="$PATH" \
  bash -c "taskset -c $CPUS $LAB run --kiln $KILN --label main --rounds 1 --window $WINDOW --out $OUT/lab > $OUT/lab.log 2>&1"
ROUND=$OUT/lab/rounds/001-main
for _ in $(seq 600); do [ -s "$ROUND/data/admin.token" ] && [ -s "$ROUND/data/join.token" ] && break; sleep 1; done
PORT=$(sed -n 's/^http_addr = "127.0.0.1:\([0-9]*\)"/\1/p' "$ROUND/kiln.toml")
[ -n "$PORT" ] || { say "no server"; exit 1; }
auth() { echo "Authorization: Bearer $(cat "$ROUND/data/admin.token")"; }
api() { curl -sS -m 30 -H "$(auth)" -H 'Content-Type: application/json' "$@"; }
say "server on $PORT"
# The fill: done once the lab's window loads pages (its page loader, about 50 files a second; a
# runner's own few GETs outside /api count as pages too).
for _ in $(seq 3600); do
  pages=$(api "http://127.0.0.1:$PORT/metrics" | sed -n 's/^kiln_route_seconds_count{class="page"} \([0-9]*\).*/\1/p')
  [ "${pages:-0}" -gt 200 ] && break
  sleep 1
done
say "filled: the window's load is on ($pages pages so far)"

# The runner beside it: a scope of its own at its weight, its headroom on (and never pacing the
# user's other units: escapes off), its claude the lab's stand-in. The runner itself isn't held to
# the lab's CPUs (pinned, it counts only those as its cores and, with others' work loading the
# machine past twice that, kiln holds its CI jobs as on an overloaded machine): its legs and its
# agents' hogs are, by their own taskset.
KEY=$(python3 -c 'import hashlib; print(hashlib.sha256(b"kiln machine id\nhog-runner").hexdigest()[:32])')
mkdir -p "$OUT/hog-state/headroom" "$OUT/hog-home" "$OUT/hog-work" "$OUT/hog-bin"
SETTINGS='{"enabled":true,"automatic":true,"targets":[],"false_alarms_per_day":1.0,"probe_load_share":0.1,"escapes":false}'
echo "$SETTINGS" > "$OUT/hog-state/headroom/settings.json"
printf '#!/bin/sh\nexec taskset -c %s %s "$@"\n' "$CPUS" "$OUT/lab/bin/claude" > "$OUT/hog-bin/claude"
chmod +x "$OUT/hog-bin/claude"
systemd-run --user --scope --quiet --collect --unit="g8b-s5-hogs-$NAME" -p CPUWeight="$RUNNER_WEIGHT" -p Delegate=yes \
  env -i HOME="$OUT/hog-home" PATH="$OUT/hog-bin:$PATH" XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" USER="$USER" LANG=C.UTF-8 \
  KILN_MACHINE_ID=hog-runner KILN_WARM_FLOOR=0 NO_COLOR=1 RUST_LOG=info \
  "$KILN" agent --server "http://127.0.0.1:$PORT" --token "$(cat "$ROUND/data/join.token")" --name hog-runner --labels hogs --slots 16 \
  --work-dir "$OUT/hog-work" --state-dir "$OUT/hog-state" > "$OUT/hog-runner.log" 2>&1 &
for _ in $(seq 120); do
  api "http://127.0.0.1:$PORT/api/agents" | grep -q "\"$KEY\"" && break
  sleep 1
done
api -X PUT "http://127.0.0.1:$PORT/api/runners/$KEY/headroom" -d "$SETTINGS" > /dev/null
SCOPE=/sys/fs/cgroup$(systemctl --user show "g8b-s5-hogs-$NAME.scope" -p ControlGroup --value)
say "hog runner $KEY in $SCOPE: $(api "http://127.0.0.1:$PORT/api/runners/$KEY/headroom" | head -c 300)"
# SAME_CGROUP=1: the server's process joins the runner's scope, where it competes thread by thread
# with the runner's legs and agents (their groups share no CPU controller there), as a server's own
# runner holds them when the server isn't a delegated unit of its own.
if [ "${SAME_CGROUP:-0}" = 1 ]; then
  LABCG=/sys/fs/cgroup$(systemctl --user show "g8b-s5-lab-$NAME.service" -p ControlGroup --value)
  for pid in $(cat "$LABCG/cgroup.procs"); do
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q -- "$ROUND/kiln.toml server"; then
      echo "$pid" > "$SCOPE/cgroup.procs" && say "the server ($pid) shares the runner's cgroup: $(cat "/proc/$pid/cgroup")"
    fi
  done
fi

# The legs' repository: a pipeline run by hand, each job busy on the CPU.
REPO=$OUT/hogs-repo
mkdir -p "$REPO/.kiln"
sed "s/@CPUS@/$CPUS/g" "$D/legs.yaml" > "$REPO/.kiln/legs.yaml"
git -C "$REPO" init -q -b main && git -C "$REPO" add . && git -C "$REPO" -c user.name=lab -c user.email=lab@example.com commit -qm legs
REPO_ID=$(api -X POST "http://127.0.0.1:$PORT/api/repos" -d "{\"name\":\"lab/hogs\",\"clone_url\":\"file://$REPO\",\"branches\":[\"main\"],\"poll_seconds\":5}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
say "hogs repo $REPO_ID"

cat > "$OUT/env.sh" <<EOF
PORT=$PORT
ROUND=$ROUND
KEY=$KEY
SCOPE=$SCOPE
REPO_ID=$REPO_ID
SETTINGS='$SETTINGS'
EOF
python3 -I "$D/record.py" "$PORT" "$ROUND/data/admin.token" "$KEY" "$SCOPE" "$OUT" &
REC=$!
say "recording ($REC); hogs.sh $OUT <what> drives the hogs"
# Until the lab's round ends (its window), or STOP.
while systemctl --user is-active -q "g8b-s5-lab-$NAME" && [ ! -e "$OUT/STOP" ]; do sleep 5; done
touch "$OUT/STOP"
wait "$REC"
systemctl --user stop "g8b-s5-hogs-$NAME.scope" 2>/dev/null
say "done; $(tail -3 "$OUT/lab.log" | tr '\n' ' ')"
