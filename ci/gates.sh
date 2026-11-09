#!/usr/bin/env bash
# A change's gate, side by side (AGENTS.md, "Your change's gate"; W05-R03): a run of ci/gate.sh per
# --needs, all at once, each through `kiln run --needs`, never detached. It waits for each by its
# PID, keeps each run's output in a file of its own and shows it, labelled, as that run ends; it
# passes only when every run exited 0 and said GATE: PASS. A run that ends any other way fails it,
# a client stopped or detached that exits 0 with no verdict too:
#
#   ci/gates.sh --needs linux/amd64,cpus=<N>,mem=<M> --since origin/main \
#               --needs macos,cpus=<N>,mem=<M> clippy --since origin/main \
#               --needs windows,cpus=<N>,mem=<M> clippy --since origin/main
#
# Each --needs starts a run: what it asks kiln for (the CPUs and memory its steps were measured to
# need), then that run's ci/gate.sh arguments. Its own last line is its verdict: GATE: PASS (exit
# 0); GATE: FAIL (exit 1), with each run's verdict above it; GATE: YIELDED (exit 75) when none
# failed but one yielded (its guard stopped it for the machine's sake: run that one again later, or
# elsewhere); GATE: STOPPED (128 and the signal) when stopped from outside, its runs stopped first.
# Exit 2: it was asked wrong. Its runs' outputs are kept under ${XDG_CACHE_HOME:-~/.cache}/kiln-gates.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)

needs=() first=() count=() args=()
while [ $# -gt 0 ]; do
  case $1 in
    -h | --help) sed -n '2,/^set -euo pipefail/p' "$here/ci/gates.sh" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    --needs)
      case ${2-} in '' | -*) echo "ci/gates.sh: --needs takes what its run needs (linux/amd64,cpus=<N>,mem=<M>)" >&2; exit 2 ;; esac
      needs+=("$2") first+=(${#args[@]}) count+=(0)
      shift 2
      continue
      ;;
  esac
  if [ ${#needs[@]} -eq 0 ]; then echo "ci/gates.sh: each run starts with --needs; $1 comes before any (--help says how)" >&2; exit 2; fi
  n=$((${#needs[@]} - 1))
  args+=("$1") count[$n]=$((count[n] + 1))
  shift
done
if [ ${#needs[@]} -eq 0 ]; then echo "ci/gates.sh: no runs: each starts with --needs (--help says how)" >&2; exit 2; fi

# A word as the shell on the runner reads it back: kiln run sends the command as one line of text.
word() {
  case $1 in
    '' | *[!A-Za-z0-9_./:@%+=,-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s' "$1" ;;
  esac
}

out=${XDG_CACHE_HOME:-$HOME/.cache}/kiln-gates/$(date -u +%Y%m%dT%H%M%SZ)-$$
mkdir -p "$out"
cd "$here"
pids=() labels=() logs=()
i=0
while [ $i -lt ${#needs[@]} ]; do
  # Its label: its number and the platform it asks for (the first of its needs that's no amount).
  platform=$(printf '%s\n' "${needs[$i]}" | tr ',' '\n' | awk '!/[=<>]/ && !/^[+]/ { print; exit }')
  label="$((i + 1)) ${platform:-any machine}"
  log=$out/$((i + 1))-$(printf '%s' "${platform:-any}" | tr -c 'A-Za-z0-9._-' '_').log
  cmd=(kiln run --needs "${needs[$i]}" -- ci/gate.sh)
  j=${first[$i]} end=$((first[i] + count[i]))
  while [ $j -lt $end ]; do cmd+=("$(word "${args[$j]}")"); j=$((j + 1)); done
  "${cmd[@]}" > "$log" 2>&1 < /dev/null &
  pids+=($!) labels+=("$label") logs+=("$log")
  echo "[$label] started (pid $!): ${cmd[*]}; its output: $log"
  i=$((i + 1))
done

# Stopped from outside: each run's client gets the signal, as kiln run sends it on to the command
# over there; each run is still waited for, its verdict shown.
signal=''
on_signal() {
  signal=$1
  trap - TERM INT HUP
  for p in "${pids[@]}"; do kill -"$1" "$p" 2> /dev/null || true; done
}
trap 'on_signal TERM' TERM
trap 'on_signal INT' INT
trap 'on_signal HUP' HUP

failed=0 yielded=0 summary=''
i=0
while [ $i -lt ${#pids[@]} ]; do
  pid=${pids[$i]}
  while :; do
    code=0
    wait "$pid" || code=$?
    # A signal of its own ends wait early: wait on while the run goes on.
    if [ $code -gt 128 ] && kill -0 "$pid" 2> /dev/null; then continue; fi
    break
  done
  verdict=$(awk '/^GATE: / { v = $0 } END { print v }' "${logs[$i]}")
  awk -v l="[${labels[$i]}] " '{ print l $0 }' "${logs[$i]}"
  case $code:$verdict in
    "0:GATE: PASS" | "0:GATE: PASS "*) said=PASS ;;
    "75:GATE: YIELDED"*) said=YIELDED yielded=$((yielded + 1)) ;;
    *) said=FAIL failed=$((failed + 1)) ;;
  esac
  summary="$summary[${labels[$i]}] $said: exit $code, ${verdict:-and no GATE line}"$'\n'
  i=$((i + 1))
done
printf '%s' "$summary"
echo "outputs: $out"
runs=${#pids[@]}
if [ -n "$signal" ]; then
  case $signal in TERM) code=143 ;; INT) code=130 ;; *) code=129 ;; esac
  echo "GATE: STOPPED (by SIG$signal: $failed of $runs runs failed, $yielded yielded)"
  exit $code
fi
if [ $failed -gt 0 ]; then
  echo "GATE: FAIL ($failed of $runs runs failed$([ $yielded -eq 0 ] || echo ", $yielded yielded"))"
  exit 1
fi
if [ $yielded -gt 0 ]; then
  echo "GATE: YIELDED ($yielded of $runs runs yielded to a strained machine, none failed: run those again later, or elsewhere)"
  exit 75
fi
echo "GATE: PASS (all $runs runs passed)"
exit 0
