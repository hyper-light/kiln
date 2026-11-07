#!/bin/bash
# Can Playwright's WebKit, in its official image under rootless podman (default seccomp, no
# --privileged, a non-root user in the container), run inside a network namespace the lab made?
# The lab runs in podman's own user namespace (podman unshare), so the containers podman starts
# (in that same user namespace) may join the lab's client namespace.
set -u
G=$HOME/.cache/kiln-bp/g1/podman
IMG=mcr.microsoft.com/playwright:v1.64.1-noble
WEB=$(cd $(dirname $0)/../crates/kiln-lab/web && pwd)
mkdir -p $G
podman images --format '{{.Repository}}:{{.Tag}}' | grep -q playwright:v1.64.1-noble || { echo "no image yet"; exit 2; }
podman unshare bash -c '
  set -u
  G='"$G"'
  echo "in podman userns: uid $(id -u), caps $(grep CapEff /proc/self/status)"
  # The link (the lab'"'"'s own namespace) and a client, each held by a sleeper.
  unshare --net -- sleep 600 & L=$!
  unshare --net -- sleep 600 & H=$!
  sleep 0.5
  nsenter --target $L --net -- ip link add t0 type veth peer name eth0 netns $H 2>&1 && echo "veth: ok"
  nsenter --target $H --net -- ip addr add 10.99.0.2/24 dev eth0 && nsenter --target $H --net -- ip link set eth0 up
  nsenter --target $L --net -- ip addr add 10.99.0.1/24 dev t0; nsenter --target $L --net -- ip link set t0 up
  nsenter --target $L --net -- ip link add ifb0 type ifb && echo "ifb: ok"
  nsenter --target $H --net -- env TC_LIB_DIR=$HOME/.cache/kiln-bp/g1/tools/rpm/usr/lib64/tc $HOME/.cache/kiln-bp/g1/tools/rpm/usr/bin/tc qdisc add dev eth0 root netem delay 100ms 2>&1 && echo "netem in holder: ok"
  echo "holder netns: $(readlink /proc/$H/ns/net)"
  podman run --rm --network ns:/proc/$H/ns/net --user pwuser -v '"$WEB"':/web:ro,z '"$IMG"' bash -c "id; cat /proc/net/dev | tail -2; readlink /proc/self/ns/net; node -e \"const {webkit}=require(\\\"/web/node_modules/playwright\\\");(async()=>{const b=await webkit.launch();const p=await b.newPage();await p.setContent(\\\"<p>x</p>\\\");console.log(\\\"webkit ok\\\",b.version());await b.close()})().catch(e=>{console.log(\\\"webkit FAIL\\\",String(e).slice(0,400))})\"" 2>&1 | tail -8
  kill $H $L
'
