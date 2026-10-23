# W21-AC03's runs (docs/ci.md), on signin_setup.py's server: two repositories kiln hosts, each
# with a workflow that makes a 1,000-leg run no runner takes (the server has none), so each run
# is there whole, queued, for the graph bench (graph_bench.mjs):
# - acme/graph-wide: four jobs, each a 250-leg matrix, and two after them;
# - acme/graph-flat: 200 jobs of five legs each (a job's legs are made once what it needs is done,
#   so a run no runner takes has the legs of the jobs that need nothing).
# Prints {"base", "admin", "runs": {"wide": id, "flat": id}}.
#
#     SIGNIN="$(cat <signin_setup.py's output>)" python3 -I ci/ui/graph_setup.py
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

cfg = json.loads(os.environ["SIGNIN"])
base, admin = cfg["base"], cfg["admin"]


def call(method, path, body=None):
    req = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(), headers={"Authorization": f"Bearer {admin}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read() or b"null")


def wide():
    jobs = []
    for name in ["build-a", "build-b", "build-c", "build-d"]:
        jobs.append(f"  {name}:\n    runs-on: nowhere\n    strategy:\n      matrix:\n        x: [{', '.join(str(i) for i in range(25))}]\n        y: [{', '.join(str(i) for i in range(10))}]\n    steps:\n      - run: 'true'\n")
    jobs.append("  merge:\n    needs: [build-a, build-b, build-c, build-d]\n    runs-on: nowhere\n    steps:\n      - run: 'true'\n")
    jobs.append("  ship:\n    needs: merge\n    runs-on: nowhere\n    steps:\n      - run: 'true'\n")
    return "on: push\njobs:\n" + "".join(jobs)


def flat():
    jobs = [f"  j{i}:\n    runs-on: nowhere\n    strategy:\n      matrix:\n        n: [1, 2, 3, 4, 5]\n    steps:\n      - run: 'true'\n" for i in range(200)]
    return "on: push\njobs:\n" + "".join(jobs)


def git(src, *args):
    genv = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="lead", GIT_AUTHOR_EMAIL="lead@example.invalid", GIT_COMMITTER_NAME="lead", GIT_COMMITTER_EMAIL="lead@example.invalid")
    subprocess.run(["git", "-C", src, *args], env=genv, check=True, capture_output=True)


runs = {}
for kind, text in [("wide", wide()), ("flat", flat())]:
    repo = f"acme/graph-{kind}"
    if not any(r["name"] == repo for r in call("GET", "/api/repos")):
        call("POST", "/api/repos", {"name": repo, "mode": "native"})
    src = tempfile.mkdtemp(prefix=f"graph-{kind}-")
    url = f"http://x:{admin}@{base.split('://', 1)[1]}/git/{repo}.git"
    # kiln takes what a repository has on its first look, and runs what comes after: a first
    # commit, looked at, then the workflow.
    git(src, "init", "-q", "-b", "main")
    open(os.path.join(src, "README.md"), "w").write("graph\n")
    git(src, "add", ".")
    git(src, "commit", "-qm", "first")
    git(src, "push", "-q", "--force", url, "main")
    seen = time.time() + 60
    while not any(r["name"] == repo and r.get("last_polled_at") for r in call("GET", "/api/repos")):
        if time.time() > seen:
            sys.exit(f"{repo} was never looked at")
        time.sleep(0.5)
    time.sleep(2)
    os.makedirs(os.path.join(src, ".github", "workflows"))
    open(os.path.join(src, ".github", "workflows", "big.yml"), "w").write(text)
    git(src, "add", ".")
    git(src, "commit", "-qm", "1,000 legs")
    git(src, "push", "-q", url, "main")
    deadline = time.time() + 180
    while True:
        listed = call("GET", f"/api/ci/runs?repo={repo}&limit=5").get("runs") or []
        if listed:
            detail = call("GET", f"/api/ci/runs/{listed[0]['id']}")
            if len(detail.get("legs") or []) >= 1000:
                runs[kind] = listed[0]["id"]
                break
        if time.time() > deadline:
            sys.exit(f"{repo}: no 1,000-leg run: {listed}")
        time.sleep(1)
print(json.dumps({"base": base, "admin": admin, "runs": runs}))
