#!/bin/bash
# Production's guard while the lab runs on its host (adacachy): every minute, production's /readyz
# (read only). Slower than 1 s or failing, the lab's unit is frozen (`systemctl --user freeze`:
# every process of it stopped where it stands) and that is logged; once /readyz has answered well
# for 5 minutes in a row, the unit is thawed and that is logged. Jobs a freeze overlapped are named
# by its window in the log, and the report leaves them out.
#   guard-prod.sh UNIT LOG
set -u
UNIT=$1; LOG=$2
frozen=0; good=0
while systemctl --user is-active -q "$UNIT" || [ $frozen = 1 ]; do
  r=$(curl -s -o /dev/null -m 5 -w '%{http_code} %{time_total}' http://127.0.0.1:7070/readyz || echo "000 5")
  code=${r% *}; t=${r#* }
  slow=$(python3 -c "print(1 if float('$t') > 1.0 else 0)")
  if [ "$code" != 200 ] || [ "$slow" = 1 ]; then
    good=0
    if [ $frozen = 0 ]; then
      systemctl --user freeze "$UNIT" && frozen=1
      echo "$(date -u +%FT%TZ) FREEZE $UNIT: /readyz $code in ${t}s" >> "$LOG"
    fi
  else
    good=$((good + 1))
    if [ $frozen = 1 ] && [ $good -ge 5 ]; then
      systemctl --user thaw "$UNIT" && frozen=0
      echo "$(date -u +%FT%TZ) THAW $UNIT: /readyz answered well 5 minutes in a row" >> "$LOG"
    fi
  fi
  sleep 60
done
echo "$(date -u +%FT%TZ) $UNIT ended; the guard ends" >> "$LOG"
