#!/usr/bin/env bash
# Load an image from this machine's Docker into every node of a minikube cluster (docker driver,
# docker runtime): how a published image that lives in the local Docker reaches the cluster
# (docs/cd-targets.md §7.1, §10). Each node's copy is checked by the image's creation time, which
# a save and load keep (image IDs differ between Docker's image stores). Nodes that already have
# it are left alone. Errors go to the job's log too.
#   load-image.sh <image> [minikube profile; default $KILN_MINIKUBE_PROFILE, else kiln]
set -euo pipefail
IMAGE="${1:?usage: load-image.sh <image> [profile]}"
PROFILE="${2:-${KILN_MINIKUBE_PROFILE:-kiln}}"
WANT=$(docker image inspect -f '{{.Created}}' "$IMAGE")
NODES=$(docker ps --format '{{.Names}}' | grep -E "^${PROFILE}(-m[0-9]+)?$" || true)
if [ -z "$NODES" ]; then
    echo "no running node of minikube profile ${PROFILE}" | tee /dev/stderr
    exit 1
fi
for node in $NODES; do
    if [ "$(docker exec "$node" docker image inspect -f '{{.Created}}' "$IMAGE" 2>/dev/null || true)" = "$WANT" ]; then
        echo "$node has $IMAGE"
        continue
    fi
    docker save "$IMAGE" | docker exec -i "$node" docker load -q >/dev/null
    GOT=$(docker exec "$node" docker image inspect -f '{{.Created}}' "$IMAGE")
    if [ "$GOT" != "$WANT" ]; then
        echo "$node holds $IMAGE made $GOT, not the one made $WANT" | tee /dev/stderr
        exit 1
    fi
    echo "loaded $IMAGE into $node"
done
