#!/usr/bin/env bash
# Load an image from this machine's Docker into every node of a minikube cluster (docker driver,
# docker runtime), checked by image ID: how a published image that lives in the local Docker
# reaches the cluster (docs/cd-targets.md §7.1, §10). Nodes that already have it are left alone.
#   load-image.sh <image> [minikube profile; default $KILN_MINIKUBE_PROFILE, else kiln]
set -euo pipefail
IMAGE="${1:?usage: load-image.sh <image> [profile]}"
PROFILE="${2:-${KILN_MINIKUBE_PROFILE:-kiln}}"
WANT=$(docker image inspect -f '{{.Id}}' "$IMAGE")
NODES=$(docker ps --format '{{.Names}}' | grep -E "^${PROFILE}(-m[0-9]+)?$" || true)
if [ -z "$NODES" ]; then
    echo "no running node of minikube profile ${PROFILE}" >&2
    exit 1
fi
for node in $NODES; do
    if [ "$(docker exec "$node" docker image inspect -f '{{.Id}}' "$IMAGE" 2>/dev/null || true)" = "$WANT" ]; then
        echo "$node has $IMAGE"
        continue
    fi
    docker save "$IMAGE" | docker exec -i "$node" docker load -q >/dev/null
    GOT=$(docker exec "$node" docker image inspect -f '{{.Id}}' "$IMAGE")
    if [ "$GOT" != "$WANT" ]; then
        echo "$node holds $GOT for $IMAGE, not $WANT" >&2
        exit 1
    fi
    echo "loaded $IMAGE into $node"
done
