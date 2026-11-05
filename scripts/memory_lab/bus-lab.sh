#!/bin/bash
# The bus's memory bound, measured (goal 11, plan 01a1a87b2e56 step 3): the lab's realistic profile, 8 rounds calm
# and 8 with as many hogs as the server's CPUs, in production's layout, the bus sampled each second of every round
# (bus-hook.sh), then bus-summary.py's table and the bounds it gives: [bus] memory, and [bus] topic_bytes from each
# topic's bytes (kiln_bus_topic_bytes_peak).
#
#   bus-lab.sh <kiln> <kiln-lab> <out>
#     LAB_CPUS (8-15) and SERVER_CPUS (16-31): the lab's clients, and the server with its hogs (taskset);
#     ROUNDS (8); NEXT_HOOK: another hook to run after bus-hook.sh (goal 9's);
#     BUS_MEMORY: the [bus] memory each round's server starts with (kiln-lab --bus-memory), 1 GiB by default:
#       twice what the realistic profile's topics may keep of keys and payloads, its 32 sessions' (2 + 6 + 24,
#       kiln-lab's Profile::realistic) each at [bus] topic_bytes as it was before this measured it (16 MiB):
#       32 x 16 MiB x 2. The other half is for what Quiver charges beside them (each message's Focal framing:
#       1,083,392 bytes for the largest, 1,048,576 of payload) and the segments readers still hold after their
#       topic let them go. So no round reaches it and the peaks are what the load needs, past kiln's default if
#       it needs that.
# A round that refused frames or sent lag notices anyway is flagged by the summary: its peaks are floors.
# Each round's kiln (its data) is removed as the round ends: the samples are in <round>.hook, beside it, which is all
# bus-summary.py reads, and sixteen rounds' data would fill a shared lab machine's disk.
#
# Run it where kiln puts a lab (MCP run, needs "linux/amd64,cpus=16,mem=16g,exclusive"), in a unit of its own:
#   systemd-run --user --unit=kiln-bus-lab --working-directory=$HOME bus-lab.sh <kiln> <kiln-lab> ~/.cache/kiln-lab/bus
# Each run is one release build of kiln and of kiln-lab, both from the branch: the server's own bound is what's
# measured, and kiln-lab must know --bus-memory.
set -u
kiln=${1:?kiln} lab=${2:?kiln-lab} out=${3:?out}
here=$(cd "$(dirname "$0")" && pwd)
LAB_CPUS=${LAB_CPUS:-8-15} SERVER_CPUS=${SERVER_CPUS:-16-31} ROUNDS=${ROUNDS:-8}
BUS_MEMORY=${BUS_MEMORY:-$((32 * (16 << 20) * 2))}
mkdir -p "$out"
echo "$(date -u +%H:%M:%S) [bus] memory = $BUS_MEMORY for every round" | tee -a "$out/driver.log"
for cfg in calm hogs; do
  hogs=()
  [ "$cfg" = hogs ] && hogs=(--hogs 1x)
  KILN_LAB_HOOK="$here/bus-hook.sh" NEXT_HOOK="${NEXT_HOOK:-}" taskset -c "$LAB_CPUS" "$lab" run --kiln "$kiln" --label "bus-$cfg" \
    --rounds "$ROUNDS" --server-cpus "$SERVER_CPUS" --layout production --bus-memory "$BUS_MEMORY" "${hogs[@]}" --out "$out/$cfg" > "$out/$cfg.log" 2>&1
  echo "$(date -u +%H:%M:%S) $cfg: the lab exited $?" | tee -a "$out/driver.log"
done
python3 -I "$here/bus-summary.py" "$out/calm" "$out/hogs" | tee "$out/summary.md"
