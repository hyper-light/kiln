#!/bin/sh
# Install a kiln agent on this machine (Linux or macOS), fetched from the kiln server that served
# this script. Never asks for a password: installs into ~/.local (or /usr/local when run as root)
# and starts on boot via `kiln install --agent --unattended` (systemd, cron or XDG autostart on
# Linux; launchd on macOS). Windows: use install.ps1 instead.
#
#   curl -fsSL @@KILN_SERVER@@/install.sh | sh -s -- --token <join-token> [--labels baremetal,bench] [--name box1] [--slots 4]
set -eu

SERVER="@@KILN_SERVER@@"
# The target the server itself was built for: its own binary is served when it fits this machine.
SERVER_TARGET="@@KILN_TARGET@@"
ARGS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="$2"; shift 2 ;;
    *) ARGS="$ARGS $1"; shift ;;
  esac
done

case "$(uname -s)" in
  Linux) OS=unknown-linux-musl ;;
  Darwin) OS=apple-darwin ;;
  *) echo "kiln: unsupported system $(uname -s) (on Windows run install.ps1 in PowerShell)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) ARCH=x86_64 ;;
  aarch64 | arm64) ARCH=aarch64 ;;
  *) echo "kiln: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
# An x86_64 shell under Rosetta on Apple silicon still gets the native build.
if [ "$OS" = apple-darwin ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then ARCH=aarch64; fi
TARGET="$ARCH-$OS"

# Same machine type as the server (on Linux: same CPU, as before): the server's own binary.
# Anything else: the release build for this target (the server redirects to it).
URL="$SERVER/dl/kiln/$TARGET"
case "$SERVER_TARGET" in
  "$TARGET") URL="$SERVER/dl/kiln" ;;
  "$ARCH"-unknown-linux-*) if [ "$OS" = unknown-linux-musl ]; then URL="$SERVER/dl/kiln"; fi ;;
esac

if [ "$(id -u)" = "0" ]; then BIN_DIR=/usr/local/bin; else BIN_DIR="${HOME}/.local/bin"; fi
mkdir -p "$BIN_DIR"
TMP="$(mktemp "${BIN_DIR}/.kiln.XXXXXX")"
trap 'rm -f "$TMP"' EXIT
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "$TMP"
else
  wget -qO "$TMP" "$URL"
fi
chmod 0755 "$TMP"
mv -f "$TMP" "$BIN_DIR/kiln"
trap - EXIT
echo "kiln ($TARGET) installed to $BIN_DIR/kiln"
# shellcheck disable=SC2086
exec "$BIN_DIR/kiln" install --agent --unattended --server "$SERVER" $ARGS
