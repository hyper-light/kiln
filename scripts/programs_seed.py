#!/usr/bin/env python3
"""Seeds a kiln server, through its API, with a program like the critical path (CP-M3;
docs/programs.md, "Tests"): a master plan and ten track plans (tagged critical-path, t1..t10), the
master run as goals on a coordinator and 120 track goals lined up on its stack, six workers assigned
and each handed its track's first goal. Its agents should run a stub harness that exits at once
(scripts/programs_load.sh gives the server one), so they exist and run nothing.

    python3 scripts/programs_seed.py <port> <admin token file> <work folder>

Prints what it made, then the master plan's id (the program's) on its last line. The token is read
from its file and never printed.
"""

from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request

# The mock's tracks (ui/mock/programs.ts): names, weights and goals, 120 in all.
TRACKS = [(1, "Amphora", 5, 13), (2, "Quiver", 3, 13), (3, "Sends", 5, 7), (4, "Isolation", 4, 10), (5, "UIs", 2, 10), (6, "Quality", 3, 6), (7, "Hex", 4, 14), (8, "Veil", 3, 10), (9, "Reliquary", 3, 10), (10, "Mantle", 3, 27)]
WORKERS = (1, 3, 4, 6, 7, 10)


def main() -> None:
    port, token_file, work = sys.argv[1], sys.argv[2], sys.argv[3]
    with open(token_file, encoding="utf-8") as f:
        token = f.read().strip()
    base = f"http://127.0.0.1:{port}"

    def api(method: str, path: str, body: object = None) -> object:
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(base + path, method=method, data=data, headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                text = r.read().decode()
        except urllib.error.HTTPError as e:
            sys.exit(f"{method} {path}: {e.code} {e.read().decode()[:300]}")
        return json.loads(text) if text else None

    def agent(name: str) -> str:
        out = api("POST", "/api/harness/sessions", {"target": "local", "spec": {"kind": "claude", "name": name, "cwd": work}})
        first = out[0] if isinstance(out, list) else out
        return first["id"]

    coordinator = agent("Coordinator")
    workers = {n: agent(f"Worker T{n}") for n in WORKERS}
    table = "| Track | Weight |\n|---|---|\n" + "".join(f"| T{n} {name} | {w} |\n" for n, name, w, _ in TRACKS)
    steps = [{"title": "CP-M1 Vendoring from a manifest"}, {"title": "CP-M2 Seams"}, {"title": "CP-M3 The Programs view"}]
    master = api("POST", "/api/plans", {"title": "Critical path, master: a lab of ten tracks and 120 goals", "body": table, "items": steps, "tags": ["critical-path", "master"]})
    for n, name, _, _ in TRACKS:
        api("POST", "/api/plans", {"title": f"Critical path T{n}: {name}", "body": f"The {name} track.", "items": [], "tags": ["critical-path", f"t{n}"]})
    api("POST", f"/api/plans/{master['id']}/run", {"mode": "goals", "session": coordinator})
    # The tracks' goals: more than a plan's 100 steps, so lined up on the coordinator's stack itself
    # (as the real coordinator's are, 107 of them).
    texts = [f"CP-T{n}.{m} {name}: step {m} of {g}" for n, name, _, g in TRACKS for m in range(1, g + 1)]
    api("POST", f"/api/harness/sessions/{coordinator}/goals", {"goals": texts})
    stack = api("GET", f"/api/harness/sessions/{coordinator}/goals")
    first = {i["text"].split()[0]: i["id"] for i in stack["items"] if i["text"].split()[0].endswith(".1")}
    for n in WORKERS:
        api("PUT", f"/api/programs/{master['id']}/tracks/T{n}", {"agent": workers[n], "goal": first[f"CP-T{n}.1"]})
    p = api("GET", f"/api/programs/{master['id']}")
    workers_in = sum(1 for t in p["tracks"] if t["worker"])
    print(f"seeded: {len(p['tracks'])} tracks, {p['total']} goals, {workers_in} workers, {p['running']} at work; the coordinator's stack holds {len(stack['items'])}")
    print(master["id"])


if __name__ == "__main__":
    main()
