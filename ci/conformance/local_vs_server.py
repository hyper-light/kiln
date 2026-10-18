#!/usr/bin/env python3
"""docs/ci.md W17-AC04: GitHub's starter workflows with `kiln workflow run`, against the server.

Runs each starter workflow the server ran (ci/conformance/starters.py) again with `kiln workflow
run`, in the same repository at the same commit, on this machine, and compares every leg's and
every step's conclusion with the server's run of it. Writes docs/ci-conformance-local.md.

    python3 -I ci/conformance/local_vs_server.py --kiln target/debug/kiln --server http://127.0.0.1:17070 \\
        --token-file <data_dir>/admin.token --repo-dir <the starters work tree> --repo acme/starters

The work tree must be at the commit the server ran, unchanged, so both run the same commit.
"""

import argparse
import datetime
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
OUT = os.path.join(ROOT, "docs", "ci-conformance-local.md")

_spec = importlib.util.spec_from_file_location("starters", os.path.join(HERE, "starters.py"))
starters = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(starters)


def server_side(api, run):
    """The server's run: its conclusion and, by leg key, each leg's conclusion and steps."""
    d = api.call("GET", "/api/ci/runs/" + run["id"])
    legs = {}
    for leg in d.get("legs") or []:
        detail = api.call("GET", "/api/ci/legs/" + leg["id"])
        steps = sorted(detail.get("steps") or [], key=lambda s: (s.get("seq") or 1 << 60, s.get("idx") or 0))
        legs[leg["key"]] = {"status": leg.get("status"), "conclusion": leg.get("conclusion"), "steps": [(s.get("name"), s.get("conclusion")) for s in steps]}
    return d["run"].get("conclusion") or d["run"].get("status"), legs


def here_side(report):
    """The same from `kiln workflow run --report`."""
    runs = report.get("runs") or []
    if not runs:
        return None, {}
    r = runs[0]
    legs = {leg["key"]: {"status": leg.get("status"), "conclusion": leg.get("conclusion"), "steps": [(s.get("name"), s.get("conclusion")) for s in leg.get("steps") or []]} for leg in r.get("legs") or []}
    return r.get("conclusion") or r.get("status"), legs


def unrunnable(leg):
    """A leg no runner here could take: waiting on the server, cancelled with the run here."""
    return leg["status"] in ("queued", "pending") or (leg["conclusion"] == "cancelled" and not any(c for _, c in leg["steps"]))


