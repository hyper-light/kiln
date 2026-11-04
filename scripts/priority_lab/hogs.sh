#!/bin/bash
# Goal 8's headroom lab (plan 01a1a59c2b29 step 5): drives the hogs of a lab headroom.sh started,
# each step noted in OUT/driver.log with the time, for timeline.py.
#
#   hogs.sh OUT legs [RUNS]            RUNS runs of legs.yaml: 16 busy threads each, 10 minutes
#   hogs.sh OUT agents N THREADS SECS  N agent sessions on the runner, each a turn of THREADS busy
#                                      threads for SECS seconds (the stand-in's lab:hog)
#   hogs.sh OUT headroom on|off        the runner's headroom on, or off (settings, as a person sets them)
#   hogs.sh OUT cancel                 every run of the legs cancelled
#   hogs.sh OUT end                    the recorder stopped and the runner with it
set -uo pipefail
OUT=$1 WHAT=$2
shift 2
# shellcheck source=/dev/null
. "$OUT/env.sh"
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$OUT/driver.log"; }
api() { curl -sS -m 30 -H "Authorization: Bearer $(cat "$ROUND/data/admin.token")" -H 'Content-Type: application/json' "$@"; }
S=http://127.0.0.1:$PORT
case "$WHAT" in
  legs)
    for _ in $(seq "${1:-1}"); do
      run=$(api -X POST "$S/api/repos/$REPO_ID/trigger" -d '{"branch":"main","pipeline":"legs"}' | python3 -c 'import json,sys; print(",".join(r["id"] for r in json.load(sys.stdin)))')
      say "legs: run $run"
      echo "$run" >> "$OUT/legs.runs"
    done
    ;;
  agents)
    n=$1 threads=$2 secs=$3
    for i in $(seq "$n"); do
      mkdir -p "$OUT/agent-work/a$i"
      id=$(api -X POST "$S/api/harness/sessions" -d "{\"target\":\"agent:$KEY\",\"spec\":{\"kind\":\"claude\",\"cwd\":\"$OUT/agent-work/a$i\",\"prompt\":\"lab:hog:$threads:$secs\"}}" | python3 -c 'import json,sys; v=json.load(sys.stdin); print(v[0]["id"] if isinstance(v,list) else v)')
      say "agents: session $id, $threads threads for ${secs}s"
    done
    ;;
  headroom)
    on=$([ "$1" = on ] && echo true || echo false)
    body=$(echo "$SETTINGS" | python3 -c 'import json,sys; s=json.load(sys.stdin); s["enabled"]=sys.argv[1]=="true"; print(json.dumps(s))' "$on")
    api -X PUT "$S/api/runners/$KEY/headroom" -d "$body" > /dev/null
    say "headroom: enabled $on"
    ;;
  cancel)
    for run in $(tr ',' '\n' < "$OUT/legs.runs" 2>/dev/null); do api -X POST "$S/api/runs/$run/cancel" > /dev/null; done
    say "legs: cancelled"
    ;;
  end)
    touch "$OUT/STOP"
    say "end"
    ;;
  *)
    echo "legs | agents N THREADS SECS | headroom on|off | cancel | end" >&2
    exit 2
    ;;
esac
