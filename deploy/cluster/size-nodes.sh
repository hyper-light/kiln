#!/usr/bin/env bash
# Tell Kubernetes each minikube node's real memory, and give each node a share of the CPUs to match,
# one node at a time.
#
#   deploy/cluster/size-nodes.sh            # every node of the profile
#   deploy/cluster/size-nodes.sh --dry-run  # say what would change, change nothing
#
# Under the docker driver a node reports the host's memory, though its container is capped
# (minikube's --memory, or a later `docker update --memory` for one node). bootstrap.sh reserves the
# difference in the kubelet (--system-reserved), but minikube passes every node's kubelet the same
# flag: a node resized on its own kept the others' reservation, and Kubernetes saw 77.5 GiB on nodes
# of 16, 48 and 16 GiB. It placed 16Gi jobs where they couldn't fit: killed (CI's rust job, from run
# #1522 on) or starving etcd (2026-10-29). Here each node's memory reservation comes from its own
# container's cap: its capacity less the cap, plus bootstrap.sh's headroom.
#
# CPUs aren't capped per node container, so bootstrap.sh leaves every node the profile's CPUs less
# its headroom (7.5 of 32 each, for 8). Here the same total goes to the nodes by their share of the
# nodes' memory (4.5, 13.5 and 4.5 for 16, 48 and 16 GiB), so the big jobs only the big node can take
# run side by side there. Without the profile's CPUs (not minikube; KILN_NODE_CPUS says them) the
# CPU part stays as it is.
#
# Each node's kubelet restarts (its pods keep running), and the next node waits until this one is
# Ready with its new allocatable memory and CPUs. A node whose pods ask for more memory or CPU than
# it would offer, or use more memory, stays as it is, saying so: its kubelet would turn them away as
# it restarts. minikube writes its own flag back on every `minikube start`, so bootstrap.sh and the
# autostart unit run this after it.
set -euo pipefail

PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
HEADROOM_KIB="${KILN_SYSTEM_HEADROOM_KIB:-1572864}"        # 1.5 GiB, as bootstrap.sh keeps
HEADROOM_MILLICPU="${KILN_SYSTEM_HEADROOM_MILLICPU:-500}" # as bootstrap.sh keeps
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
cpus() { awk -v m="$1" 'BEGIN { printf "%g CPUs", m / 1000 }'; }

# Kubernetes quantities: memory ("16Gi", "47636628Ki", "512M", bytes) in KiB, CPU ("32", "7500m",
# "4.5") in millicores.
QUANTITY='
import re
units = {"Ki": 2**10, "Mi": 2**20, "Gi": 2**30, "Ti": 2**40, "Pi": 2**50, "Ei": 2**60,
         "k": 10**3, "M": 10**6, "G": 10**9, "T": 10**12, "P": 10**15, "E": 10**18, "m": 10**-3, "": 1}
def amount(q):
    m = re.fullmatch(r"([0-9.]+(?:[eE][-+]?[0-9]+)?)(Ki|Mi|Gi|Ti|Pi|Ei|k|M|G|T|P|E|m)?", str(q).strip())
    if not m:
        raise ValueError("not a quantity: " + str(q))
    return float(m.group(1)) * units[m.group(2) or ""]
