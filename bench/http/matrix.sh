#!/bin/bash
# The whole comparison: every workload against every server that speaks it, in interleaved rounds
# (a shared machine's noise falls on all of them alike), each run's line appended to $1. Then
# `python3 summarize.py --medians < $1` prints the medians as a table.
#
#   matrix.sh results.jsonl [rounds]
set -u
out=$1 rounds=${2:-3}
here=$(dirname "$(readlink -f "$0")")
run() { LOAD_CPUS=8-15 "$here/run.sh" "$@" >> "$out"; }
port=19000
next() { port=$((port + 1)); }
for r in $(seq "$rounds"); do
  echo "round $r: $(cat /proc/loadavg)" >&2
  for d in 1 16; do
    for s in "kiln-http|kiln-http --threads 4" "hyper-tokio|hyper-tokio --threads 4" "hyper-tokio-pf|hyper-tokio --threads 4 --pipeline-flush" "raw-epoll|raw-epoll --threads 4"; do
      [ "$d" = 1 ] && [ "${s%%|*}" = hyper-tokio-pf ] && continue
      next; run "${s%%|*}" $port "${s#*|}" "--mode h1 --conns 256 --threads 8 --depth $d --secs 5 --warmup 1"
    done
  done
  for m in "64 100" "256 10"; do
    set -- $m
    for s in "kiln-http|kiln-http --threads 4 --proto h2" "hyper-tokio|hyper-tokio --threads 4 --proto h2"; do
      next; run "${s%%|*}" $port "${s#*|}" "--mode h2 --conns $1 --threads 8 --streams $2 --secs 5 --warmup 1"
    done
  done
  for s in "kiln-http|kiln-http --threads 4" "hyper-tokio|hyper-tokio --threads 4" "raw-epoll|raw-epoll --threads 4"; do
    next; run "${s%%|*}" $port "${s#*|}" "--mode churn --conns 64 --threads 8 --secs 5 --warmup 1"
  done
  for s in "kiln-http|kiln-http --threads 4 --tasks 16384" "hyper-tokio|hyper-tokio --threads 4" "raw-epoll|raw-epoll --threads 4"; do
    next; LOAD_CPUS=8-15 "$here/run.sh" "${s%%|*}" $port "${s#*|}" "--mode idle --conns 50000 --secs 1" >> "$out"
  done
  for s in "kiln-http|kiln-http --threads 4 --proto h2 --tasks 16384" "hyper-tokio|hyper-tokio --threads 4 --proto h2"; do
    next; LOAD_CPUS=8-15 "$here/run.sh" "${s%%|*}" $port "${s#*|}" "--mode idle --idle-h2 --conns 20000 --secs 1" >> "$out"
  done
done
