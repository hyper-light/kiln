#!/bin/bash
# kiln-lab's hook (KILN_LAB_HOOK) in goal 9's lab (plan 01a1a87b2cec steps 2 and 4, docs/runtime-lab.md "Goal 9").
# Every round: as its window ends, each of the server's threads and children with the CPUs it may run on and the one it
# last ran on (affinity.txt), so a round on cores of the server's own shows what shares the shards' cores. With PERF_ON=1
# (the driver's profiled rounds), perf records the round's server over its window too: every thread, its call graph by
# PERF_CALL_GRAPH (DWARF by default: a release build keeps .eh_frame; `fp` for a build with frame pointers), user space only (perf_event_paranoid 2 lets a user profile their own
# processes). The lab waits for the hook, so perf runs in the background from window-start until window-end stops it.
#   hook.sh <event> <server pid> <dir>     (PERF_ON; PERF: the perf binary; PERF_LIBS: a folder of its libraries;
#                                          PERF_CALL_GRAPH: perf record's --call-graph)
event=$1 pid=$2 dir=$3
case "$event" in
  window-start)
    if [ "${PERF_ON:-0}" = 1 ]; then
      LD_LIBRARY_PATH=${PERF_LIBS:-${LD_LIBRARY_PATH:-}} nohup "${PERF:-perf}" record -F 499 --call-graph "${PERF_CALL_GRAPH:-dwarf,16384}" -p "$pid" -o "$dir/perf.data" > "$dir/perf.log" 2>&1 &
      echo $! > "$dir/perf.pid"
    fi
    ;;
  window-end)
    {
      for t in /proc/"$pid"/task/*; do
        # comm, the CPUs it may run on, the CPU it last ran on (stat's 39th field, after the comm's closing parenthesis).
        printf '%s %s cpus=%s last=%s\n' "${t##*/}" "$(cat "$t/comm" 2>/dev/null)" "$(awk '/^Cpus_allowed_list:/ {print $2}' "$t/status" 2>/dev/null)" "$(sed 's/.*) //' "$t/stat" 2>/dev/null | awk '{print $37}')"
      done
      for c in $(cat /proc/"$pid"/task/*/children 2>/dev/null); do
        printf 'child %s %s cpus=%s\n' "$c" "$(cat "/proc/$c/comm" 2>/dev/null)" "$(awk '/^Cpus_allowed_list:/ {print $2}' "/proc/$c/status" 2>/dev/null)"
      done
    } > "$dir/affinity.txt" 2>/dev/null
    if [ -s "$dir/perf.pid" ]; then
      p=$(cat "$dir/perf.pid")
      kill -INT "$p" 2>/dev/null
      for _ in $(seq 300); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
    fi
    ;;
esac
exit 0
