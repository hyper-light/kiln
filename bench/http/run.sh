#!/bin/bash
# One measurement: a server on the CPUs in $SERVER_CPUS (default 0-3), the load generator on its
# own ($LOAD_CPUS, default 8-11), then the server stopped. Prints the load generator's line with the
# server's name and its CPU time (user and system seconds) during the run in front.
#
#   run.sh NAME PORT 'server command and flags' 'load flags'
#   run.sh hyper-tokio 8081 'hyper-tokio --threads 4' '--mode h1 --conns 256 --threads 4 --depth 16'
set -u
name=$1 port=$2 server=$3 load=$4
here=$(dirname "$(readlink -f "$0")")
bin=$here/target/release
server_cpus=${SERVER_CPUS:-0-3}
load_cpus=${LOAD_CPUS:-8-11}
# shellcheck disable=SC2086
taskset -c "$server_cpus" $bin/$server --port "$port" 2>/dev/null &
pid=$!
for _ in $(seq 100); do
  (exec 3<>/dev/tcp/127.0.0.1/"$port") 2>/dev/null && break
  sleep 0.05
done
ticks() { awk '{print $14, $15}' "/proc/$pid/stat"; }
read -r u0 s0 < <(ticks)
# shellcheck disable=SC2086
out=$(taskset -c "$load_cpus" $bin/load --addr 127.0.0.1:"$port" --cpus "$load_cpus" --server-pid "$pid" $load)
read -r u1 s1 < <(ticks)
kill "$pid"
wait "$pid" 2>/dev/null
hz=$(getconf CLK_TCK)
printf '{"server":"%s","user_s":%s,"sys_s":%s,%s\n' "$name" "$(awk "BEGIN{printf \"%.2f\", ($u1-$u0)/$hz}")" "$(awk "BEGIN{printf \"%.2f\", ($s1-$s0)/$hz}")" "${out#\{}"
