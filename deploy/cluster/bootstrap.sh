#!/usr/bin/env bash
# Bring up the kiln dev cluster: multi-node minikube + ArgoCD + in-cluster git server +
# kube-prometheus-stack + kiln, all reconciled by ArgoCD (app-of-apps). Idempotent: re-run at
# any time to rebuild/reload the image, re-push the repo and re-apply the root Application.
#
#   deploy/cluster/bootstrap.sh                 # interactive (may prompt, e.g. to fix docker access)
#   deploy/cluster/bootstrap.sh --unattended    # never prompts; fails with instructions instead
#
# Environment overrides (flags win over env):
#   KILN_MINIKUBE_PROFILE (kiln)   KILN_NODES (3)   KILN_NODE_CPUS   KILN_NODE_MEMORY_MB
#   KILN_NODE_DISK (80g)   KILN_K8S_VERSION (stable)   KILN_REPO_URL   KILN_REPO_REVISION (main)
#   KILN_WAIT_TIMEOUT (1200 s)   KILN_IMAGE (kiln:dev)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ORIG_ARGS=("$@")

ARGOCD_CHART_REPO="https://argoproj.github.io/argo-helm"
ARGOCD_CHART_VERSION="10.10.2"     # ArgoCD v3.5.4
IN_CLUSTER_REPO_URL="http://kiln-git.kiln-git.svc.cluster.local/kiln.git"

PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
NODES="${KILN_NODES:-3}"
NODE_CPUS="${KILN_NODE_CPUS:-}"
NODE_MEMORY_MB="${KILN_NODE_MEMORY_MB:-}"
NODE_DISK="${KILN_NODE_DISK:-80g}"
K8S_VERSION="${KILN_K8S_VERSION:-stable}"
REPO_URL="${KILN_REPO_URL:-$IN_CLUSTER_REPO_URL}"
REPO_REVISION="${KILN_REPO_REVISION:-main}"
WAIT_TIMEOUT="${KILN_WAIT_TIMEOUT:-1200}"
KILN_IMAGE="${KILN_IMAGE:-kiln:dev}"
GIT_IMAGE="kiln-git:dev"
CRED_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/kiln/cluster"
CRED_FILE="$CRED_DIR/credentials.env"

UNATTENDED=0
SKIP_BUILD=0
NO_WAIT=0

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

  -y, --unattended      never prompt; generate all secrets; exit with instructions on problems
      --skip-build      do not run docker build (reuse existing $KILN_IMAGE / $GIT_IMAGE images)
      --no-wait         do not wait for the ArgoCD applications to become healthy
      --repo-url URL    git URL ArgoCD syncs from (default: in-cluster git server
                        $IN_CLUSTER_REPO_URL)
      --revision REV    branch/tag/commit to track (default: main)
      --nodes N         minikube nodes (default: 3)           [new clusters only]
      --cpus N          CPUs per node (default: derived)       [new clusters only]
      --memory MB       memory per node in MiB (default: derived) [new clusters only]
  -h, --help            show this help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
    -y | --unattended) UNATTENDED=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --no-wait) NO_WAIT=1 ;;
    --repo-url) REPO_URL="${2:?--repo-url needs a value}"; shift ;;
    --revision) REPO_REVISION="${2:?--revision needs a value}"; shift ;;
    --nodes) NODES="${2:?}"; shift ;;
    --cpus) NODE_CPUS="${2:?}"; shift ;;
    --memory) NODE_MEMORY_MB="${2:?}"; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
# No TTY → behave as unattended.
if [ ! -t 0 ] || [ ! -t 1 ]; then UNATTENDED=1; fi

# ------------------------------------------------------------------------------ helpers
c_blue=$'\033[1;34m' c_yellow=$'\033[1;33m' c_red=$'\033[1;31m' c_green=$'\033[1;32m' c_off=$'\033[0m'
log() { printf '%s==>%s %s\n' "$c_blue" "$c_off" "$*" >&2; }
ok() { printf '%s ✓%s %s\n' "$c_green" "$c_off" "$*" >&2; }
warn() { printf '%swarn:%s %s\n' "$c_yellow" "$c_off" "$*" >&2; }
die() { printf '%serror:%s %s\n' "$c_red" "$c_off" "$*" >&2; exit "${2:-1}"; }
kc() { kubectl --context "$PROFILE" "$@"; }

