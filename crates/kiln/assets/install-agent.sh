#!/bin/sh
# Install a kiln agent on this machine, fetched from the kiln server that served this script.
# Never asks for a password: installs into ~/.local (or /usr/local when run as root) and starts
# on boot via `kiln install --agent --unattended`.
#
#   curl -fsSL @@KILN_SERVER@@/install.sh | sh -s -- --token <join-token> [--labels baremetal,bench] [--name box1] [--slots 4]
set -eu

SERVER="@@KILN_SERVER@@"
ARGS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="$2"; shift 2 ;;
    *) ARGS="$ARGS $1"; shift ;;
  esac
done

if [ "$(id -u)" = "0" ]; then BIN_DIR=/usr/local/bin; else BIN_DIR="${HOME}/.local/bin"; fi
mkdir -p "$BIN_DIR"
TMP="$(mktemp "${BIN_DIR}/.kiln.XXXXXX")"
trap 'rm -f "$TMP"' EXIT
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$SERVER/dl/kiln" -o "$TMP"
else
  wget -qO "$TMP" "$SERVER/dl/kiln"
fi
chmod 0755 "$TMP"
mv -f "$TMP" "$BIN_DIR/kiln"
trap - EXIT
echo "kiln installed to $BIN_DIR/kiln"
# shellcheck disable=SC2086
exec "$BIN_DIR/kiln" install --agent --unattended --server "$SERVER" $ARGS
