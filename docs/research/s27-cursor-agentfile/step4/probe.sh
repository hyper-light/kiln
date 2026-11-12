# Step 4's probe (plan 01a1ce705e5c7504b88732f443d99288; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27): this
# text is a test-only harness.json's run.command, `sh -c` it, with the real harness.json's run.env as it is. It runs
# as Cursor's harness would, the project given at /workspace and HOMEVOL at /home/cursor with -v. No shards command
# reads a harness's output, so it writes what it sees into the project, as probe.txt, then `done`. Its model is step
# 19's probe of Claude Code (kiln main, docs/research/s19-claude-agentfile/step5/probe.sh). Every cursor-agent runs
# with --disable-auto-update, as kiln's harness.json starts it.
P=/workspace/probe.txt
exec >"$P" 2>&1
echo "== cursor-agent --disable-auto-update --version"; cursor-agent --disable-auto-update --version; echo "exit $?"
echo "== cursor-agent --disable-auto-update status --format json"; cursor-agent --disable-auto-update status --format json; echo "exit $?"
echo "== id -u"; id -u
echo "== id"; id
echo "== HOME"; echo "HOME=$HOME"
echo "== env: the names harness.json's run.env and init give"
env | grep -E '^(HOME|PATH|USER|LOGNAME|XDG_[A-Z_]+|NO_OPEN_BROWSER|CURSOR_[A-Z_]+|AGENT_CLI_[A-Z_]+|NODE_[A-Z_]+|SHARDS_DOMAIN)=' | sort
echo "== name lookup: getent ahosts api2.cursor.sh"; getent ahosts api2.cursor.sh; echo "exit $?"
echo "== acp: one initialize request, then stopped by signal (acp does not exit at EOF)"
printf '%s\n' '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}' >/tmp/acp.in
{ cat /tmp/acp.in; sleep 20; } | timeout -s TERM 40 cursor-agent --disable-auto-update acp >/workspace/acp.out 2>/workspace/acp.err
echo "exit $? (124: timeout's TERM stopped it)"
echo "acp stdout: $(head -c 3000 /workspace/acp.out)"
echo "acp stderr: $(head -c 800 /workspace/acp.err)"
echo '== touch "$HOME/x"'; touch "$HOME/x"; echo "exit $?"
echo "== git config --global user.name probe"; git config --global user.name probe; echo "exit $?"
echo "== git config --global --get user.name"; git config --global --get user.name; echo "exit $?"
echo "== cursor-agent --disable-auto-update sandbox run true (an observation, not a gate)"
timeout 60 cursor-agent --disable-auto-update sandbox run true; echo "exit $?"
echo "== ls -la \$HOME and \$HOME/.cursor"; ls -la "$HOME" "$HOME/.cursor" 2>&1
echo "== the compile cache: find /tmp/.cache -maxdepth 2"; find /tmp/.cache -maxdepth 2 2>&1 | head -20
echo "== find \$HOME -name '*compile-cache*'"; find "$HOME" -name '*compile-cache*' -print; echo "exit $?"
echo "== HTTPS by name: curl https://api2.cursor.sh/"
curl -sS -o /dev/null -w 'http_code=%{http_code} remote=%{remote_ip}:%{remote_port}\n' --connect-timeout 15 --max-time 30 https://api2.cursor.sh/; echo "exit $?"
echo "== TCP port 80 by name: api2.cursor.sh:80"
timeout 15 bash -c 'exec 3<>/dev/tcp/api2.cursor.sh/80' && echo "port 80 OPEN" || echo "port 80 closed (exit $?)"
echo "== TCP port 80 by address: 1.1.1.1:80"
timeout 15 bash -c 'exec 3<>/dev/tcp/1.1.1.1/80' && echo "port 80 OPEN" || echo "port 80 closed (exit $?)"
echo "== end"
echo done >/workspace/done
