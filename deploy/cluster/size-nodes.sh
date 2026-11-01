#!/usr/bin/env bash
# Tell Kubernetes each minikube node's real memory, one node at a time.
#
#   deploy/cluster/size-nodes.sh            # every node of the profile
#   deploy/cluster/size-nodes.sh --dry-run  # say what would change, change nothing
#
# Under the docker driver a node reports the host's memory, though its container is capped
# (minikube's --memory, or a later `docker update --memory` for one node). bootstrap.sh reserves the
# difference in the kubelet (--system-reserved), but minikube passes every node's kubelet the same
# flag: a node resized on its own kept the others' reservation, and Kubernetes saw 77.5 GiB on nodes
# of 16, 48 and 16 GiB. It placed 16Gi jobs where they couldn't fit: killed (CI's rust job, from run
# #1522 on) or starving etcd (2026-10-29). Here each node's reservation comes from its own
# container's cap (its capacity less the cap, plus bootstrap.sh's headroom), minikube's CPU part
# kept. Its kubelet restarts (its pods keep running), and the next node waits until this one is
# Ready with its new allocatable memory. A node whose pods ask for or use more than its new size
# stays as it is, saying so: its kubelet would turn them away as it restarts. minikube writes its
# own flag back on every `minikube start`, so bootstrap.sh and the autostart unit run this after it.
set -euo pipefail

PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
HEADROOM_KIB="${KILN_SYSTEM_HEADROOM_KIB:-1572864}" # 1.5 GiB, as bootstrap.sh keeps
DRY_RUN=0
case "${1:-}" in
--dry-run) DRY_RUN=1 ;;
"") ;;
*) echo "usage: $(basename "$0") [--dry-run]" >&2; exit 2 ;;
esac

