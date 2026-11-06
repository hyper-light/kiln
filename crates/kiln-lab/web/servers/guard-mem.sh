#!/bin/bash
# The lab machine's memory guard (plan 01a1a832e9cc step 8, on a machine others share): every 20 s,
# its available memory. Under LOW_GB (3), the newest-started of the lab's parts (`kiln-lab browser
# run` processes in this user's kiln-bp-g1* units) has its unit frozen, and that is logged with the
# parts in that unit; once over HIGH_GB (6) again, the unit frozen last is thawed and logged. A
# part a freeze overlapped is named in the log: its jobs over the window are dropped and rerun.
#   guard-mem.sh LOG [LOW_GB HIGH_GB]
set -u
LOG=$1; LOW=${2:-3}; HIGH=${3:-6}
frozen=()
avail_mb() { awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo; }
newest_unit() { # the unit of the newest kiln-lab run process not yet frozen, and its parts
  for p in $(pgrep -u $(id -u) -x kiln-lab); do
    grep -q ' browser run ' <(tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null) || continue
    local u; u=$(grep -o 'kiln-bp-g1[^/]*\.service' /proc/$p/cgroup 2>/dev/null | head -1)
    [ -n "$u" ] || continue
    echo "$(stat -c %Y /proc/$p) $u"
  done | sort -rn | awk '{print $2}' | while read u; do
    case " ${frozen[*]:-} " in *" $u "*) continue;; esac
    echo "$u"; break
  done
}
parts_of() { for p in $(pgrep -u $(id -u) -x kiln-lab); do grep -q "kiln-bp-g1" /proc/$p/cgroup 2>/dev/null && grep -q "/$1.service" /proc/$p/cgroup && tr '\0' '\n' < /proc/$p/cmdline | grep -A1 -x -- --out | tail -1 | xargs -r basename; done | sort -u | tr '\n' ' '; }
while true; do
  a=$(avail_mb)
  if [ $a -lt $((LOW * 1024)) ]; then
    u=$(newest_unit)
    if [ -n "$u" ]; then
      systemctl --user freeze "$u" && frozen+=("$u") && echo "$(date -u +%FT%TZ) FREEZE $u ($(parts_of ${u%.service})): ${a} MB available" >> "$LOG"
    fi
  elif [ $a -gt $((HIGH * 1024)) ] && [ ${#frozen[@]} -gt 0 ]; then
    u=${frozen[-1]}; unset 'frozen[-1]'
    systemctl --user thaw "$u"; echo "$(date -u +%FT%TZ) THAW $u ($(parts_of ${u%.service})): ${a} MB available" >> "$LOG"
  fi
  sleep 20
done