def kib(q): return int(amount(q) // 1024)
def milli(q): return int(round(amount(q) * 1000))
'
kib_of() { python3 -c "$QUANTITY"'
import sys; print(kib(sys.argv[1]))' "$1"; }
milli_of() { python3 -c "$QUANTITY"'
import sys; print(milli(sys.argv[1]))' "$1"; }

# What a node's pods ask for, "<memory KiB> <CPU millicores>", added up as the scheduler does: each
# running or pending pod's containers together (or its largest init container, if that's more) and
# its overhead.
asked() {
    kc get pods -A --field-selector "spec.nodeName=$1" -o json | python3 -c "$QUANTITY"'
import json, sys
def ask(c, r):
    return amount(((c.get("resources") or {}).get("requests") or {}).get(r, "0"))
mem = cpu = 0.0
for p in json.load(sys.stdin)["items"]:
    if p.get("status", {}).get("phase") in ("Succeeded", "Failed"):
        continue
    spec = p.get("spec", {})
    for r in ("memory", "cpu"):
        containers = sum(ask(c, r) for c in spec.get("containers", []))
        init = max([ask(c, r) for c in spec.get("initContainers", [])] or [0.0])
        total = max(containers, init) + amount((spec.get("overhead") or {}).get(r, "0"))
        if r == "memory":
            mem += total
        else:
            cpu += total
print(int(mem // 1024), int(round(cpu * 1000)))
'
}

# What the node's kubelet reserves as it runs (its configz), "<memory KiB> <CPU millicores>"; empty if
# unknown. Its unit can say otherwise: `minikube start` writes the shared flag back without
# restarting a kubelet.
running() {
    kc get --raw "/api/v1/nodes/$1/proxy/configz" 2>/dev/null | python3 -c "$QUANTITY"'
import json, sys
r = (json.load(sys.stdin).get("kubeletconfig") or {}).get("systemReserved") or {}
print(kib(r.get("memory", "0")), milli(r.get("cpu", "0")))
' 2>/dev/null || true
}

# What the node's pods use now, in KiB (cgroup v2 or v1, systemd or cgroupfs driver); empty if unknown.
used_kib() {
    local b
    b="$(docker exec "$1" sh -c 'for f in /sys/fs/cgroup/kubepods.slice/memory.current /sys/fs/cgroup/kubepods/memory.current /sys/fs/cgroup/memory/kubepods.slice/memory.usage_in_bytes /sys/fs/cgroup/memory/kubepods/memory.usage_in_bytes; do [ -r "$f" ] && { cat "$f"; exit 0; }; done; exit 1' 2>/dev/null)" || return 0
    echo $((b / 1024))
}

# The profile's CPUs per node, as minikube made them (MINIKUBE_HOME may name .minikube or its parent).
mk="${MINIKUBE_HOME:-$HOME}"
[ "$(basename "$mk")" = .minikube ] || mk="$mk/.minikube"
NODE_CPUS="${KILN_NODE_CPUS:-$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["CPUs"])' "$mk/profiles/$PROFILE/config.json" 2>/dev/null || true)}"

# Workers first: a node that goes wrong stops the run before the control plane's kubelet restarts.
nodes="$(kc get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}') $(kc get nodes -l 'node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}')"
[ -n "${nodes// /}" ] || die "no nodes in context $PROFILE"

# Every node's size first: each node's CPUs are its share of all their memory.
declare -A size
total_size=0
count=0
left=0
for n in $nodes; do
    if ! cap="$(docker inspect -f '{{.HostConfig.Memory}}' "$n" 2>/dev/null)"; then
        warn "$n: no docker container of that name (not the docker driver?): left as it is"
        left=1
        continue
    fi
    capacity="$(kib_of "$(kc get node "$n" -o jsonpath='{.status.capacity.memory}')")"
    size[$n]="$capacity"
    [ "$cap" -gt 0 ] && [ $((cap / 1024)) -lt "$capacity" ] && size[$n]=$((cap / 1024))
    total_size=$((total_size + size[$n]))
    count=$((count + 1))
done
budget=""
if [ -n "$NODE_CPUS" ] && [ "$count" -gt 0 ]; then
    budget=$((count * (NODE_CPUS * 1000 - HEADROOM_MILLICPU)))
else
    warn "the profile's CPUs per node are unknown (set KILN_NODE_CPUS): CPUs stay as they are"
fi

for n in $nodes; do
    [ -n "${size[$n]:-}" ] || continue
    capacity="$(kib_of "$(kc get node "$n" -o jsonpath='{.status.capacity.memory}')")"
    alloc="$(kib_of "$(kc get node "$n" -o jsonpath='{.status.allocatable.memory}')")"
    alloc_cpu="$(milli_of "$(kc get node "$n" -o jsonpath='{.status.allocatable.cpu}')")"
    unit="$(docker exec "$n" sh -c 'grep -l -e "--system-reserved=" /etc/systemd/system/kubelet.service.d/*.conf /etc/systemd/system/kubelet.service /lib/systemd/system/kubelet.service 2>/dev/null | head -n1' || true)"
    flag=""
    [ -n "$unit" ] && flag="$(docker exec "$n" grep -oE -e '--system-reserved=[^[:space:]"]+' "$unit" | head -n1 || true)"
    if [ -z "$flag" ]; then
        warn "$n: its kubelet unit has no --system-reserved flag (a cluster bootstrap.sh didn't make?): left as it is"
        left=1
        continue
    fi
    # The unit's flag: its memory and CPU parts, and the rest kept as they are.
    unit_mem=0
    unit_cpu=0
    others=""
    IFS=, read -r -a kv <<<"${flag#--system-reserved=}"
    for p in "${kv[@]}"; do
        case "$p" in
        memory=*) unit_mem="$(kib_of "${p#memory=}")" ;;
        cpu=*) unit_cpu="$(milli_of "${p#cpu=}")" ;;
        *) others="${others:+$others,}$p" ;;
        esac
    done
    read -r run_mem run_cpu <<<"$(running "$n")" || true
    if [ -z "${run_mem:-}" ]; then
        warn "$n: what its kubelet reserves as it runs is unknown (configz): taking its unit's flag"
        run_mem="$unit_mem"
        run_cpu="$unit_cpu"
    fi
    want_mem=$((capacity - size[$n] + HEADROOM_KIB))
    # Allocatable moves by what the running reservation moves (eviction thresholds and kube-reserved stay).
    want_alloc=$((alloc - (want_mem - run_mem)))
    want_cpu="$run_cpu"
    want_alloc_cpu="$alloc_cpu"
    if [ -n "$budget" ]; then
        want_alloc_cpu=$((budget * size[$n] / total_size / 100 * 100))
        want_cpu=$((run_cpu + alloc_cpu - want_alloc_cpu))
    fi
    if [ "$want_mem" -eq "$run_mem" ] && [ "$want_mem" -eq "$unit_mem" ] && [ "$want_cpu" -eq "$run_cpu" ] && { [ -z "$budget" ] || [ "$want_cpu" -eq "$unit_cpu" ]; }; then
        log "$n: allocatable $(gib "$alloc") and $(cpus "$alloc_cpu") already fit its $(gib "${size[$n]}") container"
        continue
    fi
    read -r asked_mem asked_cpu <<<"$(asked "$n")"
    used="$(used_kib "$n")"
    log "$n: container $(gib "${size[$n]}"); allocatable $(gib "$alloc") -> $(gib "$want_alloc"), $(cpus "$alloc_cpu") -> $(cpus "$want_alloc_cpu"); its pods ask $(gib "$asked_mem") and $(cpus "$asked_cpu")${used:+, use $(gib "$used")}"
    [ "$unit_mem" -ne "$run_mem" ] && log "$n: its unit says $(gib "$unit_mem") reserved, its kubelet runs with $(gib "$run_mem")"
    if [ "$want_alloc" -le 0 ] || [ "$want_alloc_cpu" -le 0 ] || [ "$asked_mem" -gt "$want_alloc" ] || [ "$asked_cpu" -gt "$want_alloc_cpu" ] || { [ -n "$used" ] && [ "$used" -gt "$want_alloc" ]; }; then
        warn "$n: its pods ask for or use more than it would offer: left as it is (run again once they're done)"
        left=1
        continue
    fi
    [ -n "$used" ] || warn "$n: what its pods use now is unknown (no kubepods cgroup found)"
    [ "$DRY_RUN" = 1 ] && continue
    cpu_part=""
    [ "$want_cpu" -gt 0 ] && cpu_part="cpu=${want_cpu}m,"
    new="--system-reserved=${cpu_part}memory=${want_mem}Ki${others:+,$others}"
    docker exec "$n" sed -i "s|$flag|$new|" "$unit"
    docker exec "$n" sh -c 'systemctl daemon-reload && systemctl restart kubelet'
    done_ok=""
    ready=""
    for _ in $(seq 1 90); do
        sleep 2
        now_mem="$(kc get node "$n" -o jsonpath='{.status.allocatable.memory}' 2>/dev/null || true)"
        now_cpu="$(kc get node "$n" -o jsonpath='{.status.allocatable.cpu}' 2>/dev/null || true)"
        ready="$(kc get node "$n" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || true)"
        if [ -n "$now_mem" ] && [ -n "$now_cpu" ] && [ "$ready" = True ] &&
            [ "$(kib_of "$now_mem")" -eq "$want_alloc" ] && [ "$(milli_of "$now_cpu")" -eq "$want_alloc_cpu" ]; then
            done_ok=1
            break
        fi
    done
    [ -n "$done_ok" ] || die "$n: not Ready with $(gib "$want_alloc") and $(cpus "$want_alloc_cpu") allocatable 3 minutes after its kubelet restarted (Ready: ${ready:-unknown}); the nodes after it are untouched"
    log "$n: Ready, allocatable $(gib "$want_alloc") and $(cpus "$want_alloc_cpu")"
done
exit "$left"
