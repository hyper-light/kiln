#!/usr/bin/env bash
# Start the kiln minikube cluster at boot (and the LAN forwarders, which docker restarts itself).
#
#   deploy/cluster/autostart.sh enable | disable | status
#
# Installs a systemd *user* unit (no root, no password). With lingering enabled the user manager
# starts at boot without anyone logging in; `enable` tries `loginctl enable-linger` itself
# (allowed for your own user by default polkit policy) and tells you if it could not.
# kiln agents and the server reconnect forever, so start order does not matter.
set -euo pipefail
PROFILE="${KILN_MINIKUBE_PROFILE:-kiln}"
UNIT="kiln-cluster.service"
DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
MINIKUBE="$(command -v minikube || echo /usr/bin/minikube)"

case "${1:-status}" in
enable)
    mkdir -p "$DIR"
    cat >"$DIR/$UNIT" <<EOF
[Unit]
Description=kiln minikube cluster ($PROFILE)
After=network-online.target docker.service
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
# docker may still be starting at boot: retry a few times.
ExecStart=/bin/sh -c 'for i in 1 2 3 4 5 6; do $MINIKUBE start -p $PROFILE && exit 0; sleep 20; done; exit 1'
ExecStop=$MINIKUBE stop -p $PROFILE
TimeoutStartSec=900

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable "$UNIT"
    if loginctl --no-ask-password enable-linger "$USER" 2>/dev/null; then
        echo "linger enabled: the cluster starts at boot without a login"
    else
        echo "could not enable linger without a password; the cluster starts at login."
        echo "for boot without login run once: sudo loginctl enable-linger $USER"
    fi
    echo "enabled $DIR/$UNIT"
    ;;
disable)
    systemctl --user disable "$UNIT" 2>/dev/null || true
    rm -f "$DIR/$UNIT"
    systemctl --user daemon-reload
    echo "disabled"
    ;;
status)
    systemctl --user status "$UNIT" --no-pager 2>/dev/null || echo "not installed"
    ;;
*)
    echo "usage: $0 enable|disable|status" >&2
    exit 2
    ;;
esac
