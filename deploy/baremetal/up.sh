#!/usr/bin/env bash
# Start Prometheus + Grafana for a kiln server running on this host (no Kubernetes).
# Generates deploy/baremetal/.env (0600) with a random Grafana admin password on first run.
#
#   deploy/baremetal/up.sh          # start / update
#   deploy/baremetal/up.sh down     # stop (data volumes are kept)
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$DIR/.env"

if ! docker info >/dev/null 2>&1; then
    echo "error: cannot talk to docker. Run once: sudo $DIR/../cluster/privileged-setup.sh, then 'newgrp docker'." >&2
    exit 3
fi
docker compose version >/dev/null 2>&1 || { echo "error: the docker compose plugin is required" >&2; exit 3; }

if [ "${1:-up}" = down ]; then
    docker compose -f "$DIR/docker-compose.yml" --env-file "$ENV_FILE" down
    exit 0
fi

if [ ! -f "$ENV_FILE" ]; then
    (
        umask 077
        printf 'GRAFANA_ADMIN_USER=admin\nGRAFANA_ADMIN_PASSWORD=%s\n# KILN_OBS_BIND=0.0.0.0\n' \
            "$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')" >"$ENV_FILE"
    )
    echo "generated $ENV_FILE (Grafana admin credentials)"
fi

docker compose -f "$DIR/docker-compose.yml" --env-file "$ENV_FILE" up -d
# shellcheck disable=SC1090
. "$ENV_FILE"
cat <<EOF
Prometheus: http://localhost:${PROM_PORT:-9090}    (targets: http://localhost:${PROM_PORT:-9090}/targets)
Grafana:    http://localhost:${GRAFANA_PORT:-3000}    user ${GRAFANA_ADMIN_USER:-admin}, password in $ENV_FILE
kiln must listen on 0.0.0.0:7070 (or the docker bridge address) for Prometheus to reach it.
EOF
