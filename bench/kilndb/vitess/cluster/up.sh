#!/usr/bin/env bash
# Clustered MySQL for KilnDB's benchmarks (docs/kilndb.md, K0): Vitess on one host, every tablet its
# own mysqld. Keyspace `convo`, four shards by session, three tablets each (a primary and two
# replicas); durability `semi_sync`, so a commit waits for a replica to have it, as a Raft quorum of
# three does. vtorc elects the primaries. vtgate's MySQL protocol on 127.0.0.1:${PORT:-33578}.
#
# usage: up.sh [down]
set -euo pipefail
cd "$(dirname "$0")"
NET=kilndb-vt
IMG=kilndb-vt:latest
TOPO=(--topo-implementation etcd2 --topo-global-server-address etcd:2379 --topo-global-root /vitess/global)
SHARDS=(-40 40-80 80-c0 c0-)
PORT=${PORT:-33578}

down() {
  docker ps -aq --filter "label=kilndb-vt" | xargs -r docker rm -f >/dev/null
  docker network rm "$NET" >/dev/null 2>&1 || true
}
if [[ "${1:-}" == "down" ]]; then down; exit 0; fi
down

docker build -q -t "$IMG" . >/dev/null
docker network create "$NET" >/dev/null
run() { docker run -d --label kilndb-vt --network "$NET" "$@" >/dev/null; }

run --name etcd --hostname etcd quay.io/coreos/etcd:v3.5.17 etcd --name etcd --data-dir /tmp/etcd \
  --listen-client-urls http://0.0.0.0:2379 --advertise-client-urls http://etcd:2379
sleep 2
run --name vtctld --hostname vtctld "$IMG" vtctld "${TOPO[@]}" --cell zone1 --port 15000 --grpc-port 15999 \
  --service-map grpc-vtctl,grpc-vtctld --backup-storage-implementation file --file-backup-storage-root /vt/vtdataroot/backups
ctl() { docker exec vtctld vtctldclient --server localhost:15999 "$@"; }
for i in $(seq 1 30); do ctl GetCellInfoNames >/dev/null 2>&1 && break; sleep 1; done
ctl AddCellInfo --root /vitess/zone1 --server-address etcd:2379 zone1
ctl CreateKeyspace --durability-policy=semi_sync convo

uid=100
for shard in "${SHARDS[@]}"; do
  for t in 0 1 2; do
    uid=$((uid + 1))
    dir=$(printf 'vt_%010d' "$uid")
    run --name "tablet-$uid" --hostname "tablet-$uid" --memory 2g "$IMG" bash -c "
      mysqlctld --tablet-uid $uid --mysql-port 3306 --socket-file /vt/vtdataroot/mysqlctl.sock --wait-time 300s &
      until [ -S /vt/vtdataroot/$dir/mysql.sock ]; do sleep 1; done
      exec vttablet ${TOPO[*]} --tablet-path zone1-$uid --tablet-hostname tablet-$uid \
        --init-keyspace convo --init-shard $shard --init-tablet-type replica \
        --port 15000 --grpc-port 16000 --service-map grpc-queryservice,grpc-tabletmanager,grpc-updatestream \
        --mycnf-file /vt/vtdataroot/$dir/my.cnf --enable-replication-reporter \
        --backup-storage-implementation file --file-backup-storage-root /vt/vtdataroot/backups --restore-from-backup=false"
  done
done

run --name vtorc --hostname vtorc "$IMG" vtorc "${TOPO[@]}" --cell zone1 --port 15000 --instance-poll-time 1s
# Every shard with a primary, and its replicas replicating.
for i in $(seq 1 180); do
  n=$(ctl GetTablets --keyspace convo --tablet-type primary 2>/dev/null | wc -l || true)
  [[ "$n" -ge "${#SHARDS[@]}" ]] && break
  sleep 2
done
echo "primaries: $(ctl GetTablets --keyspace convo --tablet-type primary | wc -l) of ${#SHARDS[@]}"

docker cp ../schema/convo/001_events.sql vtctld:/tmp/events.sql
docker cp ../schema/convo/vschema.json vtctld:/tmp/vschema.json
ctl ApplySchema --sql-file /tmp/events.sql convo >/dev/null
ctl ApplyVSchema --vschema-file /tmp/vschema.json convo >/dev/null

run --name vtgate --hostname vtgate -p "127.0.0.1:$PORT:$PORT" "$IMG" vtgate "${TOPO[@]}" --cell zone1 --cells-to-watch zone1 \
  --tablet-types-to-wait PRIMARY,REPLICA --service-map grpc-vtgateservice --port 15001 --grpc-port 15991 \
  --mysql-server-port "$PORT" --mysql-auth-server-impl none
for i in $(seq 1 60); do
  docker exec vtgate mysql -h 127.0.0.1 -P "$PORT" -u root -e 'select 1' >/dev/null 2>&1 && break
  sleep 1
done
echo "vtgate on 127.0.0.1:$PORT"
ctl GetTablets --keyspace convo
