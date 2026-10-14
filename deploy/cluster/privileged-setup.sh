#!/usr/bin/env bash
# One-time host setup for kiln that needs root. Run ONCE:
#
#   sudo deploy/cluster/privileged-setup.sh                 # interactive
#   sudo -n deploy/cluster/privileged-setup.sh --yes        # non-interactive (or when run by root without a TTY)
#
# What it does (idempotent — safe to re-run):
#   1. adds the invoking user (SUDO_USER, or --user NAME) to the `docker` group
#   2. enables + starts docker (and containerd) on boot via systemd
#   3. raises inotify / file-descriptor / socket-buffer limits for many concurrent builds and QUIC
#      (/etc/sysctl.d/90-kiln.conf, /etc/security/limits.d/90-kiln.conf)
#   4. optionally installs a kiln binary to /usr/local/bin/kiln (--install-kiln [PATH])
#
# It never touches anything else. After step 1 the user must start a new login session
# (or run `newgrp docker`); deploy/cluster/bootstrap.sh does this automatically via `sg docker`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ORIG_ARGS=("$@")
TARGET_USER="${SUDO_USER:-}"
YES=0
INSTALL_KILN=""        # "" = ask (interactive) / skip (non-interactive); "skip"; or a path
SKIP_SYSCTL=0

usage() { sed -n '2,16p' "$0"; }

while [ $# -gt 0 ]; do
    case "$1" in
    --user) TARGET_USER="${2:?--user needs a name}"; shift ;;
    -y | --yes | --unattended) YES=1 ;;
    --install-kiln)
        if [ -n "${2:-}" ] && [ "${2#-}" = "$2" ]; then INSTALL_KILN="$2"; shift; else INSTALL_KILN="auto"; fi ;;
    --no-install-kiln) INSTALL_KILN="skip" ;;
    --skip-sysctl) SKIP_SYSCTL=1 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

if [ "$(id -u)" -ne 0 ]; then
    if [ -t 0 ] && [ "$YES" = 0 ]; then
        echo "re-running with sudo…" >&2
        exec sudo "$0" --user "$(id -un)" "${ORIG_ARGS[@]}"
    fi
    echo "error: must run as root: sudo $0 --yes" >&2
    exit 1
fi
# no TTY (e.g. `sudo -n` from a script) → non-interactive
[ -t 0 ] || YES=1

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok() { printf '\033[1;32m ✓\033[0m %s\n' "$*"; }
ask() { # ask "question" → 0 yes / 1 no ; non-interactive: default no
    [ "$YES" = 1 ] && return 1
    local a; read -r -p "$1 [y/N] " a || a=""; [[ "$a" =~ ^[Yy] ]]
}

# --------------------------------------------------------------------- 1. docker group
getent group docker >/dev/null || groupadd --system docker
if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = root ]; then
    say "no non-root user given (use --user NAME); skipping docker group membership"
elif id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx docker; then
    ok "$TARGET_USER is already in the docker group"
else
    usermod -aG docker "$TARGET_USER"
    ok "added $TARGET_USER to the docker group (takes effect in new sessions: 'newgrp docker' or re-login)"
fi

# ------------------------------------------------------------------ 2. docker on boot
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    for unit in containerd.service docker.service; do
        if systemctl list-unit-files "$unit" >/dev/null 2>&1 && systemctl cat "$unit" >/dev/null 2>&1; then
            systemctl enable --now "$unit" >/dev/null 2>&1 && ok "$unit enabled and running" ||
                echo "warn: could not enable $unit" >&2
        fi
    done
else
    say "systemd not detected: make sure dockerd starts on boot with your init system"
fi

# ------------------------------------------------------------------------ 3. limits
if [ "$SKIP_SYSCTL" = 0 ]; then
    cat >/etc/sysctl.d/90-kiln.conf <<'EOF'
# kiln: many concurrent builds (cargo/node/go watchers), a multi-node minikube, and QUIC.
fs.inotify.max_user_watches = 1048576
fs.inotify.max_user_instances = 8192
fs.inotify.max_queued_events = 65536
fs.file-max = 4194304
fs.nr_open = 4194304
vm.max_map_count = 1048576
kernel.pid_max = 4194304
kernel.keys.maxkeys = 20000
kernel.keys.maxbytes = 400000
# QUIC (UDP) socket buffers: let quinn use 8 MiB receive/send windows
net.core.rmem_max = 8388608
net.core.wmem_max = 8388608
net.core.netdev_max_backlog = 16384
net.core.somaxconn = 8192
EOF
    sysctl -q --system >/dev/null 2>&1 || sysctl -q -p /etc/sysctl.d/90-kiln.conf || true
    ok "sysctls applied (/etc/sysctl.d/90-kiln.conf)"

    cat >/etc/security/limits.d/90-kiln.conf <<'EOF'
# kiln: open-file limits for build-heavy users (new login sessions)
*    soft    nofile    1048576
*    hard    nofile    1048576
EOF
    ok "file-descriptor limits set (/etc/security/limits.d/90-kiln.conf, new sessions)"
fi

# ------------------------------------------------------------------ 4. host firewall
# Bare-metal agents on other machines (and containers) reach kiln on 7070/tcp (HTTP: UI, API,
# enrollment) and 7071/udp (QUIC agent transport). Open them only if a firewall is active.
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
    ufw allow 7070/tcp comment 'kiln http' >/dev/null && ufw allow 7071/udp comment 'kiln quic' >/dev/null &&
        ok "ufw: allowed 7070/tcp and 7071/udp (kiln)"
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --permanent --add-port=7070/tcp --add-port=7071/udp >/dev/null && firewall-cmd --reload >/dev/null &&
        ok "firewalld: allowed 7070/tcp and 7071/udp (kiln)"
else
    ok "no active host firewall to configure"
fi

# -------------------------------------------------------------- 5. kiln binary (opt.)
install_kiln() {
    local src="$1"
    [ -f "$src" ] || { echo "error: $src not found" >&2; return 1; }
    install -m 0755 -o root -g root "$src" /usr/local/bin/kiln
    ok "installed $(/usr/local/bin/kiln --version 2>/dev/null || echo kiln) → /usr/local/bin/kiln"
}
find_kiln() {
    local c
    for c in "$REPO_ROOT/target/x86_64-unknown-linux-musl/release/kiln" "$REPO_ROOT/target/release/kiln"; do
        [ -x "$c" ] && { echo "$c"; return 0; }
    done
    return 1
}
case "$INSTALL_KILN" in
skip) ;;
"")
    if src="$(find_kiln)" && ask "Install $src to /usr/local/bin/kiln?"; then install_kiln "$src"; fi ;;
auto)
    src="$(find_kiln)" || { echo "error: no built kiln binary under $REPO_ROOT/target (cargo build --release -p kiln)" >&2; exit 1; }
    install_kiln "$src" ;;
*) install_kiln "$INSTALL_KILN" ;;
esac

echo
ok "privileged setup complete."
if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != root ]; then
    echo "   Next (as $TARGET_USER, no sudo): newgrp docker   # or log out/in"
    echo "   then:                             $SCRIPT_DIR/bootstrap.sh"
fi
