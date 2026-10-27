#!/usr/bin/env python3
"""Read-only: fetch v1 runs (and their jobs) of a repository from kiln's API into a JSON file.

The admin token comes from `kiln token` and is never printed or written anywhere.
"""
import json
import subprocess
import sys
import time
import urllib.request

BASE = "http://adacachy:7070"
REPO = sys.argv[1] if len(sys.argv) > 1 else "hyper-light/kiln"
DAYS = float(sys.argv[2]) if len(sys.argv) > 2 else 7
OUT = sys.argv[3] if len(sys.argv) > 3 else "/tmp/cires/runs.json"

tok = subprocess.run(["kiln", "token"], capture_output=True, text=True, check=True).stdout.strip().splitlines()[-1].strip()


def get(path):
    req = urllib.request.Request(BASE + path, headers={"Authorization": "Bearer " + tok})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


repos = get("/api/repos")
rid = next((r["id"] for r in repos if r["name"] == REPO), None)
if rid is None:
    sys.exit(f"no repo {REPO}")
since = time.time() - DAYS * 86400
runs, before = [], None
while True:
    q = f"/api/runs?repo={rid}&limit=200" + (f"&before={before}" if before else "")
    page = get(q)
    if not page:
        break
    runs.extend(page)
    oldest = page[-1]
    if oldest["created_at"] < since or len(page) < 200:
        break
    if before == oldest["created_at"]:
        break
    before = oldest["created_at"]
runs = [r for r in runs if r["created_at"] >= since]
detail = []
for r in runs:
    try:
        d = get(f"/api/runs/{r['id']}")
    except Exception as e:  # noqa: BLE001
        d = {"run": r, "jobs": [], "error": str(e)}
    for j in d.get("jobs", []):
        j.pop("spec", None)
    detail.append(d)
json.dump({"repo": REPO, "fetched_at": time.time(), "days": DAYS, "runs": detail}, open(OUT, "w"))
print(f"{len(detail)} runs of {REPO} since {time.strftime('%Y-%m-%d %H:%M', time.localtime(since))} -> {OUT}")