confirm() { # confirm "question" default(y|n)
    local q="$1" def="${2:-y}" ans
    [ "$UNATTENDED" = 1 ] && { [ "$def" = y ]; return; }
    read -r -p "$q [$([ "$def" = y ] && echo Y/n || echo y/N)] " ans || ans=""
    ans="${ans:-$def}"
    [[ "$ans" =~ ^[Yy] ]]
}

gen_secret() { # 48 hex chars from the kernel CSPRNG, no external deps, no SIGPIPE games
    od -An -N24 -tx1 /dev/urandom | tr -d ' \n'
}

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed${2:+ ($2)}"; }

# ------------------------------------------------------------------------- preflight
preflight_tools() {
    need docker "https://docs.docker.com/engine/install/"
    need minikube
    need kubectl
    need helm
    need git
    need od
}

reexec_with_docker_group() {
    # The user is in the docker group in /etc/group but this login session predates it.
    # `sg docker` (shadow) or `newgrp docker` (util-linux; Arch/CachyOS ship only this one) start
    # a process with the group active — no password needed for group members.
    if [ "${KILN_SG_REEXEC:-0}" = 1 ]; then
        return 1
    fi
    local launcher stdin_src=/dev/null rc=0
    [ "$UNATTENDED" = 0 ] && [ -r /dev/tty ] && stdin_src=/dev/tty
    launcher="$(mktemp "${TMPDIR:-/tmp}/kiln-bootstrap.XXXXXX")"
    {
        printf '#!/usr/bin/env bash\n'
        # shellcheck disable=SC2016 # $0 must expand inside the launcher, not here
        printf 'rm -f -- "$0"\n'
        printf 'export KILN_SG_REEXEC=1\n'
        printf 'exec %s\n' "$(printf '%q ' "${BASH:-bash}" "$0" "${ORIG_ARGS[@]}")"
    } >"$launcher"
    chmod 700 "$launcher"
    if command -v sg >/dev/null 2>&1; then
        log "re-running under 'sg docker' so the docker group applies to this session"
        sg docker -c "$launcher <$stdin_src" || rc=$?
    elif command -v newgrp >/dev/null 2>&1; then
        log "re-running under 'newgrp docker' so the docker group applies to this session"
        # newgrp starts the login shell (bash/zsh/fish) reading commands from stdin
        printf 'exec %s <%s\n' "$launcher" "$stdin_src" | newgrp docker || rc=$?
    else
        rm -f -- "$launcher"
        return 1
    fi
    exit "$rc"
}

check_docker() {
    local err user="${USER:-$(id -un)}"
    if err="$(docker info 2>&1 >/dev/null)"; then
        ok "docker is reachable"
        return 0
    fi
    if grep -qiE 'permission denied' <<<"$err"; then
        local in_db=0 in_session=0
        if getent group docker | cut -d: -f4 | tr ',' '\n' | grep -qx "$user"; then in_db=1; fi
        if id -nG | tr ' ' '\n' | grep -qx docker; then in_session=1; fi
        if [ "$in_db" = 1 ] && [ "$in_session" = 0 ]; then
            reexec_with_docker_group || true
            die "you were added to the 'docker' group but this login session does not have it yet.
       Fix (pick one):
         newgrp docker                      # in this terminal, then re-run this script
         sg docker -c '$0 ${ORIG_ARGS[*]}'
         log out and back in" 3
        fi
        local msg="your user '$user' cannot access the docker socket (not in the 'docker' group).
       Run ONCE (asks for your sudo password):
         sudo $SCRIPT_DIR/privileged-setup.sh
       or minimal:
         sudo usermod -aG docker $user && sudo systemctl enable --now docker
       then either run 'newgrp docker' or log out/in, and re-run:
         $0 ${ORIG_ARGS[*]}"
        if [ "$UNATTENDED" = 1 ]; then
            die "$msg" 3
        fi
        warn "$msg"
        if confirm "Run 'sudo $SCRIPT_DIR/privileged-setup.sh --user $user' now?" y; then
            sudo "$SCRIPT_DIR/privileged-setup.sh" --user "$user" --yes
            reexec_with_docker_group || die "group added; run 'newgrp docker' and re-run this script" 3
        fi
        die "docker access is required" 3
    fi
    if grep -qiE 'cannot connect|is the docker daemon running|no such file' <<<"$err"; then
        local msg="the docker daemon is not running.
       Start it (and enable on boot):
         sudo systemctl enable --now docker
       or run the one-time setup: sudo $SCRIPT_DIR/privileged-setup.sh"
        if [ "$UNATTENDED" = 1 ]; then die "$msg" 3; fi
        warn "$msg"
        if confirm "Run 'sudo systemctl enable --now docker' now?" y; then
            sudo systemctl enable --now docker
            docker info >/dev/null 2>&1 && { ok "docker is reachable"; return 0; }
            check_docker
            return
        fi
        die "docker daemon required" 3
    fi
    die "docker is not usable: $err" 3
}

