#!/usr/bin/env bash
# Make the in-cluster kiln server reachable from OTHER machines on the LAN (bare-metal
# benchmark agents). minikube's docker-driver node IPs (192.168.49.x) only exist on this host,
# so we forward host ports to the kiln NodePorts with two tiny socat containers:
#
#   <this host>:7070/tcp → <node>:30070  (HTTP: UI/API/enroll)
#   <this host>:7071/udp → <node>:30071  (QUIC agent transport)
#
#   deploy/cluster/expose-lan.sh up | down | status
#
# Agents then join with server http://<this-host-LAN-IP>:7070 and QUIC <this-host-LAN-IP>:7071.
# Open the ports in your host firewall if one is active (e.g. ufw allow 7070/tcp; ufw allow 7071/udp).
set -euo pipefail
PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
SOCAT_IMAGE="${KILN_SOCAT_IMAGE:-alpine/socat:1.8.0.3}"
HTTP_PORT="${KILN_LAN_HTTP_PORT:-7070}"
QUIC_PORT="${KILN_LAN_QUIC_PORT:-7071}"
NODE_HTTP_PORT="${KILN_NODEPORT_HTTP:-30070}"
NODE_QUIC_PORT="${KILN_NODEPORT_QUIC:-30071}"

cmd="${1:-status}"
case "$cmd" in
up)
    ip="$(minikube -p "$PROFILE" ip)"
    for c in kiln-lan-http kiln-lan-quic; do docker rm -f "$c" >/dev/null 2>&1 || true; done
    docker run -d --name kiln-lan-http --restart unless-stopped --network host \
        --memory 32m --cpus 0.5 "$SOCAT_IMAGE" \
        "TCP-LISTEN:${HTTP_PORT},fork,reuseaddr" "TCP:${ip}:${NODE_HTTP_PORT}" >/dev/null
    # UDP: one forked child per agent (QUIC keeps a stable 4-tuple); idle children exit after 10 min
    docker run -d --name kiln-lan-quic --restart unless-stopped --network host \
        --memory 32m --cpus 0.5 "$SOCAT_IMAGE" -T 600 \
        "UDP4-LISTEN:${QUIC_PORT},fork,reuseaddr" "UDP4:${ip}:${NODE_QUIC_PORT}" >/dev/null
    lan_ip="$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i=1;i<=NF;i++) if ($i=="src") print $(i+1)}' | head -n1)"
    echo "forwarding ${lan_ip:-<lan-ip>}:${HTTP_PORT}/tcp → ${ip}:${NODE_HTTP_PORT}, ${lan_ip:-<lan-ip>}:${QUIC_PORT}/udp → ${ip}:${NODE_QUIC_PORT}"
    echo "note: set kiln publicURL/quicAdvertise for external agents to ${lan_ip:-<lan-ip>}:${HTTP_PORT}/${QUIC_PORT} (see deploy/README.md)"
    ;;
down)
    for c in kiln-lan-http kiln-lan-quic; do docker rm -f "$c" >/dev/null 2>&1 || true; done
    echo "stopped"
    ;;
status)
    docker ps --filter name=kiln-lan- --format '{{.Names}}\t{{.Status}}\t{{.Command}}'
    ;;
*)
    echo "usage: $0 up|down|status" >&2
    exit 2
    ;;
esac
