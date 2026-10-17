#!/usr/bin/env bash
# Make the in-cluster kiln server reachable from OTHER machines on the LAN (bare-metal
# benchmark agents). minikube's docker-driver node IPs (192.168.49.x) only exist on this host,
# so we forward host ports to the kiln NodePorts with two tiny socat containers:
#
#   <this host>:7070/tcp → <node>:30070  (HTTP: UI/API/enroll)
#   <this host>:7071/udp → <node>:30071  (QUIC agent transport)
#
#   deploy/cluster/expose-lan.sh up | refresh | down | status
#
# `refresh` starts the UDP forward afresh; run it whenever the kiln pod is replaced (a rollout). An
# agent's forwarded QUIC packets reach the pod through a UDP path the node keeps for as long as
# packets flow, and an agent retrying against a pod that is gone keeps that path to the old pod
# alive; fresh forwarders take fresh paths, to the pod that is there now.
#
# Agents then join with server http://<this-host-LAN-IP>:7070 and QUIC <this-host-LAN-IP>:7071.
# Only 7070/tcp has to be open in your host firewall if one is active (e.g. ufw allow 7070/tcp):
# agents use 7071/udp when it is reachable and tunnel QUIC through 7070 when it is not.
set -euo pipefail
PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
SOCAT_IMAGE="${KILN_SOCAT_IMAGE:-alpine/socat:1.8.0.3}"
HTTP_PORT="${KILN_LAN_HTTP_PORT:-7070}"
QUIC_PORT="${KILN_LAN_QUIC_PORT:-7071}"
NODE_HTTP_PORT="${KILN_NODEPORT_HTTP:-30070}"
NODE_QUIC_PORT="${KILN_NODEPORT_QUIC:-30071}"

# The UDP forward: one forked child per agent (QUIC keeps a stable 4-tuple). A live QUIC session sends
# every 2 s, so a child idle for 30 s has lost its agent and exits.
quic_up() {
    docker rm -f kiln-lan-quic >/dev/null 2>&1 || true
    docker run -d --name kiln-lan-quic --restart unless-stopped --network host \
        --memory 32m --cpus 0.5 "$SOCAT_IMAGE" -T 30 \
        "UDP4-LISTEN:${QUIC_PORT},fork,reuseaddr" "UDP4:$1:${NODE_QUIC_PORT}" >/dev/null
}

cmd="${1:-status}"
case "$cmd" in
up)
    ip="$(minikube -p "$PROFILE" ip)"
    for c in kiln-lan-http kiln-lan-quic; do docker rm -f "$c" >/dev/null 2>&1 || true; done
    # nodelay on both legs: without it Nagle holds every small write behind the client's delayed
    # ACK, adding ~40 ms to each terminal echo (keystrokes, prompts) through the forwarder.
    docker run -d --name kiln-lan-http --restart unless-stopped --network host \
        --memory 32m --cpus 0.5 "$SOCAT_IMAGE" \
        "TCP-LISTEN:${HTTP_PORT},fork,reuseaddr,nodelay" "TCP:${ip}:${NODE_HTTP_PORT},nodelay" >/dev/null
    quic_up "$ip"
    lan_ip="$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i=1;i<=NF;i++) if ($i=="src") print $(i+1)}' | head -n1)"
    echo "forwarding ${lan_ip:-<lan-ip>}:${HTTP_PORT}/tcp → ${ip}:${NODE_HTTP_PORT}, ${lan_ip:-<lan-ip>}:${QUIC_PORT}/udp → ${ip}:${NODE_QUIC_PORT}"
    echo "note: set kiln publicURL/quicAdvertise for external agents to ${lan_ip:-<lan-ip>}:${HTTP_PORT}/${QUIC_PORT} (see deploy/README.md)"
    echo "note: only ${HTTP_PORT}/tcp must be open in this host's firewall; agents use ${QUIC_PORT}/udp when it is reachable and tunnel through HTTP when it is not"
    ;;
refresh)
    # Only the UDP forward: connections through the HTTP forward are untouched.
    if docker ps -a --format '{{.Names}}' | grep -qx kiln-lan-quic; then
        quic_up "$(minikube -p "$PROFILE" ip)"
        echo "UDP forward started afresh"
    else
        echo "no UDP forward to refresh (run: $0 up)"
    fi
    ;;
down)
    for c in kiln-lan-http kiln-lan-quic; do docker rm -f "$c" >/dev/null 2>&1 || true; done
    echo "stopped"
    ;;
status)
    docker ps --filter name=kiln-lan- --format '{{.Names}}\t{{.Status}}\t{{.Command}}'
    ;;
*)
    echo "usage: $0 up|refresh|down|status" >&2
    exit 2
    ;;
esac