derive_sizes() {
    local cpus mem_mb
    cpus="$(nproc)"
    mem_mb="$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo)"
    if [ -z "$NODE_CPUS" ]; then
        # 3/4 of the machine split across nodes (docker driver CPU is a cgroup quota, not a
        # reservation), at least 2, at most 16
        NODE_CPUS=$((cpus * 3 / 4 / NODES))
        [ "$NODE_CPUS" -lt 2 ] && NODE_CPUS=2
        [ "$NODE_CPUS" -gt 16 ] && NODE_CPUS=16
    fi
    if [ -z "$NODE_MEMORY_MB" ]; then
        # ~80% of RAM across nodes, capped at 16 GiB/node, at least 4 GiB
        NODE_MEMORY_MB=$((mem_mb * 8 / 10 / NODES))
        [ "$NODE_MEMORY_MB" -gt 16384 ] && NODE_MEMORY_MB=16384
        [ "$NODE_MEMORY_MB" -lt 4096 ] && NODE_MEMORY_MB=4096
    fi
    log "host: ${cpus} CPUs, ${mem_mb} MiB RAM → ${NODES} nodes × ${NODE_CPUS} CPUs × ${NODE_MEMORY_MB} MiB, disk ${NODE_DISK}"
}

# --------------------------------------------------------------------------- cluster
start_cluster() {
    local host
    host="$(minikube -p "$PROFILE" status --format '{{.Host}}' 2>/dev/null | head -n1 || true)"
    if [ "$host" = "Running" ]; then
        ok "minikube profile '$PROFILE' is running"
        local have
        have="$(minikube -p "$PROFILE" node list 2>/dev/null | wc -l)"
        while [ "$have" -lt "$NODES" ]; do
            log "adding node ($have/$NODES)"
            minikube -p "$PROFILE" node add --worker
            have=$((have + 1))
        done
    else
        if [ -n "$host" ]; then
            log "starting existing minikube profile '$PROFILE' (state: $host)"
        else
            derive_sizes
            if [ "$UNATTENDED" = 0 ] && ! confirm "Create minikube cluster '$PROFILE' with these sizes?" y; then
                die "aborted"
            fi
            log "creating minikube profile '$PROFILE'"
        fi
        minikube start -p "$PROFILE" \
            --driver=docker \
            --nodes="$NODES" \
            --cpus="${NODE_CPUS:-2}" \
            --memory="${NODE_MEMORY_MB:-4096}m" \
            --disk-size="$NODE_DISK" \
            --kubernetes-version="$K8S_VERSION" \
            --addons=metrics-server,storage-provisioner-rancher \
            --wait=all
    fi
    minikube -p "$PROFILE" addons enable metrics-server >/dev/null
    minikube -p "$PROFILE" addons enable storage-provisioner-rancher >/dev/null
    kc wait --for=condition=Ready nodes --all --timeout=300s >/dev/null
    # `standard` (hostpath) stays the default class; `local-path` is used explicitly for state
    # that must stay on one node (kiln data, git server, Prometheus TSDB).
    for _ in $(seq 1 30); do kc get storageclass local-path >/dev/null 2>&1 && break; sleep 2; done
    kc get storageclass local-path >/dev/null 2>&1 || die "storage class 'local-path' did not appear (storage-provisioner-rancher addon)"
    kc patch storageclass local-path -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"false"}}}' >/dev/null
    kc patch storageclass standard -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}' >/dev/null 2>&1 || true
    ok "cluster ready: $(kc get nodes --no-headers | wc -l) nodes"
}

