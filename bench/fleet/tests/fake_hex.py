"""A stand-in `hex` for the hex driver's light tests: `node` and `schedule serve` say `ready
<socket>` and wait to be ended; `schedule call` answers Submit, Status and Delete as Hex's
docs/scheduler-live.md says, its boxes kept in a file beside the scheduler's socket, each box's run
host a process of its own whose arguments say `api-run` and name the scope (a box whose command is
`true` ends at once). FAKE_HEX_REFUSE, where set, is the refusal each Submit after the first
FAKE_HEX_REFUSE_AFTER (default 0) gets (its bytes, as Hex's codec carries a refusal)."""

import json
import os
import signal
import subprocess
import sys
import time


def wait_forever(socket):
    print(f"ready {socket}", flush=True)
    while True:
        time.sleep(3600)


def alive(pid):
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    try:
        done, _ = os.waitpid(pid, os.WNOHANG)
        return done == 0
    except ChildProcessError:
        return True


def call(socket, request_path):
    scope = os.path.dirname(socket)
    state_path = os.path.join(scope, "fake-hex.json")
    state = json.load(open(state_path)) if os.path.exists(state_path) else {"seq": 0, "submits": 0, "boxes": {}}
    request = json.load(open(request_path))
    reply = {"key": None, "at": 0, "tracked": False, "physical": None, "created": False, "ended": False, "offer": None, "observations": [], "refusals": []}
    if "Submit" in request:
        refuse = os.environ.get("FAKE_HEX_REFUSE")
        state["submits"] += 1
        if refuse and state["submits"] > int(os.environ.get("FAKE_HEX_REFUSE_AFTER", "0")):
            reply["refusals"] = [list(refuse.encode())]
        else:
            state["seq"] += 1
            key = {"owner": 0, "epoch": 1, "seq": state["seq"]}
            command = request["Submit"]["payload"]["spec"]["class"]["shardsimage"].get("command")
            ends = ["--exit"] if command == ["true"] else []
            host = subprocess.Popen([sys.executable, os.path.abspath(__file__), "api-run", "--scope", scope] + ends, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            state["boxes"][str(state["seq"])] = {"pid": host.pid, "deleted": False, "id": request["Submit"]["payload"]["spec"]["id"]}
            reply.update(key=key, tracked=True, created=True)
    else:
        verb = "Status" if "Status" in request else "Delete"
        key = request[verb]["key"]
        box = state["boxes"].get(str(key["seq"]))
        reply["key"] = key
        if box is not None and not box["deleted"]:
            if verb == "Delete":
                try:
                    os.kill(box["pid"], signal.SIGKILL)
                except OSError:
                    pass
                box["deleted"] = True
            else:
                running = alive(box["pid"])
                reply.update(tracked=True, created=True, ended=not running)
    json.dump(state, open(state_path, "w"))
    print(json.dumps(reply))


def main(argv):
    if argv[:1] == ["node"]:
        wait_forever(argv[argv.index("--socket") + 1])
    elif argv[:2] == ["schedule", "serve"]:
        config = json.load(open(argv[argv.index("--config") + 1]))
        wait_forever(config["socket"])
    elif argv[:2] == ["schedule", "call"]:
        call(argv[argv.index("--socket") + 1], argv[argv.index("--request") + 1])
    elif argv[:1] == ["api-run"]:
        while "--exit" not in argv:
            time.sleep(3600)
    else:
        sys.stderr.write(f"fake hex: {argv}\n")
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
