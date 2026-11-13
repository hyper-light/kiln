"""The controller: one `kiln run` per host, all at once, each streaming its agent's records back.

`kiln run --on HOST --needs NEEDS` syncs this checkout to the host, holds the run to the room it
asks for (KILN_NEEDS, which the agent takes as its room), and streams the agent's stdout back as
it is written; its processes live until the run ends, so kiln's between-turns sweep leaves them
(they are the run's own). Records from every host go into one records.jsonl; anything else a host
prints goes to logs/<host>.out. When every host has ended, the report is written.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
import threading
import time
from typing import Dict, List

from . import records, report, witness

HOST_KEYS = {"on", "needs", "shards", "label", "python", "image", "ballast", "chaos", "rounds", "deadline_s", "seed", "driver", "hex", "vm", "kind", "asks"}
PLAN_KEYS = {"hosts", "image", "ballast", "chaos", "rounds", "deadline_s", "seed", "kiln", "driver", "vm", "kind", "asks"}


def load_plan(path: str) -> dict:
    with open(path, encoding="utf-8") as f:
        plan = json.load(f)
    if not isinstance(plan, dict) or not isinstance(plan.get("hosts"), list) or not plan["hosts"]:
        raise ValueError("a plan names its hosts: {\"hosts\": [{\"on\": …, \"needs\": …, \"shards\": …}], …}")
    unknown = set(plan) - PLAN_KEYS
    if unknown:
        raise ValueError(f"unknown plan keys {sorted(unknown)}")
    for h in plan["hosts"]:
        missing = {"on", "needs", "shards"} - set(h)
        if missing:
            raise ValueError(f"host {h} lacks {sorted(missing)}")
        if set(h) - HOST_KEYS:
            raise ValueError(f"host {h.get('on')}: unknown keys {sorted(set(h) - HOST_KEYS)}")
        if "mem=" not in h["needs"]:
            raise ValueError(f"host {h['on']}: its needs give no mem= (the room the agent derives its limit from)")
    for key in ("image", "deadline_s", "seed"):
        if all(key not in h for h in plan["hosts"]) and key not in plan:
            raise ValueError(f"the plan gives no {key}")
    return plan


def host_command(plan: dict, host: dict, run_id: str, now_ns: int) -> List[str]:
    get = lambda key, default=None: host.get(key, plan.get(key, default))  # noqa: E731
    label = host.get("label") or host["on"]
    agent = [
        get("python", "python3"), "bench/fleet/fleet.py", "host",
        "--out", f"$HOME/.cache/kiln-fleet/{run_id}/{label}",
        "--label", label, "--shards", host["shards"], "--image", get("image"),
        "--ballast", str(get("ballast", 0)), "--chaos", get("chaos", "none"), "--rounds", str(get("rounds", 0)),
        "--deadline-s", str(get("deadline_s")), "--seed", str(get("seed")), "--controller-t", str(now_ns), "--echo",
    ]
    if get("driver", "shards") == "hex":
        agent += ["--driver", "hex", "--hex", host.get("hex", ""), "--vm", get("vm", "")]
    elif get("driver", "shards") == "kiln":
        agent += ["--driver", "kiln", "--kind", get("kind", "claude"), "--asks", str(get("asks", 0))]
    # `$HOME` is the remote shell's: everything else is quoted as given.
    text = " ".join(a if a.startswith("$HOME/") else shlex.quote(a) for a in agent)
    text = text.replace("$HOME/", '"$HOME"/')
    kiln = plan.get("kiln", "kiln")
    return [kiln, "run", "--on", host["on"], "--needs", host["needs"], "-q", "--", text]


def run(plan_path: str, out: str, repo: str) -> int:
    plan = load_plan(plan_path)
    run_id = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + f"-{os.getpid()}"
    os.makedirs(os.path.join(out, "logs"), exist_ok=True)
    rec_path = os.path.join(out, "records.jsonl")
    lock = threading.Lock()
    sink = open(rec_path, "a", encoding="utf-8")
    with open(os.path.join(out, "plan.json"), "w", encoding="utf-8") as f:
        json.dump({"run": run_id, "plan": plan}, f, indent=1, sort_keys=True)
    procs: Dict[str, subprocess.Popen] = {}
    threads: List[threading.Thread] = []
    for host in plan["hosts"]:
        label = host.get("label") or host["on"]
        cmd = host_command(plan, host, run_id, time.time_ns())
        with open(os.path.join(out, "logs", f"{label}.cmd"), "w", encoding="utf-8") as f:
            f.write(" ".join(shlex.quote(c) for c in cmd) + "\n")
        p = subprocess.Popen(cmd, cwd=repo, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace", bufsize=1)
        procs[label] = p

        def pump(label: str = label, p: subprocess.Popen = p) -> None:
            other = open(os.path.join(out, "logs", f"{label}.out"), "a", encoding="utf-8")
            assert_stdout = p.stdout
            if assert_stdout is None:
                return
            for line in assert_stdout:
                text = line.strip()
                if text.startswith("{") and '"kind"' in text:
                    with lock:
                        sink.write(text + "\n")
                        sink.flush()
                else:
                    other.write(line)
                    other.flush()
            other.close()

        t = threading.Thread(target=pump, name=f"pump-{label}", daemon=True)
        t.start()
        threads.append(t)
    codes = {label: p.wait() for label, p in procs.items()}
    for t in threads:
        t.join()
    sink.close()
    all_records = records.read_all(rec_path)
    last_live: Dict[str, List[str]] = {}
    for r in all_records:
        if r.get("kind") == "scan" and r.get("quiescent"):
            last_live[str(r.get("host"))] = list(r.get("live") or [])
    cross = witness.across_hosts(last_live)
    text = report.render(all_records, cross)
    text += "== exits: " + ", ".join(f"{k} {v}" for k, v in sorted(codes.items())) + "\n"
    with open(os.path.join(out, "summary.txt"), "w", encoding="utf-8") as f:
        f.write(text)
    sys.stdout.write(text)
    return 0 if all(c == 0 for c in codes.values()) and not cross else 3
