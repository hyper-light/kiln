#!/usr/bin/env python3
"""Goal 8's lab: each configuration's rounds (one run.json a round, runs/<cfg>/<i>/run.json) as one
run.json (runs/<cfg>/run.json), for kiln-lab's table and compare."""
import json, pathlib, sys
runs = pathlib.Path(sys.argv[1])
for cfg in sys.argv[2:]:
    parts = sorted((runs / cfg).glob("*/run.json"), key=lambda p: int(p.parent.name))
    if not parts:
        continue
    whole = json.loads(parts[0].read_text())
    whole["rounds"], whole["failures"] = [], []
    for p in parts:
        r = json.loads(p.read_text())
        whole["rounds"] += r.get("rounds", [])
        whole["failures"] += r.get("failures", [])
    for i, r in enumerate(whole["rounds"]):
        r["index"] = i
    whole["command"] = f"goal 8's lab, {cfg}: {len(whole['rounds'])} rounds (driver.sh)"
    (runs / cfg / "run.json").write_text(json.dumps(whole))
    print(cfg, len(whole["rounds"]), "rounds,", len(whole["failures"]), "failures")