build_images() {
    if [ "$SKIP_BUILD" = 1 ]; then
        log "skipping image builds (--skip-build)"
    else
        log "building $KILN_IMAGE (first build compiles the whole workspace; later builds use BuildKit caches)"
        DOCKER_BUILDKIT=1 docker build -t "$KILN_IMAGE" "$REPO_ROOT" ||
            die "docker build of $KILN_IMAGE failed (does 'cargo build --release -p kiln' work?)"
        if uses_in_cluster_git; then
            log "building $GIT_IMAGE"
            DOCKER_BUILDKIT=1 docker build -q -t "$GIT_IMAGE" "$REPO_ROOT/deploy/gitserver" >/dev/null
        fi
    fi
    local img
    for img in "$KILN_IMAGE" $(uses_in_cluster_git && echo "$GIT_IMAGE"); do
        docker image inspect "$img" >/dev/null 2>&1 || die "image $img does not exist locally (drop --skip-build)"
        log "loading $img into every node of '$PROFILE'"
        minikube -p "$PROFILE" image load --overwrite=true "$img"
    done
    ok "images loaded"
}

uses_in_cluster_git() { [ "$REPO_URL" = "$IN_CLUSTER_REPO_URL" ]; }

# ------------------------------------------------------------------------- secrets
ensure_ns() { kc create namespace "$1" --dry-run=client -o yaml | kc apply -f - >/dev/null; }

secret_value() { # ns name key
    kc -n "$1" get secret "$2" -o "jsonpath={.data.$3}" 2>/dev/null | base64 -d 2>/dev/null || true
}

ensure_secrets() {
    ensure_ns kiln
    ensure_ns monitoring
    KILN_TOKEN="$(secret_value kiln kiln-admin token)"
    if [ -z "$KILN_TOKEN" ]; then
        KILN_TOKEN="$(gen_secret)"
        kc -n kiln create secret generic kiln-admin --from-literal=token="$KILN_TOKEN" >/dev/null
        ok "generated kiln admin token (secret kiln/kiln-admin)"
    fi
    GRAFANA_PASSWORD="$(secret_value monitoring grafana-admin admin-password)"
    if [ -z "$GRAFANA_PASSWORD" ]; then
        GRAFANA_PASSWORD="$(gen_secret)"
        kc -n monitoring create secret generic grafana-admin \
            --from-literal=admin-user=admin --from-literal=admin-password="$GRAFANA_PASSWORD" >/dev/null
        ok "generated Grafana admin password (secret monitoring/grafana-admin)"
    fi
}

write_credentials() {
    local argocd_pw
    argocd_pw="$(secret_value argocd argocd-initial-admin-secret password)"
    mkdir -p "$CRED_DIR"
    chmod 700 "$CRED_DIR"
    (
        umask 077
        cat >"$CRED_FILE" <<EOF
# kiln dev cluster credentials (generated by deploy/cluster/bootstrap.sh) — keep private.
KILN_URL=http://localhost:7070
KILN_ADMIN_TOKEN=$KILN_TOKEN
GRAFANA_URL=http://localhost:3000
GRAFANA_ADMIN_USER=admin
GRAFANA_ADMIN_PASSWORD=$GRAFANA_PASSWORD
ARGOCD_URL=http://localhost:8080
ARGOCD_ADMIN_USER=admin
ARGOCD_ADMIN_PASSWORD=$argocd_pw
EOF
    )
    chmod 600 "$CRED_FILE"
}

# ----------------------------------------------------------------------- git server
deploy_git_server() {
    if ! uses_in_cluster_git; then
        log "repo URL is $REPO_URL — skipping the in-cluster git server"
        return
    fi
    log "deploying in-cluster git server (kiln-git)"
    kc apply -f "$REPO_ROOT/deploy/gitserver/kiln-git.yaml" >/dev/null
    kc -n kiln-git rollout restart deploy/kiln-git >/dev/null 2>&1 || true # pick up a reloaded image
    kc -n kiln-git rollout status deploy/kiln-git --timeout=300s
    KUBE_CONTEXT="$PROFILE" "$REPO_ROOT/deploy/gitserver/push.sh"
    ok "repo published at $IN_CLUSTER_REPO_URL"
}

# --------------------------------------------------------------------------- ArgoCD
install_argocd() {
    log "installing ArgoCD (argo-cd chart $ARGOCD_CHART_VERSION)"
    helm --kube-context "$PROFILE" upgrade --install argocd argo-cd \
        --repo "$ARGOCD_CHART_REPO" --version "$ARGOCD_CHART_VERSION" \
        --namespace argocd --create-namespace \
        -f "$REPO_ROOT/deploy/argocd/bootstrap/argocd-values.yaml" \
        --wait --timeout 10m >/dev/null
    ok "ArgoCD installed"
}

