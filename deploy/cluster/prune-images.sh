#!/usr/bin/env bash
# Remove all but the newest images a pipeline published locally, from this machine's Docker and
# from every node of a minikube cluster, so building at every push doesn't fill their disks
# (docs/cd-targets.md §10). Only images labelled `kiln.published=<name>` (the image pipeline's
# own) are touched. An image any node's container uses stays everywhere, so a pod moved to another
# node finds it there. KilnCD rolls back within the kept ones; an older commit's image comes back
# by running its pipeline at that commit.
#   prune-images.sh <name> [keep; default 20] [minikube profile; default $KILN_MINIKUBE_PROFILE, else kiln]
set -euo pipefail
NAME="${1:?usage: prune-images.sh <name> [keep] [profile]}"
KEEP="${2:-20}"
PROFILE="${3:-${KILN_MINIKUBE_PROFILE:-kiln}}"
NODES=$(docker ps --format '{{.Names}}' | grep -E "^${PROFILE}(-m[0-9]+)?$" || true)

# The tags of the images the nodes' containers use (they name them by ID).
USED=$(mktemp)
trap 'rm -f "$USED"' EXIT
for node in $NODES; do
    containers=$(docker exec "$node" docker ps -aq)
    [ -n "$containers" ] || continue
    # shellcheck disable=SC2086 # one argument per container
    docker exec "$node" docker inspect -f '{{.Image}}' $containers | sort -u |
        xargs -r docker exec "$node" docker image inspect -f '{{range .RepoTags}}{{println .}}{{end}}' 2>/dev/null || true
done | sort -u >"$USED"

# prune <docker command>...: one Docker, this machine's or a node's (through docker exec). Newest
# first; an image's tags share its creation time, so they sort together and count once.
prune() {
    "$@" image ls --filter "label=kiln.published=$NAME" --format '{{.CreatedAt}}|{{.ID}}|{{.Repository}}:{{.Tag}}' |
        sort -r |
        awk -F'|' -v keep="$KEEP" '!($2 in seen) { seen[$2] = ++n } seen[$2] > keep { print ($3 ~ /<none>/ ? $2 : $3) }' |
        { grep -vxF -f "$USED" || true; } |
        while read -r ref; do
            if "$@" image rm "$ref" >/dev/null 2>&1; then echo "removed $ref"; fi
        done
}

prune docker
for node in $NODES; do
    prune docker exec "$node" docker
done
