#!/bin/bash
# kiln for goal 7's lab (driver.sh): `kiln ... server` in a scope of its own at CPUWeight=1000, the weight
# kiln.slice gives the server (docs/priority.md), so the server alone has it; anything else (its runners) runs as
# it is, in the lab's unit. systemd-run --scope execs the command in its own process, so the lab's process group
# and the server's pid stay as they were.
D=$(cd "$(dirname "$0")" && pwd)
for a in "$@"; do
  if [ "$a" = server ]; then
    exec systemd-run --user --scope --quiet --collect -p CPUWeight=1000 -- "$D/bin/kiln" "$@"
  fi
done
exec "$D/bin/kiln" "$@"
