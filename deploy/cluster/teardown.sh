#!/usr/bin/env bash
# Tear down the kiln dev cluster.
#
#   deploy/cluster/teardown.sh            # asks before deleting
#   deploy/cluster/teardown.sh --yes      # no prompt (alias: --unattended)
#   deploy/cluster/teardown.sh --stop     # only stop the minikube nodes (keeps all data)
#   deploy/cluster/teardown.sh --purge    # also forget generated credentials
set -euo pipefail
PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
CRED_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/kiln/cluster"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
YES=0 STOP=0 PURGE=0
for a in "$@"; do
    case "$a" in
    -y | --yes | --unattended) YES=1 ;;
    --stop) STOP=1 ;;
    --purge) PURGE=1 ;;
    -h | --help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done
[ -t 0 ] || YES=1

"$SCRIPT_DIR/expose-lan.sh" down >/dev/null 2>&1 || true

if [ "$STOP" = 1 ]; then
    minikube -p "$PROFILE" stop
    exit 0
fi

if [ "$YES" = 0 ]; then
    read -r -p "Delete minikube profile '$PROFILE' and ALL its data (kiln DB, Prometheus, caches)? [y/N] " ans
    [[ "${ans:-n}" =~ ^[Yy] ]] || { echo "aborted"; exit 1; }
fi
minikube delete -p "$PROFILE"
if [ "$PURGE" = 1 ] && [ -d "$CRED_DIR" ]; then
    rm -f -- "$CRED_DIR/credentials.env"
    echo "removed $CRED_DIR/credentials.env"
fi
echo "done. Local images (kiln:dev, kiln-git:dev) are kept; remove with: docker image rm kiln:dev kiln-git:dev"
