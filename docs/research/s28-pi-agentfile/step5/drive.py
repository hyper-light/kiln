"""Drives pi through one prompt in its RPC mode, as kiln does (kiln plan
01a1ce705e62732b83f16295d74ee263, step 5; Kind `pi`'s arguments for kiln's read-only mode).

    python3 -I drive.py OUT_DIR CWD PI DEADLINE_S -- NAME=VALUE ...

Starts PI in CWD with exactly the environment given after `--`, sends one prompt, writes every
record pi prints to OUT_DIR/events.jsonl and its stderr to OUT_DIR/stderr.txt until agent_settled,
then closes stdin (pi's orderly shutdown) and waits for it to exit. DEADLINE_S bounds the whole run
so a run that never settles fails rather than hangs; it is the test's bound, not pi's. Prints one
line per tool run: its name, whether pi called it an error, and the first line of its result.
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time

out_dir, cwd, pi, deadline_s = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
sep = sys.argv.index("--")
env = dict(kv.split("=", 1) for kv in sys.argv[sep + 1 :])
session = "s5-" + os.path.basename(os.path.normpath(out_dir))
args = [pi, "--mode", "rpc", "--session-id", session, "--model", "mock/mock-1", "--tools", "read,grep,find,ls"]
os.makedirs(out_dir, exist_ok=True)

with open(os.path.join(out_dir, "stderr.txt"), "w", encoding="utf-8") as err:
    p = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
    lines = queue.Queue()

    def read():
        for line in p.stdout:
            lines.put(line)
        lines.put(None)

    threading.Thread(target=read, daemon=True).start()
    p.stdin.write(json.dumps({"id": "p1", "type": "prompt", "message": "Find the word needle in this folder, and its .txt files."}) + "\n")
    p.stdin.flush()
    end = time.monotonic() + deadline_s
    settled = False
    tools = []
    with open(os.path.join(out_dir, "events.jsonl"), "w", encoding="utf-8") as ev:
        while time.monotonic() < end:
            try:
                line = lines.get(timeout=max(0.0, end - time.monotonic()))
            except queue.Empty:
                break
            if line is None:
                break
            ev.write(line)
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if rec.get("type") == "tool_execution_end":
                text = " ".join(c.get("text", "") for c in (rec.get("result") or {}).get("content") or [] if isinstance(c, dict))
                tools.append((rec.get("toolName"), bool(rec.get("isError")), text.strip().splitlines()[0] if text.strip() else ""))
            if rec.get("type") == "agent_settled":
                settled = True
                break
    p.stdin.close()
    try:
        code = p.wait(timeout=max(1.0, end - time.monotonic()))
    except subprocess.TimeoutExpired:
        p.kill()
        code = p.wait()

with open(os.path.join(out_dir, "tools.json"), "w", encoding="utf-8") as f:
    json.dump([{"tool": name, "error": is_error, "first_line": first} for name, is_error, first in tools], f)
for name, is_error, first in tools:
    print(f"tool {name}: {'error' if is_error else 'ok'}: {first}")
print(f"settled: {settled}; pi exit: {code}")
sys.exit(0 if settled else 1)