log() { printf '==> %s\n' "$*" >&2; }
warn() { printf 'warn: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
kc() { kubectl --context "$PROFILE" "$@"; }
gib() { awk -v k="$1" 'BEGIN { printf "%.1f GiB", k / 1048576 }'; }

# A Kubernetes memory quantity ("16Gi", "47636628Ki", "512M", bytes) in KiB.
kib_of() {
    python3 -c '
import re, sys
units = {"Ki": 2**10, "Mi": 2**20, "Gi": 2**30, "Ti": 2**40, "Pi": 2**50, "Ei": 2**60,
         "k": 10**3, "M": 10**6, "G": 10**9, "T": 10**12, "P": 10**15, "E": 10**18, "m": 10**-3, "": 1}
m = re.fullmatch(r"([0-9.]+(?:[eE][-+]?[0-9]+)?)(Ki|Mi|Gi|Ti|Pi|Ei|k|M|G|T|P|E|m)?", sys.argv[1].strip())
if not m:
    sys.exit("not a memory quantity: " + sys.argv[1])
print(int(float(m.group(1)) * units[m.group(2) or ""] // 1024))
' "$1"
}

# The memory a node's pods ask for, in KiB, added up as the scheduler does: each running or pending
# pod's containers together (or its largest init container, if that's more) and its overhead.
asked_kib() {
    kc get pods -A --field-selector "spec.nodeName=$1" -o json | python3 -c '
import json, re, sys
units = {"Ki": 2**10, "Mi": 2**20, "Gi": 2**30, "Ti": 2**40, "Pi": 2**50, "Ei": 2**60,
         "k": 10**3, "M": 10**6, "G": 10**9, "T": 10**12, "P": 10**15, "E": 10**18, "m": 10**-3, "": 1}
def size(q):
    m = re.fullmatch(r"([0-9.]+(?:[eE][-+]?[0-9]+)?)(Ki|Mi|Gi|Ti|Pi|Ei|k|M|G|T|P|E|m)?", str(q).strip())
    return float(m.group(1)) * units[m.group(2) or ""] if m else 0.0
def asked(c):
    return size(((c.get("resources") or {}).get("requests") or {}).get("memory", "0"))
total = 0.0
for p in json.load(sys.stdin)["items"]:
    if p.get("status", {}).get("phase") in ("Succeeded", "Failed"):
        continue
    spec = p.get("spec", {})
    containers = sum(asked(c) for c in spec.get("containers", []))
    init = max([asked(c) for c in spec.get("initContainers", [])] or [0.0])
    total += max(containers, init) + size((spec.get("overhead") or {}).get("memory", "0"))
print(int(total // 1024))
'
}

# The memory the node's kubelet reserves as it runs (its configz), in KiB; empty if unknown. Its unit
# can say otherwise: `minikube start` writes the shared flag back without restarting a kubelet.
running_kib() {
    local q
    q="$(kc get --raw "/api/v1/nodes/$1/proxy/configz" 2>/dev/null | python3 -c 'import json, sys; print(((json.load(sys.stdin).get("kubeletconfig") or {}).get("systemReserved") or {}).get("memory", "0"))' 2>/dev/null)" || return 0
    kib_of "$q"
}

# What the node's pods use now, in KiB (cgroup v2 or v1, systemd or cgroupfs driver); empty if unknown.
used_kib() {
    local b
    b="$(docker exec "$1" sh -c 'for f in /sys/fs/cgroup/kubepods.slice/memory.current /sys/fs/cgroup/kubepods/memory.current /sys/fs/cgroup/memory/kubepods.slice/memory.usage_in_bytes /sys/fs/cgroup/memory/kubepods/memory.usage_in_bytes; do [ -r "$f" ] && { cat "$f"; exit 0; }; done; exit 1' 2>/dev/null)" || return 0
    echo $((b / 1024))
}

# Workers first: a node that goes wrong stops the run before the control plane's kubelet restarts.
nodes="$(kc get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}') $(kc get nodes -l 'node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}')"
[ -n "${nodes// /}" ] || die "no nodes in context $PROFILE"
left=0
for n in $nodes; do
    if ! cap="$(docker inspect -f '{{.HostConfig.Memory}}' "$n" 2>/dev/null)"; then
        warn "$n: no docker container of that name (not the docker driver?): left as it is"
        left=1
        continue
    fi
    capacity="$(kib_of "$(kc get node "$n" -o jsonpath='{.status.capacity.memory}')")"
    alloc="$(kib_of "$(kc get node "$n" -o jsonpath='{.status.allocatable.memory}')")"
    size="$capacity"
    [ "$cap" -gt 0 ] && [ $((cap / 1024)) -lt "$capacity" ] && size=$((cap / 1024))
    unit="$(docker exec "$n" sh -c 'grep -l -e "--system-reserved=" /etc/systemd/system/kubelet.service.d/*.conf /etc/systemd/system/kubelet.service /lib/systemd/system/kubelet.service 2>/dev/null | head -n1' || true)"
    flag=""
    [ -n "$unit" ] && flag="$(docker exec "$n" grep -oE -e '--system-reserved=[^[:space:]"]+' "$unit" | head -n1 || true)"
    if [ -z "$flag" ]; then
        warn "$n: its kubelet unit has no --system-reserved flag (a cluster bootstrap.sh didn't make?): left as it is"
        left=1
        continue
    fi
    parts="${flag#--system-reserved=}"
    reserved=0
    others=""
    IFS=, read -r -a kv <<<"$parts"
    for p in "${kv[@]}"; do
        case "$p" in
        memory=*) reserved="$(kib_of "${p#memory=}")" ;;
        *) others="${others:+$others,}$p" ;;
        esac
    done
    running="$(running_kib "$n")"
    if [ -z "$running" ]; then
        warn "$n: what its kubelet reserves as it runs is unknown (configz): taking its unit's flag"
        running="$reserved"
    fi
    want=$((capacity - size + HEADROOM_KIB))
    # Allocatable moves by what the running reservation moves (eviction thresholds and kube-reserved stay).
    want_alloc=$((alloc - (want - running)))
    if [ "$want" -eq "$running" ] && [ "$want" -eq "$reserved" ]; then
        log "$n: allocatable $(gib "$alloc") already fits its $(gib "$size") container"
        continue
    fi
    asked="$(asked_kib "$n")"
    used="$(used_kib "$n")"
    unit_says=""
    [ "$reserved" -ne "$running" ] && unit_says=" (its unit says $(gib "$reserved"))"
    log "$n: container $(gib "$size"); reserved $(gib "$running")$unit_says -> $(gib "$want"); allocatable $(gib "$alloc") -> $(gib "$want_alloc"); its pods ask $(gib "$asked")${used:+, use $(gib "$used")}"
    if [ "$want_alloc" -le 0 ] || [ "$asked" -gt "$want_alloc" ] || { [ -n "$used" ] && [ "$used" -gt "$want_alloc" ]; }; then
        warn "$n: its pods ask for or use more than $(gib "$want_alloc"): left as it is (run again once they're done)"
        left=1
        continue
    fi
    [ -n "$used" ] || warn "$n: what its pods use now is unknown (no kubepods cgroup found)"
    [ "$DRY_RUN" = 1 ] && continue
    new="--system-reserved=${others:+$others,}memory=${want}Ki"
    docker exec "$n" sed -i "s|$flag|$new|" "$unit"
    docker exec "$n" sh -c 'systemctl daemon-reload && systemctl restart kubelet'
    ready=""
    for _ in $(seq 1 90); do
        sleep 2
        now="$(kc get node "$n" -o jsonpath='{.status.allocatable.memory}' 2>/dev/null || true)"
        ready="$(kc get node "$n" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || true)"
        [ -n "$now" ] && [ "$(kib_of "$now")" -eq "$want_alloc" ] && [ "$ready" = True ] && break
        now=""
    done
    [ -n "$now" ] || die "$n: not Ready with $(gib "$want_alloc") allocatable 3 minutes after its kubelet restarted (Ready: ${ready:-unknown}); the nodes after it are untouched"
    log "$n: Ready, allocatable $(gib "$want_alloc")"
done
exit "$left"
