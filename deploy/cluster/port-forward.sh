#!/usr/bin/env bash
# Forward kiln, ArgoCD, Grafana and Prometheus to localhost until Ctrl-C.
#   kiln http://localhost:7070  ArgoCD http://localhost:8080  Grafana http://localhost:3000  Prometheus http://localhost:9090
set -euo pipefail
PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

fwd() { # ns svc local:remote
    while :; do
        kubectl --context "$PROFILE" -n "$1" port-forward "svc/$2" "$3" >/dev/null 2>&1 || true
        sleep 2 # port-forward dies when the pod restarts; reconnect
    done &
    pids+=($!)
}

fwd kiln kiln 7070:7070
fwd argocd argocd-server 8080:80
fwd monitoring kube-prometheus-stack-grafana 3000:80
fwd monitoring kps-prometheus 9090:9090

cred="${XDG_STATE_HOME:-$HOME/.local/state}/kiln/cluster/credentials.env"
echo "kiln        http://localhost:7070"
echo "ArgoCD      http://localhost:8080   (admin)"
echo "Grafana     http://localhost:3000   (admin)"
echo "Prometheus  http://localhost:9090"
[ -f "$cred" ] && echo "credentials: $cred"
echo "Ctrl-C to stop."
wait
