# Step 5's probe (plan 01a1cf48e8157494ac9a51f7a564f526): this text is a test-only harness.json's run.command,
# `sh -c` it, with the real harness.json's run.env as it is. It runs as Claude Code's harness would, the project
# given at /workspace and HOMEDIR at /home/claude with -v. No shards command reads a harness's output, so it writes
# what it sees into the project, as probe.txt, then `done`.
P=/workspace/probe.txt
exec >"$P" 2>&1
echo "== claude --version"; claude --version; echo "exit $?"
echo "== id -u"; id -u
echo "== id"; id
echo "== HOME"; echo "HOME=$HOME"
echo "== env: the names harness.json's run.env and init give"
env | grep -E '^(HOME|PATH|USER|LOGNAME|DISABLE_UPDATES|CLAUDE_[A-Z_]+|USE_BUILTIN_RIPGREP|SHARDS_DOMAIN)=' | sort
echo "== name lookup: getent ahosts api.anthropic.com"; getent ahosts api.anthropic.com; echo "exit $?"
echo '== touch "$HOME/x"'; touch "$HOME/x"; echo "exit $?"
echo "== git config --global user.name kiln-probe"; git config --global user.name kiln-probe; echo "exit $?"
echo "== git config --global --get user.name"; git config --global --get user.name; echo "exit $?"
echo "== claude mcp add --scope user kiln-probe -- true"; claude mcp add --scope user kiln-probe -- true; echo "exit $?"
echo "== ls -la \$HOME"; ls -la "$HOME"
echo "== find /tmp for x, .gitconfig, .claude.json and .claude"
find /tmp -maxdepth 4 \( -name x -o -name .gitconfig -o -name .claude.json -o -name .claude \) -print; echo "exit $?"
echo "== ls -la /tmp"; ls -la /tmp
echo "== HTTPS by name: curl https://api.anthropic.com/"
curl -sS -o /dev/null -w 'http_code=%{http_code} remote=%{remote_ip}:%{remote_port}\n' --connect-timeout 15 --max-time 30 https://api.anthropic.com/; echo "exit $?"
echo "== TCP port 80 by name: api.anthropic.com:80"
timeout 15 bash -c 'exec 3<>/dev/tcp/api.anthropic.com/80' && echo "port 80 OPEN" || echo "port 80 closed (exit $?)"
echo "== TCP port 80 by address: 1.1.1.1:80"
timeout 15 bash -c 'exec 3<>/dev/tcp/1.1.1.1/80' && echo "port 80 OPEN" || echo "port 80 closed (exit $?)"
echo "== HTTP on port 80: curl http://api.anthropic.com/"
curl -sS -o /dev/null -w 'http_code=%{http_code}\n' --connect-timeout 15 --max-time 20 http://api.anthropic.com/; echo "exit $?"
echo "== end"
echo done >/workspace/done