apply_root() {
    log "applying root Application (repo $REPO_URL @ $REPO_REVISION)"
    sed -e "s|__REPO_URL__|$REPO_URL|g" "$REPO_ROOT/deploy/argocd/bootstrap/repo-secret.yaml" | kc apply -f - >/dev/null
    sed -e "s|__REPO_URL__|$REPO_URL|g" -e "s|__TARGET_REVISION__|$REPO_REVISION|g" \
        "$REPO_ROOT/deploy/argocd/root.yaml" | kc apply -f - >/dev/null
    kc -n argocd annotate application kiln-root argocd.argoproj.io/refresh=hard --overwrite >/dev/null
}

app_state() { kc -n argocd get application "$1" -o 'jsonpath={.status.sync.status}/{.status.health.status}' 2>/dev/null || true; }

wait_apps() {
    [ "$NO_WAIT" = 1 ] && { log "not waiting for applications (--no-wait)"; return 0; }
    local deadline=$((SECONDS + WAIT_TIMEOUT)) app state failed=0
    for app in kiln-root kiln-namespaces kube-prometheus-stack kiln; do
        log "waiting for application $app to be Synced/Healthy"
        while :; do
            state="$(app_state "$app")"
            [ "$state" = "Synced/Healthy" ] && { ok "$app: $state"; break; }
            if [ $SECONDS -ge $deadline ]; then
                warn "$app not healthy after ${WAIT_TIMEOUT}s (state: ${state:-missing})"
                kc -n argocd get application "$app" -o 'jsonpath={range .status.conditions[*]}{.type}: {.message}{"\n"}{end}' 2>/dev/null || true
                failed=1
                break
            fi
            printf '   %s: %s\r' "$app" "${state:-pending}" >&2
            sleep 10
        done
    done
    return $failed
}

print_summary() {
    local ip
    ip="$(minikube -p "$PROFILE" ip 2>/dev/null || echo '<node-ip>')"
    cat <<EOF

${c_green}kiln dev cluster '$PROFILE' is up.${c_off}

Credentials (0600): $CRED_FILE
  or read them from the cluster:
    kubectl --context $PROFILE -n kiln get secret kiln-admin -o jsonpath='{.data.token}' | base64 -d
    kubectl --context $PROFILE -n monitoring get secret grafana-admin -o jsonpath='{.data.admin-password}' | base64 -d
    kubectl --context $PROFILE -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d

Open the UIs (all at once: $SCRIPT_DIR/port-forward.sh):
  kiln     kubectl --context $PROFILE -n kiln port-forward svc/kiln 7070:7070                          → http://localhost:7070
  ArgoCD   kubectl --context $PROFILE -n argocd port-forward svc/argocd-server 8080:80                → http://localhost:8080  (admin)
  Grafana  kubectl --context $PROFILE -n monitoring port-forward svc/kube-prometheus-stack-grafana 3000:80 → http://localhost:3000  (admin)
  Prometheus kubectl --context $PROFILE -n monitoring port-forward svc/kps-prometheus 9090:9090       → http://localhost:9090

Bare-metal agents (this host):     http://$ip:30070  +  QUIC $ip:30071/udp  (NodePort)
Bare-metal agents (other machines): $SCRIPT_DIR/expose-lan.sh up   → <this-host-LAN-IP>:7070/tcp + :7071/udp

Re-publish local changes to ArgoCD:  KUBE_CONTEXT=$PROFILE $REPO_ROOT/deploy/gitserver/push.sh
Rebuild + reload the image:          $0 --unattended
EOF
}

main() {
    cd "$REPO_ROOT"
    preflight_tools
    check_docker
    start_cluster
    build_images
    ensure_secrets
    deploy_git_server
    install_argocd
    apply_root
    # On re-runs: restart so the freshly loaded :dev image is used (the tag did not change).
    if [ "$SKIP_BUILD" = 0 ] && kc -n kiln get deploy kiln >/dev/null 2>&1; then
        kc -n kiln rollout restart deploy/kiln >/dev/null || true
    fi
    local rc=0
    wait_apps || rc=$?
    write_credentials
    print_summary
    if [ "$rc" != 0 ]; then
        warn "some applications are not healthy yet — inspect with: kubectl --context $PROFILE -n argocd get applications"
    fi
    return "$rc"
}

main
