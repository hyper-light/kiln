"""`fleet.py host | run | report`: the host agent, the controller and the report."""

from __future__ import annotations

import argparse
import os
import re
import socket
import subprocess
import sys
from typing import List

from . import controller, records, report
from .agent import Agent, needs_room
from .records import Records

LABEL = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
PINNED = re.compile(r"@sha(256|384|512):[0-9a-f]+$")


def repo_root() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    out = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=here, capture_output=True, text=True, check=False).stdout.strip()
    return out or here


def host_main(argv: List[str]) -> int:
    p = argparse.ArgumentParser(prog="fleet.py host", description="The host agent: fill this host with microVMs, measure them, put them through chaos, and leave nothing behind.")
    p.add_argument("--out", required=True, help="the run's directory on this host: records.jsonl, logs, and the scope (s/) it removes at the end")
    p.add_argument("--label", default=socket.gethostname().split(".")[0], help="this host's name in the records")
    p.add_argument("--shards", required=True, help="the shards binary under test")
    p.add_argument("--driver", choices=("shards", "hex", "kiln"), default="shards", help="who places the VMs: Shards' daemon (shards, the direct driver), a Hex scheduler of the run's own (hex), or kiln's balancer, as isolated agent sessions (kiln: KILN_URL and KILN_TOKEN in the environment)")
    p.add_argument("--kind", default="claude", help="the harness each kiln agent session runs (--driver kiln)")
    p.add_argument("--asks", type=int, default=0, help="how many sessions the run asks kiln for at once (--driver kiln: kiln's own admission places, queues or refuses them)")
    p.add_argument("--hex", default="", help="the hex binary under test (--driver hex)")
    p.add_argument("--vm", default="", help="each VM's size, as the run asks Hex to admit it: cpus=N,mem=SIZE (--driver hex; Hex sizes a VM by what it admits, never by Shards' default)")
    p.add_argument("--image", required=True, help="the workload's image, pinned by digest (NAME@sha256:...)")
    p.add_argument("--ballast", type=int, default=0, help="bytes the workload holds resident before it answers, from a recorded harness's footprint (README, the profile); 0: the stub's own")
    p.add_argument("--needs", default=os.environ.get("KILN_NEEDS", ""), help="the room the run is held to, as kiln run's --needs says it (default: KILN_NEEDS)")
    p.add_argument("--chaos", default="none", help="chaos actions, comma-separated: vm, daemon, client; none")
    p.add_argument("--rounds", type=int, default=0, help="how many times each chaos action is taken")
    p.add_argument("--deadline-s", type=float, required=True, help="the run's own deadline, the bound of the waits no answer has seeded yet")
    p.add_argument("--seed", type=int, required=True, help="the chaos targets' generator's seed, recorded")
    p.add_argument("--repo", default=repo_root(), help="the checkout whose revision is recorded")
    p.add_argument("--rev", default="", help="the revision to record where the harness was copied out of its checkout (`git archive`), which has none of its own")
    p.add_argument("--controller-t", type=int, default=0, help="the controller's clock when it started this agent (ns), for the skew")
    p.add_argument("--keep-home", action="store_true", help="keep the scope's Shards home after the run")
    p.add_argument("--echo", action="store_true", help="also write every record to stdout (for the controller)")
    o = p.parse_args(argv)
    if not LABEL.match(o.label):
        p.error(f"--label {o.label!r}: letters, digits, dots, dashes and underscores, at most 64")
    if not PINNED.search(o.image):
        p.error(f"--image {o.image!r} isn't pinned by a digest: a tag that moves between hosts or runs would measure another image")
    if o.deadline_s <= 0:
        p.error("--deadline-s must be positive")
    if o.driver == "hex":
        vm = needs_room(o.vm)
        if not o.hex:
            p.error("--driver hex needs --hex, the hex binary under test")
        if vm["cpu"] is None or vm["mem"] is None or vm["cpu"] <= 0 or vm["cpu"] % 1000 or vm["mem"] <= 0:
            p.error(f"--vm {o.vm!r}: whole CPUs and a memory size (cpus=1,mem=512m): a Shards VM's vCPUs are whole")
        o.hex = os.path.abspath(os.path.expanduser(o.hex))
    if o.driver == "kiln":
        if o.asks <= 0:
            p.error("--driver kiln needs --asks N, how many sessions the run asks kiln for")
        if not os.environ.get("KILN_URL") or not os.environ.get("KILN_TOKEN"):
            p.error("--driver kiln needs KILN_URL and KILN_TOKEN in the environment (the token is never recorded)")
    o.out = os.path.abspath(os.path.expanduser(o.out))
    o.shards = os.path.abspath(os.path.expanduser(o.shards))
    rec = Records(os.path.join(o.out, "records.jsonl"), o.label, os.path.basename(os.path.dirname(o.out)) or o.label, sys.stdout if o.echo else None)
    try:
        code = Agent(o, rec).run()
    finally:
        rec.close()
    with open(os.path.join(o.out, "summary.txt"), "w", encoding="utf-8") as f:
        f.write(report.render(records.read_all(os.path.join(o.out, "records.jsonl"))))
    return code


def run_main(argv: List[str]) -> int:
    p = argparse.ArgumentParser(prog="fleet.py run", description="The controller: an agent on every host of the plan at once, through kiln run.")
    p.add_argument("--plan", required=True, help="the plan (JSON): its hosts, each with on, needs and shards, and the run's knobs")
    p.add_argument("--out", required=True, help="where records.jsonl, summary.txt and logs/ go")
    o = p.parse_args(argv)
    return controller.run(o.plan, os.path.abspath(o.out), repo_root())


def report_main(argv: List[str]) -> int:
    p = argparse.ArgumentParser(prog="fleet.py report", description="The percentile report of records.jsonl files.")
    p.add_argument("records", nargs="+")
    o = p.parse_args(argv)
    recs = []
    for path in o.records:
        recs.extend(records.read_all(path))
    sys.stdout.write(report.render(recs))
    return 0


def main(argv: List[str]) -> int:
    commands = {"host": host_main, "run": run_main, "report": report_main}
    if not argv or argv[0] not in commands:
        sys.stderr.write("usage: fleet.py host|run|report ...  (fleet.py COMMAND -h for each)\n")
        return 2
    return commands[argv[0]](argv[1:])