def compare(server, here):
    """The differences between the server's legs and steps and these, and the legs fail-fast
    cancelled on one side only (which leg fails first is a race, on GitHub as here)."""
    out, raced = [], []
    for key in sorted(set(server) | set(here)):
        s, h = server.get(key), here.get(key)
        if s is None or h is None:
            out.append(f"`{key}` only {'here' if s is None else 'on the server'}")
            continue
        if unrunnable(s) and unrunnable(h):
            continue
        if (s["conclusion"] == "cancelled") != (h["conclusion"] == "cancelled"):
            raced.append(key)
            continue
        if s["conclusion"] == h["conclusion"] == "cancelled":
            continue  # how far each got before the cancel is timing too
        if s["conclusion"] != h["conclusion"]:
            out.append(f"`{key}`: {s['conclusion']} on the server, {h['conclusion']} here")
        # By name (and which of that name), not position: the server's older runs order a job
        # container's setup after the steps.
        def named(steps):
            seen, out = {}, {}
            for n, c in steps:
                seen[n] = seen.get(n, 0) + 1
                out[(n, seen[n])] = c
            return out
        sn, hn = named(s["steps"]), named(h["steps"])
        diff = [f"{n}: {sn.get((n, i))} / {hn.get((n, i))}" for (n, i) in sorted(set(sn) | set(hn)) if sn.get((n, i)) != hn.get((n, i))]
        if diff:
            out.append(f"`{key}` steps: " + "; ".join(diff))
    return out, raced


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--kiln", required=True, help="the kiln binary")
    ap.add_argument("--server", required=True)
    ap.add_argument("--token-file", required=True)
    ap.add_argument("--repo-dir", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--timeout", type=int, default=1800, help="seconds each run may take here")
    ap.add_argument("--only", nargs="*", help="only these starters (file names)")
    ap.add_argument("--merge", action="store_true", help="keep the report's other rows: only the workflows run now change")
    args = ap.parse_args()
    api = starters.Api(args.server, open(args.token_file).read().strip())
    kiln = os.path.abspath(args.kiln)
    sha = subprocess.run(["git", "-C", args.repo_dir, "rev-parse", "HEAD"], check=True, capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", args.repo_dir, "status", "--porcelain"], check=True, capture_output=True, text=True).stdout.strip()
    if dirty:
        sys.exit("the work tree has changes: both sides have to run the server's commit")
    listing = api.call("GET", "/api/ci/runs?" + urllib.parse.urlencode({"repo": args.repo, "limit": 200}))["runs"]
    on_server = {}
    for r in listing:
        wf = os.path.basename(r.get("workflow", ""))
        if r.get("sha") == sha and wf not in on_server:
            on_server[wf] = r
    names = sorted(n for n in on_server if not args.only or n in args.only)
    print(f"{len(names)} workflows the server ran at {sha[:8]}", flush=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith("KILN_")}
    results = []
    for n in names:
        run = on_server[n]
        event = run.get("event") or "push"
        cmd = [kiln, "workflow", "run", ".github/workflows/" + n, "--event", event]
        for k, v in (starters.DISPATCH.get(n) or {}).items():
            if event == "workflow_dispatch":
                cmd += ["--input", f"{k}={v}"]
        with tempfile.TemporaryDirectory() as tmp:
            report = os.path.join(tmp, "report.json")
            started = time.time()
            try:
                p = subprocess.run(cmd + ["--report", report], cwd=args.repo_dir, env=env, capture_output=True, text=True, timeout=args.timeout)
                tail = (p.stdout + p.stderr).strip().splitlines()[-3:]
            except subprocess.TimeoutExpired:
                tail = ["timed out"]
            took = int(time.time() - started)
            try:
                with open(report) as f:
                    rep = json.load(f)
            except (OSError, ValueError):
                rep = {}
        here_c, here_legs = here_side(rep)
        server_c, server_legs = server_side(api, run)
        diffs, raced = compare(server_legs, here_legs) if here_c else (["no report: " + " / ".join(tail)], [])
        rate = any("rate limit" in line.lower() for line in tail)
        results.append((n, server_c, here_c, diffs, took, rate, raced))
        print(f"  {n}: server {server_c}, here {here_c}, {'same' if not diffs else str(len(diffs)) + ' differences'} ({took}s)", flush=True)
    write(results, sha, args.merge)


def rows(results):
    out = {}
    for n, server_c, here_c, diffs, took, rate, raced in results:
        notes = "; ".join(diffs) if diffs else "none"
        if raced and not diffs:
            notes = "none; fail-fast cancelled " + ", ".join(f"`{k}`" for k in raced) + " on one side only (which leg fails first is a race)"
        if diffs and rate:
            notes = "**github.com's limit on anonymous API reads**; " + notes
        notes = notes.replace("|", "\\|")
        out[n] = (not diffs, f"| `{n}` | {server_c} | {here_c or 'no run'} ({took}s) | {notes} |")
    return out


def write(results, sha, merge=False):
    table = rows(results)
    if merge and os.path.exists(OUT):
        # The rows of workflows not run now stay as they were.
        with open(OUT) as f:
            for line in f:
                cells = line.split(" | ")
                if line.startswith("| `") and len(cells) == 4:
                    n = cells[0].strip("| `")
                    if n not in table:
                        table[n] = (cells[3].strip().rstrip("|").strip().startswith("none"), line.rstrip("\n"))
    same = sum(1 for ok, _ in table.values() if ok)
    lines = [
        "# CI conformance: `kiln workflow run` against the server",
        "",
        "Generated by `ci/conformance/local_vs_server.py` (docs/ci.md W17-AC04) on " + datetime.date.today().isoformat()
        + ": each starter workflow the server ran (`docs/ci-conformance.md`) run again with `kiln workflow run` on the same machine,"
        f" at the same commit (`{sha[:12]}`), and every leg's and step's conclusion compared with the server's run.",
        "",
        f"**{same} of {len(table)} give the same step results.**",
        "",
        "| Workflow | Server | `kiln workflow run` | Differences |",
        "| --- | --- | --- | --- |",
    ]
    lines += [table[n][1] for n in sorted(table)]
    with open(OUT, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"{same} of {len(table)} the same; wrote {os.path.relpath(OUT, ROOT)}")


if __name__ == "__main__":
    sys.exit(main())
