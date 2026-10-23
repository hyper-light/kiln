#!/usr/bin/env bash
# Quiver's ephemeral class against NATS core, Redis Pub/Sub and Kafka, class for class, on one
# machine (CLAUDE.md §7; bench/README.md, "The comparison"), beside the kernel's own wake-up (the
# floor under a subscriber that waits). Every broker up in Docker on CPUs of their own (Quiver and
# the floor have none: they run in the generator's process); then RUNS rounds of every load, each
# load's systems back to back on the generator's CPUs, their order turned each round, so on a
# machine whose load moves every system's runs of a load come from the same minutes. Every run is
# added to OUT/runs.tsv and printed to OUT/<system>.md; then OUT/report.md: each system's best run
# beside Quiver's, load by load.
#
#   bench/compare.sh OUT
#
# The loads and the placement, from the environment (defaults in brackets):
#   SYSTEMS [wake quiver nats redis kafka]  RATES [100000]  SUBSCRIBERS [1 100 1000]  WAITS [wait busy]
#   RUNS [3]  DURATION [30] seconds measured  WARMUP [5] seconds  PAYLOAD [256] bytes
#   BROKER_CPUS [8-15]: the broker's CPUs (on fedora's i9-14900HX, four P-cores, both threads each)
#   CLIENT_CPUS [0-7,16-31]: the generator's: its publisher and subscribers, and Quiver's node
#   LOAD_LIMIT [2]: the load average the first run must start under; a smoke test of the harness
#   on a busy machine raises it, and what it measures is no record
# librdkafka's build needs a C and a C++ compiler and make (bench/README.md).
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
# A thousand subscribers' connections, and Kafka's consumers' sockets, need more files than a
# session's soft limit (often 1,024): it's raised to the hard one.
ulimit -n "$(ulimit -Hn)"
out=${1:?usage: bench/compare.sh OUT}
mkdir -p "$out"
systems=${SYSTEMS:-wake quiver nats redis kafka}
rates=${RATES:-100000}
fanouts=${SUBSCRIBERS:-1 100 1000}
waits=${WAITS:-wait busy}
runs=${RUNS:-3}
duration=${DURATION:-30}
warmup=${WARMUP:-5}
payload=${PAYLOAD:-256}
broker_cpus=${BROKER_CPUS:-8-15}
client_cpus=${CLIENT_CPUS:-0-7,16-31}
load_limit=${LOAD_LIMIT:-2}

# The competitors' images, pinned by digest: Focal's comparison's (Focal's tools/compare/compose),
# NATS's and Redis's from Docker's official library through its ECR Public mirror.
nats_image=public.ecr.aws/docker/library/nats:2.12-alpine@sha256:b270f5e2428354c0335612694d7dd2fb588148e567a5757fdff325ef9c9332e6
redis_image=public.ecr.aws/docker/library/redis:8.2-alpine@sha256:b51665e66f00759be7c3152ad5ac3c66fb2f619c13ef62dea7cc1f9914524635
kafka_image=apache/kafka:4.1.0@sha256:bff074a5d0051dbc0bbbcd25b045bb1fe84833ec0d3c7c965d1797dd289ec88f

# A quiet machine before the first run (CLAUDE.md §7); each run's row has its load before and after,
# its own predecessors' included.
load=$(cut -d' ' -f1 /proc/loadavg)
if ! awk -v l="$load" -v limit="$load_limit" 'BEGIN { exit !(l < limit) }'; then
  echo "the load average is $load: a comparison needs a quiet machine (CLAUDE.md §7)" >&2
  exit 1
fi
QUIVER_COMMIT=$(git rev-parse HEAD)
export QUIVER_COMMIT
# Built before the first run, so no build runs beside one.
cargo bench --locked -q -p quiver-bench --bench fanout --no-run

# The brokers' containers, by system.
declare -A containers=()
stop() {
  for system in "${!containers[@]}"; do
    docker stop -t 10 "${containers[$system]}" >/dev/null 2>&1 || true
  done
  containers=()
}
trap stop EXIT
# Stopped (a watchdog, systemctl stop), it exits, so the brokers it started stop with it.
trap 'exit 143' TERM INT

