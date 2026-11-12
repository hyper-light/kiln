#!/bin/bash
# Step 27 research, live half (Linux x86_64): run Cursor CLI 2026.11.11-b912975's x64 package
# as a harness would, with no network and no credential, in empty directories, and record what
# it prints and what it writes. Big files go under ~/.cache (never /tmp on kiln's Linux runners).
set -u
VER=2026.11.11-b912975
WANT=be01f42ad334f81d05a9992fcf060a52cc746f52cc451e849eb652146a6e3c91
D="$HOME/.cache/kiln-step27-live/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$D/dl" && cd "$D" || exit 1
echo "DIR $D"; uname -srm; ldd --version 2>&1 | head -1
U="https://downloads.cursor.com/lab/$VER/linux/x64/agent-cli-package.tar.gz"
curl -fsSL -o dl/pkg.tar.gz "$U" || exit 1
curl -fsSL -o dl/pkg.sha256 "$U.sha256" || exit 1
GOT=$(sha256sum dl/pkg.tar.gz | cut -d' ' -f1); SIDE=$(cut -d' ' -f1 dl/pkg.sha256)
echo "sha256 $GOT sidecar $SIDE pinned $WANT"
[ "$GOT" = "$WANT" ] && [ "$SIDE" = "$WANT" ] || { echo "DIGEST MISMATCH"; exit 1; }
mkdir -p h && tar --strip-components=1 -xzf dl/pkg.tar.gz -C h
ls h | grep -v -E '^[0-9]+\.index\.js$' | tr '\n' ' '; echo
echo "unshare -rn: $(unshare -rn true 2>&1 && echo ok)"

run() { # run NAME ENV... -- ARGS...: no network, empty HOME and config dirs, as a harness gets
  local name=$1; shift
  local envs=(); while [ "$1" != "--" ]; do envs+=("$1"); shift; done; shift
  local home="$D/$name/home"; mkdir -p "$home" "$D/$name/cfg"
  echo "--- $name: cursor-agent $*"
  ( cd "$D/$name" && env -i PATH="$D/h:/usr/bin:/bin" HOME="$home" "${envs[@]}" \
      timeout 60 unshare -rn "$D/h/cursor-agent" "$@" < "${STDIN:-/dev/null}" > out 2> err; echo "exit $?" )
  printf 'stdout: '; head -c 1200 "$D/$name/out" | od -c | head -3 | sed 's/^/  /'
  echo "stdout text: $(head -c 1500 "$D/$name/out")"
  echo "stderr: $(head -c 600 "$D/$name/err")"
  echo "files written:"; find "$D/$name" -mindepth 1 -not -name out -not -name err -printf '  %M %s %P\n' | head -30
}

run version -- --version
run version-noupdate -- --disable-auto-update --version
run status XDG_CONFIG_HOME="$D/status/cfg" CURSOR_DATA_DIR="$D/status/cfg/cursor" NO_OPEN_BROWSER=1 -- status --format json

# The harness's own command, as harness.json will give it, answering ACP's initialize.
mkdir -p "$D/acp"
printf '%s\n' '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}' > "$D/acp/in.jsonl"
echo "--- acp: cursor-agent --disable-auto-update acp (initialize, then EOF after 15 s)"
( cd "$D/acp" && mkdir -p home cfg && { cat in.jsonl; sleep 15; } | env -i PATH="$D/h:/usr/bin:/bin" HOME="$D/acp/home" XDG_CONFIG_HOME="$D/acp/cfg" CURSOR_DATA_DIR="$D/acp/cfg/cursor" NO_OPEN_BROWSER=1 \
    timeout 40 unshare -rn "$D/h/cursor-agent" --disable-auto-update acp > out 2> err; echo "exit $?" )
echo "stdout: $(head -c 2500 "$D/acp/out")"
echo "stderr: $(head -c 800 "$D/acp/err")"
echo "files written:"; find "$D/acp" -mindepth 1 -not -name out -not -name err -not -name in.jsonl -printf '  %M %s %P\n' | head -40
echo "DONE"
