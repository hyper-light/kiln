#!/bin/bash
# m4 (Linux x86_64 runner): opencode 2.2.7 linux-x64 (glibc) run as a Shards harness domain would run it:
# HOME a scratch dir, the XDG roots on a "volume" dir. --version and `debug paths` with no network at all
# (a new network namespace, loopback down); `opencode acp` driven with kiln's own initialize and session/new,
# with only loopback up, with loopback down, and with the host's network.
set -u
W=$HOME/.cache/s22-opencode-research
mkdir -p "$W/dl" && cd "$W/dl" || exit 1
V=2.2.7
echo "M4 run $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname) $(uname -srm) | $(. /etc/os-release; echo "$PRETTY_NAME") | $(ldd --version | head -1)"
f=opencode-linux-x64.tar.gz
[ -f "$f" ] || curl -fsSL -o "$f" "https://github.com/anomalyco/opencode/releases/download/v$V/$f"
echo "ARCHIVE $f sha256=$(sha256sum "$f" | cut -d' ' -f1)"
rm -rf gnu && mkdir gnu && tar -xzf "$f" -C gnu
OC=$W/dl/gnu/opencode
echo "BINARY sha256=$(sha256sum "$OC" | cut -d' ' -f1)"
VOL=$W/vol; T=$W/scratch; P=$W/project
rm -rf "$VOL" "$T" "$P"; mkdir -p "$VOL" "$T" "$P"
ENVS=(PATH=/usr/bin:/bin HOME=$T XDG_CONFIG_HOME=$VOL/.config XDG_DATA_HOME=$VOL/.local/share XDG_STATE_HOME=$VOL/.local/state XDG_CACHE_HOME=$VOL/.cache OPENCODE_DISABLE_AUTOUPDATE=1)
echo "--- version, no network (new netns, lo down)"
env -i "${ENVS[@]}" unshare -rn "$OC" --version; echo "VERSION_EXIT $?"
echo "--- files after --version"; find "$VOL" "$T" -mindepth 1 | sort | sed 's/^/  /'
echo "--- debug paths, no network"
env -i "${ENVS[@]}" unshare -rn "$OC" debug paths; echo "PATHS_EXIT $?"
cat > "$W/acp_driver.py" <<'PY'
import json, os, select, subprocess, sys, time
cmd = sys.argv[1:]
t0 = time.time()
p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
def send(o):
    p.stdin.write((json.dumps(o) + "\n").encode()); p.stdin.flush()
def recv(want, timeout):
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([p.stdout], [], [], max(0.0, end - time.time()))
        if not r:
            break
        line = p.stdout.readline()
        if not line:
            return None
        try:
            o = json.loads(line)
        except Exception:
            print("NONJSON", line[:200]); continue
        if o.get("id") == want:
            return o
        print("NOTIFY", json.dumps(o)[:300])
    return None
send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False, "session": {"configOptions": {"boolean": {}}, "compaction": {}}, "_meta": {"opencode/child-session-updates": True}}, "clientInfo": {"name": "kiln", "version": "probe"}}})
r = recv(1, 60)
print("INIT_MS", int((time.time() - t0) * 1000))
print("INIT", json.dumps(r)[:1500] if r else "NO RESPONSE")
if r and "result" in r:
    print("CHILDREN", subprocess.run(["ps", "--ppid", str(p.pid), "-o", "pid=,args="], capture_output=True, text=True).stdout.strip().replace("\n", " | ")[:400])
    print("LISTEN", subprocess.run(["ss", "-ltnpH"], capture_output=True, text=True).stdout.strip().replace("\n", " | ")[:600])
    send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": os.environ.get("PROBE_CWD", "/"), "mcpServers": []}})
    s = recv(2, 90)
    print("SESSION_MS", int((time.time() - t0) * 1000))
    print("SESSION", json.dumps(s)[:900] if s else "NO RESPONSE")
    print("ESTAB", subprocess.run(["ss", "-tnpH", "state", "established"], capture_output=True, text=True).stdout.strip().replace("\n", " | ")[:900])
p.stdin.close()
try:
    p.wait(timeout=30)
except subprocess.TimeoutExpired:
    p.kill(); print("KILLED after stdin closed")
print("EXIT", p.returncode)
print("STDERR", p.stderr.read().decode(errors="replace").strip()[-800:])
PY
echo "--- acp, only loopback up (as a Shards domain: its own loopback, brought up)"
env -i "${ENVS[@]}" PROBE_CWD="$P" unshare -rn sh -c 'ip link set lo up && exec python3 "$0" "$1" acp' "$W/acp_driver.py" "$OC"
echo "--- acp, loopback down"
env -i "${ENVS[@]}" PROBE_CWD="$P" unshare -rn python3 "$W/acp_driver.py" "$OC" acp
echo "--- acp, the host's network"
env -i "${ENVS[@]}" PROBE_CWD="$P" python3 "$W/acp_driver.py" "$OC" acp
echo "--- files after acp"; find "$VOL" "$T" -mindepth 1 -maxdepth 5 | sort | sed 's/^/  /' | head -80
echo "--- sizes"; du -sk "$VOL"/.config "$VOL"/.local/share "$VOL"/.local/state "$VOL"/.cache "$T" 2>/dev/null
echo "--- log lines naming the network"; grep -h -i -E 'models|fetch|http|update|ripgrep|download|npm|listen' "$VOL"/.local/share/opencode/log/*.log 2>/dev/null | cut -c1-240 | head -40
