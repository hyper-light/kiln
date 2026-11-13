#!/usr/bin/env python3
"""An ACP harness's capabilities, by its initialize handshake alone (kiln plan
01a1d2c16eb074f19cf7ae7f1025c0be step 6, for 01a1cd8e862e7169a074db3d5fa33f3d step 35).

Starts PROGRAM ARGS..., sends one JSON-RPC `initialize` (the Agent Client Protocol's first request,
https://agentclientprotocol.com/protocol/initialization), prints the agent's answer as it gave it,
and ends the process. It sends no session/new and no prompt: nothing reaches a model, and no
account is read beyond what the agent itself reads to answer initialize.

    python3 handshake.py PROGRAM [ARGS...]

Exit 0 with the answer on stdout; 1 with what it said instead, when it gave no answer.
"""
import json
import subprocess
import sys
import threading

# kiln's ACP adapter's own initialize: its protocol version and client capabilities, as
# crates/kiln/src/harness/acp.rs (`initialize`, lines 469-472) sends them.
REQUEST = {
    "jsonrpc": "2.0",
    "id": 0,
    "method": "initialize",
    "params": {
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": False, "writeTextFile": False},
            "terminal": False,
            "session": {"configOptions": {"boolean": {}}, "compaction": {}},
            "_meta": {"opencode/child-session-updates": True},
        },
    },
}


def main():
    if len(sys.argv) < 2:
        sys.stderr.write(__doc__)
        return 2
    p = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    err = []
    threading.Thread(target=lambda: err.extend(p.stderr), daemon=True).start()
    answer = None
    try:
        p.stdin.write(json.dumps(REQUEST) + "\n")
        p.stdin.flush()
        timer = threading.Timer(60, p.kill)
        timer.start()
        for line in p.stdout:
            try:
                m = json.loads(line)
            except ValueError:
                continue
            if m.get("id") == 0 and ("result" in m or "error" in m):
                answer = m
                break
        timer.cancel()
    finally:
        p.kill()
        p.wait()
    if answer is None:
        sys.stderr.write("no initialize answer; it said on stderr:\n" + "".join(err)[-4000:])
        return 1
    print(json.dumps(answer, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
