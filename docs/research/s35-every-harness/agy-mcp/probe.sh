#!/usr/bin/env bash
# Plan 01a1d2c16eb074f19cf7ae7f1025c0be step 4: how the Antigravity CLI kiln's Agentfile pins (1.5.7,
# .kiln/images/harness-antigravity/pin.json) takes MCP servers, by starting it with no message (no turn,
# no prompt) under a throwaway HOME with no credentials, its servers pointed at a local listener.
set -u
CARRIER=$(pwd)
W=$HOME/g121-tmp/s35-agy; mkdir -p "$W"; cd "$W" || exit 9
URL=https://storage.googleapis.com/antigravity-public/antigravity-cli/1.5.7-6190022064472064/linux-x64-musl/cli_linux_x64_musl.tar.gz
TGZ_SHA=5000036342e1269ff91624a9eb49890613e8b88ab24de22081546f5ec625c654; BIN_SHA=bd643ae267d521b8febe5cd9fbce6533a1c7c0831aa0b28325401cb3e3b2035d
[ -f cli.tgz ] || curl -fsSL -o cli.tgz "$URL" || exit 9
echo "$TGZ_SHA  cli.tgz" | sha256sum -c - || exit 9
[ -f agy ] || { tar -xzf cli.tgz antigravity && mv antigravity agy && chmod 0755 agy; } || exit 9
echo "$BIN_SHA  agy" | sha256sum -c - || exit 9
A=$W/agy; export BROWSER=true
echo "== $(hostname) $(uname -sm) | agy $($A --version </dev/null 2>&1 | head -1) | $(date -u +%FT%TZ)"
rm -rf run; mkdir -p run/h1 run/h2 run/h3; R=$W/run
python3 -I "$CARRIER/listen.py" $R/port $R/seen.log & LP=$!; sleep 1; P=$(cat $R/port)
start() { # HOME, log, extra args...: agy as kiln starts it, its stdin open and empty for 15 s
  local home=$1 log=$2; shift 2; mkfifo $R/in-$log; (sleep 16 > $R/in-$log &)
  KILN_PROBE_TOKEN=probe-abc123 HOME=$home timeout 15 $A --input-format stream-json --output-format stream-json --log-file $R/$log.log "$@" < $R/in-$log > $R/$log.out 2>&1; echo "$log exit=$?"
}
echo "== 1: an http server in agy's own config, its header naming a variable of agy's environment"
HOME=$R/h1 $A mcp add --header 'Authorization: Bearer ${KILN_PROBE_TOKEN}' kiln-probe http://127.0.0.1:$P/config-mcp </dev/null 2>&1
cat $R/h1/.gemini/config/mcp_config.json; start $R/h1 p1
echo "== 2: an http server given in --settings alone"
start $R/h2 p2 --settings "{\"mcpServers\":{\"kiln-s\":{\"serverUrl\":\"http://127.0.0.1:$P/settings-mcp\",\"headers\":{\"Authorization\":\"Bearer \${KILN_PROBE_TOKEN}\"}}}}"
head -c 400 $R/p2.out; echo
echo "== 3: a stdio server in agy's own config: what of agy's environment reaches it"
HOME=$R/h3 $A mcp add kiln-stdio -- sh -c 'printf "%s\n" "token=${KILN_PROBE_TOKEN:-<unset>}" > "$HOME/stdio-env.txt"; exec cat > /dev/null' </dev/null 2>&1
cat $R/h3/.gemini/config/mcp_config.json; start $R/h3 p3
echo "stdio server saw: $(cat $R/h3/stdio-env.txt 2>/dev/null || echo '<never started>')"
sleep 1; kill $LP 2>/dev/null
echo "== what reached the listener"; cat $R/seen.log
echo "== p1 out"; head -c 600 $R/p1.out; echo
