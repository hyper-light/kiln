#!/bin/sh
# Install a kiln agent on this machine (Linux or macOS), fetched from the kiln server that served
# this script. Never asks for a password: installs into ~/.local (or /usr/local when run as root),
# puts that on your PATH (a line in your shell's startup file, when it isn't), and starts on boot
# via `kiln install --agent --unattended` (systemd, cron or XDG autostart on Linux; launchd on
# macOS). Windows: use install.ps1 instead.
#
#   curl -fsSL @@KILN_SERVER@@/install.sh | sh -s -- --token <join-token> [--labels baremetal,bench] [--name box1] [--slots 4]
#
# Without a token, `--join` asks the server to take this machine instead (`kiln join`): it prints a
# verification code, an admin approves the machine in the UI (Runners -> Add machines), and the
# agent is installed the same way.
#
#   curl -fsSL @@KILN_SERVER@@/install.sh | sh -s -- --join [--labels baremetal,bench] [--name box1]
#
# With `--outpost`, nothing is installed: kiln, its certificate and its files go in ~/.kiln-outpost,
# nothing starts on boot, and it runs in this terminal as an outpost until you stop it (Ctrl-C). Its
# terminals, AI sessions and agents show up in kiln; it takes no jobs. With a one-time invite
# (Machines -> Add an outpost) it joins at once; without one it asks to join, as `--join` does.
#
#   curl -fsSL @@KILN_SERVER@@/install.sh | sh -s -- --outpost [--token <invite>] [--name box1]
set -eu

SERVER="@@KILN_SERVER@@"
# The target the server itself was built for: its own binary is served when it fits this machine.
SERVER_TARGET="@@KILN_TARGET@@"
JOIN=0
OUTPOST=0
ARGS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="$2"; shift 2 ;;
    --join) JOIN=1; shift ;;
    --outpost) OUTPOST=1; shift ;;
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
if [ "$OS" = apple-darwin ] && [ "$ARCH" = x86_64 ]; then
  echo "kiln: Intel Macs aren't supported: kiln runs on Apple silicon Macs" >&2
  exit 1
fi
TARGET="$ARCH-$OS"

# Same machine type as the server (on Linux: same CPU, as before): the server's own binary.
# Anything else: the release build for this target (the server redirects to it).
URL="$SERVER/dl/kiln/$TARGET"
case "$SERVER_TARGET" in
  "$TARGET") URL="$SERVER/dl/kiln" ;;
  "$ARCH"-unknown-linux-*) if [ "$OS" = unknown-linux-musl ]; then URL="$SERVER/dl/kiln"; fi ;;
esac

# An outpost keeps kiln, its certificate and its files in a folder of its own, root or not.
if [ "$OUTPOST" = 1 ]; then BIN_DIR="${HOME}/.kiln-outpost"
elif [ "$(id -u)" = "0" ]; then BIN_DIR=/usr/local/bin; else BIN_DIR="${HOME}/.local/bin"; fi
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
if [ "$OUTPOST" = 1 ]; then
  echo "kiln ($TARGET) is in $BIN_DIR, not installed: it runs here as an outpost until you stop it (Ctrl-C)"
  # shellcheck disable=SC2086
  exec "$BIN_DIR/kiln" outpost --server "$SERVER" --state-dir "$BIN_DIR" $ARGS
fi
echo "kiln ($TARGET) installed to $BIN_DIR/kiln"
if [ "$JOIN" = 1 ]; then
  # shellcheck disable=SC2086
  exec "$BIN_DIR/kiln" join --server "$SERVER" $ARGS
fi
# shellcheck disable=SC2086
exec "$BIN_DIR/kiln" install --agent --unattended --server "$SERVER" $ARGS