# Each broker as shipped, on loopback alone, on the brokers' CPUs, 8 GiB of memory; its container
# into `containers`.
start() {
  local limits=(--rm --detach --network host --cpuset-cpus "$broker_cpus" --memory 8g --name "quiver-compare-$1-$$")
  case $1 in
    nats) containers[nats]=$(docker run "${limits[@]}" "$nats_image" --addr 127.0.0.1 --port 14222) ;;
    redis) containers[redis]=$(docker run "${limits[@]}" "$redis_image" redis-server --bind 127.0.0.1 --port 16380) ;;
    kafka)
      # One KRaft broker, as Focal's comparison sets one (its pubsub.yaml), on loopback.
      containers[kafka]=$(docker run "${limits[@]}" \
        -e KAFKA_NODE_ID=1 \
        -e KAFKA_PROCESS_ROLES=broker,controller \
        -e KAFKA_LISTENERS=PLAINTEXT://127.0.0.1:19092,CONTROLLER://127.0.0.1:19093 \
        -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://127.0.0.1:19092 \
        -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
        -e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=PLAINTEXT:PLAINTEXT,CONTROLLER:PLAINTEXT \
        -e KAFKA_CONTROLLER_QUORUM_VOTERS=1@127.0.0.1:19093 \
        -e CLUSTER_ID=5L6g3nShT-eMCtK--X86sw \
        -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 \
        -e KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1 \
        -e KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1 \
        -e KAFKA_GROUP_INITIAL_REBALANCE_DELAY_MS=0 \
        -e KAFKA_HEAP_OPTS="-Xms1g -Xmx1g" \
        "$kafka_image") ;;
    *) echo "unknown system $1" >&2; exit 2 ;;
  esac
}

# The broker is ready when it says so; at most a minute.
ready() {
  local c=${containers[$1]}
  case $1 in
    nats) docker logs "$c" 2>&1 | grep -q 'Server is ready' ;;
    redis) docker exec "$c" redis-cli -p 16380 ping 2>/dev/null | grep -q PONG ;;
    kafka) docker logs "$c" 2>&1 | grep -q 'Kafka Server started' ;;
  esac
}

# Every broker up before the first run, side by side on the brokers' CPUs (one idles while another
# is measured), so the runs interleave: on a machine whose load moves, each load's runs of every
# system come from the same minutes, not one system's hour against another's.
for system in $systems; do
  case $system in quiver | wake) continue ;; esac
  start "$system"
  n=0
  until ready "$system"; do
    n=$((n + 1))
    if [ "$n" -ge 60 ]; then
      docker logs --tail 40 "${containers[$system]}" >&2 || true
      exit 1
    fi
    sleep 1
  done
  docker inspect --format '- broker: {{.Config.Image}}, CPUs {{.HostConfig.CpusetCpus}}' "${containers[$system]}" >> "$out/$system.md"
done

# Run r of every load, each load's systems back to back, their order turned by one each run (no
# system always first or last).
read -r -a order <<< "$systems"
for r in $(seq 1 "$runs"); do
  for wait in $waits; do
    for rate in $rates; do
      for subscribers in $fanouts; do
        for system in "${order[@]}"; do
          flags=(--system "$system" --rate "$rate" --subscribers "$subscribers" --runs 1
            --seed $((0x5eed + r - 1)) --seconds "$duration" --warmup "$warmup" --payload "$payload"
            --max-load 1000 --out "$out/runs.tsv")
          if [ "$wait" = busy ]; then flags+=(--busy); fi
          if [ -n "${containers[$system]:-}" ]; then
            flags+=(--broker-cgroup "/sys/fs/cgroup/system.slice/docker-${containers[$system]}.scope")
          fi
          taskset -c "$client_cpus" cargo bench --locked -q -p quiver-bench --bench fanout -- \
            fanout "${flags[@]}" | tee -a "$out/$system.md"
        done
      done
    done
  done
  order=("${order[@]:1}" "${order[0]}")
done
cargo bench --locked -q -p quiver-bench --bench fanout -- report "$out/runs.tsv" | tee "$out/report.md"
